//! Legacy RAR 1.5–4.x recovery volumes (`.rev` files).
//!
//! A legacy `.rev` file is raw Reed-Solomon parity over the volume set: for
//! every byte offset, the bytes of all data volumes form one GF(2^8) RS
//! codeword (see [`rs8`]), and each recovery volume stores one parity
//! symbol per offset. Two on-disk layouts exist, both verified byte-for-byte
//! against WinRAR 7.23 (`rv`/`rc`):
//!
//! - **trailer format** (RAR 4.20+, used when the volumes end in zero bytes,
//!   as WinRAR's 20-byte `ENDARC` guarantees): the `.rev` length equals the
//!   largest volume; the last 7 bytes are
//!   `[data_count - 1, recovery_count - 1, recovery_index, CRC32]` over the
//!   preceding bytes plus the first three trailer bytes, and the parity
//!   protects offsets `0..len - 7` only. A rebuilt volume's last 7 bytes
//!   are zeros, exactly like WinRAR. Names: `{base}.part{NN}.rev` for
//!   `.partN.rar` sets, `{base}{N}.rev` for `.rar`/`.rNN` sets.
//! - **legacy format** (RAR 3.0-era volumes without zero tails): the whole
//!   file is parity and the counts live in the file name,
//!   `{base}<data_count>_<recovery_count>_<index + 1>.rev` (new-naming sets
//!   keep their part infix: `{base}.part<data>_<rec>_<index>.rev`).
//!
//! Repair (`rc`) accepts both layouts, locates damaged volumes with the RS
//! syndromes (Berlekamp-Massey, up to `floor(recovery_count / 2)` unknown
//! damaged volumes), renames damaged data volumes to `*.bad` and writes the
//! rebuilt volumes in their place, mirroring WinRAR.
//!
//! The RS codec is ported from `rars`' `recovery/rar3.rs` (MIT OR
//! Apache-2.0 per the rars workspace metadata — see NOTICE); the on-disk
//! layouts were reverse-engineered from WinRAR output.

pub(crate) mod rs8;

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rs8::{MAX_CODEWORD, Rsc8};

use crate::error::{RarError, RarResult};
use crate::fs::volume::{legacy_volume_base, volume_path_rar4};

/// Metadata trailer length of the RAR 4.20+ `.rev` layout.
const TRAILER_LEN: usize = 7;
/// Streaming chunk for parity building and reconstruction.
const CHUNK: usize = 1024 * 1024;
const ENDARC_HEAD: u8 = 0x7b;
const LONG_BLOCK: u16 = 0x8000;

/// Recovery-set metadata: how many data volumes the set has, how many
/// recovery volumes protect it, and which recovery volume a file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub data_count: usize,
    pub rec_count: usize,
    pub recovery_index: usize,
}

impl Meta {
    fn valid(&self) -> bool {
        self.data_count > 0
            && self.rec_count > 0
            && self.recovery_index < self.rec_count
            && self.data_count + self.rec_count <= MAX_CODEWORD
    }
}

/// Which of the two recovery layouts a set uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// Trailer format: parity protects `0..len - 7`; the tail is zeroed.
    Trailer,
    /// Legacy format: full-file parity, metadata in the name.
    Legacy,
}

/// Parse the 7-byte trailer of a trailer-format `.rev` file.
fn parse_trailer(bytes: &[u8]) -> Option<Meta> {
    if bytes.len() < TRAILER_LEN {
        return None;
    }
    let tail = &bytes[bytes.len() - TRAILER_LEN..];
    let stored = u32::from_le_bytes(tail[3..7].try_into().ok()?);
    if crc32fast::hash(&bytes[..bytes.len() - 4]) != stored {
        return None;
    }
    let meta = Meta {
        data_count: usize::from(tail[0]) + 1,
        rec_count: usize::from(tail[1]) + 1,
        recovery_index: usize::from(tail[2]),
    };
    meta.valid().then_some(meta)
}

/// Append the 7-byte trailer for `payload` to `out`.
fn write_trailer(meta: &Meta, payload: &[u8], out: &mut Vec<u8>) {
    let head = [
        (meta.data_count - 1) as u8,
        (meta.rec_count - 1) as u8,
        meta.recovery_index as u8,
    ];
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(payload);
    hasher.update(&head);
    out.extend_from_slice(&head);
    out.extend_from_slice(&hasher.finalize().to_le_bytes());
}

/// The four on-disk `.rev` name shapes this module understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NameKind {
    /// `{base}.part{NN}.rev` (trailer format, `.partN.rar` sets).
    NewTrailer,
    /// `{base}{N}.rev` (trailer format, `.rar`/`.rNN` sets).
    OldTrailer,
    /// `{base}<data>_<rec>_<index>.rev` (legacy format, `.rar`/`.rNN` sets).
    OldLegacy,
    /// `{base}.part<data>_<rec>_<index>.rev` (legacy format, `.partN.rar` sets).
    NewLegacy,
}

impl NameKind {
    fn format(self) -> Format {
        match self {
            NameKind::NewTrailer | NameKind::OldTrailer => Format::Trailer,
            NameKind::OldLegacy | NameKind::NewLegacy => Format::Legacy,
        }
    }
}

/// A candidate parse of a `.rev` file name. Names are ambiguous when the
/// volume base itself ends in digits (`mv4` + `4_1_1` reads as `mv` +
/// `44_1_1`), so every split is a candidate and callers pick the one whose
/// data volumes actually exist.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RevName {
    base: String,
    new_naming: bool,
    /// Zero-padding width of the part number for new-naming sets (0 = the
    /// name did not carry one).
    width: usize,
    kind: NameKind,
    /// Metadata encoded in a legacy name.
    meta: Option<Meta>,
}

/// Split trailing decimal groups off a stem. Groups are returned
/// right-to-left (closest to the end first) so `x4_2_1` yields `[1, 2, 4]`.
fn trailing_groups(stem: &str, count: usize) -> Option<(Vec<usize>, usize)> {
    let bytes = stem.as_bytes();
    let mut cursor = bytes.len();
    let mut groups = Vec::with_capacity(count);
    while groups.len() < count {
        while cursor > 0 && !bytes[cursor - 1].is_ascii_digit() {
            cursor -= 1;
        }
        if cursor == 0 {
            return None;
        }
        let end = cursor;
        while cursor > 0 && bytes[cursor - 1].is_ascii_digit() {
            cursor -= 1;
        }
        groups.push(stem[cursor..end].parse::<usize>().ok()?);
    }
    Some((groups, cursor))
}

/// Strip a trailing part infix from a legacy rev-name base
/// (`rev_oldstyle.part` → (`rev_oldstyle`, true)).
fn strip_part_infix(base: &str) -> (&str, bool) {
    if let Some(stripped) = base.strip_suffix(".part")
        && !stripped.is_empty()
    {
        return (stripped, true);
    }
    (base, false)
}

/// Every plausible parse of a `.rev` file name, most specific first.
fn rev_name_candidates(name: &str) -> Vec<RevName> {
    let Some(stem) = name
        .strip_suffix(".rev")
        .or_else(|| name.strip_suffix(".REV"))
    else {
        return Vec::new();
    };
    let mut candidates = Vec::new();

    // New-naming trailer format: a `.partNN` infix.
    if let Some((base, infix)) = stem.rsplit_once(".part")
        && !base.is_empty()
        && !infix.is_empty()
        && infix.bytes().all(|b| b.is_ascii_digit())
    {
        candidates.push(RevName {
            base: base.to_string(),
            new_naming: true,
            width: infix.len(),
            kind: NameKind::NewTrailer,
            meta: None,
        });
    }

    // Legacy format: three trailing groups, the third of which may absorb
    // trailing digits of the base itself.
    if let Some((groups, cursor)) = trailing_groups(stem, 3) {
        let [index, rec_count, merged] = [groups[0], groups[1], groups[2]];
        let prefix = &stem[..cursor];
        for digits in 1..=merged.to_string().len() {
            let Some(divisor) = 10usize.checked_pow(digits as u32) else {
                break;
            };
            let data_count = merged % divisor;
            let base_digits = merged / divisor;
            if data_count == 0 || rec_count == 0 || index == 0 || index > rec_count {
                continue;
            }
            if data_count + rec_count > MAX_CODEWORD {
                continue;
            }
            let full_base = if base_digits == 0 {
                prefix.to_string()
            } else {
                format!("{prefix}{base_digits}")
            };
            let (base, new_naming) = strip_part_infix(&full_base);
            if base.is_empty() {
                continue;
            }
            candidates.push(RevName {
                base: base.to_string(),
                new_naming,
                width: 0,
                kind: if new_naming {
                    NameKind::NewLegacy
                } else {
                    NameKind::OldLegacy
                },
                meta: Some(Meta {
                    data_count,
                    rec_count,
                    recovery_index: index - 1,
                }),
            });
        }
    }

    // Old-naming trailer format: a single trailing group.
    if let Some((groups, cursor)) = trailing_groups(stem, 1) {
        let base = &stem[..cursor];
        if !base.is_empty() && groups[0] != 0 {
            candidates.push(RevName {
                base: base.to_string(),
                new_naming: false,
                width: 0,
                kind: NameKind::OldTrailer,
                meta: None,
            });
        }
    }
    candidates
}

/// Data and recovery volume paths for one naming family.
#[derive(Clone, Debug)]
struct Layout {
    base: String,
    new_naming: bool,
    /// Part-number padding used by `.partN.rar` sets (1 = `part1.rar`,
    /// 2 = `part01.rar`); ignored for old-naming sets.
    width: usize,
}

impl Layout {
    /// Trailer-format `.rev` path (`k` is the zero-based recovery index).
    fn trailer_rev_path(&self, parent: &Path, k: usize) -> PathBuf {
        if self.new_naming {
            let width = self.width.max(1);
            parent.join(format!(
                "{}.part{:0width$}.rev",
                self.base,
                k + 1,
                width = width
            ))
        } else {
            parent.join(format!("{}{}.rev", self.base, k + 1))
        }
    }

    /// Legacy-format `.rev` path (`k` is the zero-based recovery index).
    fn legacy_rev_path(&self, parent: &Path, k: usize, meta: &Meta) -> PathBuf {
        let stem = if self.new_naming {
            format!(
                "{}.part{}_{}_{}",
                self.base,
                meta.data_count,
                meta.rec_count,
                k + 1
            )
        } else {
            format!(
                "{}{}_{}_{}",
                self.base,
                meta.data_count,
                meta.rec_count,
                k + 1
            )
        };
        parent.join(format!("{stem}.rev"))
    }
}

/// First existing `.partN.rar` probe for a slot (padded or unpadded).
fn new_data_path(parent: &Path, base: &str, width: usize, index: usize) -> Option<PathBuf> {
    let widths: Vec<usize> = if width > 0 {
        vec![width, 1, 2, 3, 4]
    } else {
        vec![1, 2, 3, 4]
    };
    widths
        .into_iter()
        .map(|width| {
            parent.join(format!(
                "{}.part{:0width$}.rar",
                base,
                index + 1,
                width = width
            ))
        })
        .find(|path| path.exists())
}

/// Whether data volume `index` of `layout` exists under `parent`.
fn slot_exists(parent: &Path, layout: &Layout, index: usize) -> bool {
    if layout.new_naming {
        return new_data_path(parent, &layout.base, layout.width, index).is_some();
    }
    volume_path_rar4(parent, &layout.base, index + 1).exists()
}

/// Everything `rc` needs about a recovery set: the naming layout, the
/// volume counts, the on-disk layout and the recovery payloads (sorted by
/// recovery index).
struct RecoverySet {
    layout: Layout,
    meta: Meta,
    format: Format,
    payloads: Vec<(usize, Vec<u8>)>,
}

/// Locate every `.rev` file belonging to `base` under `parent`, in any of
/// the four name shapes.
///
/// Trailer-format files have their trailer bytes zeroed (those seven
/// offsets carry no parity, matching WinRAR); legacy files contribute
/// their full bytes.
fn collect_recovery_volumes(parent: &Path, base: &str) -> RarResult<RecoverySet> {
    let mut layout: Option<Layout> = None;
    let mut meta: Option<Meta> = None;
    let mut format: Option<Format> = None;
    let mut payloads: Vec<(usize, Vec<u8>)> = Vec::new();

    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    for entry in fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let bytes = fs::read(&path)?;
        let trailer = parse_trailer(&bytes);

        // The name is ambiguous when the base ends in digits; keep the
        // candidate whose data volumes actually exist.
        let mut best: Option<(usize, RevName, Meta)> = None;
        for candidate in rev_name_candidates(name) {
            if candidate.base != base {
                continue;
            }
            let file_meta = match candidate.kind.format() {
                Format::Trailer => match trailer {
                    Some(meta) => meta,
                    None => continue,
                },
                Format::Legacy => match candidate.meta {
                    Some(meta) => meta,
                    None => continue,
                },
            };
            if file_meta.data_count + file_meta.rec_count > MAX_CODEWORD {
                continue;
            }
            let probe = Layout {
                base: candidate.base.clone(),
                new_naming: candidate.new_naming,
                width: candidate.width,
            };
            let score = (0..file_meta.data_count)
                .filter(|index| slot_exists(parent, &probe, *index))
                .count();
            if best
                .as_ref()
                .is_none_or(|(best_score, _, _)| score > *best_score)
            {
                best = Some((score, candidate, file_meta));
            }
        }
        let Some((_, parsed, file_meta)) = best else {
            continue;
        };

        if let Some(existing) = &meta
            && (existing.data_count != file_meta.data_count
                || existing.rec_count != file_meta.rec_count)
        {
            return Err(RarError::Format(
                "recovery volume metadata differs across files".into(),
            ));
        }
        if let Some(existing) = format
            && existing != parsed.kind.format()
        {
            return Err(RarError::Format(
                "recovery volumes mix both legacy layouts".into(),
            ));
        }
        let payload = match parsed.kind.format() {
            Format::Trailer => {
                let mut payload = bytes;
                let len = payload.len();
                payload[len - TRAILER_LEN..].fill(0);
                payload
            }
            Format::Legacy => bytes,
        };
        meta = Some(file_meta);
        format = Some(parsed.kind.format());
        layout.get_or_insert(Layout {
            base: parsed.base,
            new_naming: parsed.new_naming,
            width: parsed.width,
        });
        payloads.push((file_meta.recovery_index, payload));
    }

    let Some(meta) = meta else {
        return Err(RarError::Format(format!(
            "{}: no recovery volumes found",
            parent.join(base).display()
        )));
    };
    let Some(layout) = layout else {
        return Err(RarError::Format(
            "no recovery volumes found for the volume set".into(),
        ));
    };
    let Some(format) = format else {
        return Err(RarError::Format(
            "no recovery volumes found for the volume set".into(),
        ));
    };
    payloads.sort_by_key(|(index, _)| *index);
    let mut seen = vec![false; meta.rec_count];
    for (index, _) in &payloads {
        if *index >= meta.rec_count || std::mem::replace(&mut seen[*index], true) {
            return Err(RarError::Format(
                "duplicate or out-of-range recovery volume".into(),
            ));
        }
    }
    Ok(RecoverySet {
        layout,
        meta,
        format,
        payloads,
    })
}

/// Recognise which naming family a data-volume or `.rev` path belongs to.
fn identify(path: &Path) -> RarResult<(PathBuf, Layout, Option<Meta>)> {
    let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| RarError::Format(format!("{}: not a volume path", path.display())))?;

    let candidates = rev_name_candidates(name);
    if !candidates.is_empty() {
        // Ambiguous names: prefer the split whose data volumes exist.
        let bytes = fs::read(path).ok();
        let trailer = bytes.as_deref().and_then(parse_trailer);
        let mut best: Option<(usize, RevName, Option<Meta>)> = None;
        for candidate in candidates {
            let probe = Layout {
                base: candidate.base.clone(),
                new_naming: candidate.new_naming,
                width: candidate.width,
            };
            let file_meta = match candidate.kind.format() {
                Format::Trailer => trailer,
                Format::Legacy => candidate.meta,
            };
            let Some(file_meta) = file_meta else {
                if best.is_none() {
                    best = Some((0, candidate, None));
                }
                continue;
            };
            let score = (0..file_meta.data_count)
                .filter(|index| slot_exists(&parent, &probe, *index))
                .count();
            if best
                .as_ref()
                .is_none_or(|(best_score, _, _)| score > *best_score)
            {
                best = Some((score, candidate, Some(file_meta)));
            }
        }
        if let Some((_, parsed, file_meta)) = best {
            return Ok((
                parent,
                Layout {
                    base: parsed.base,
                    new_naming: parsed.new_naming,
                    width: parsed.width,
                },
                file_meta,
            ));
        }
    }

    if let Some((base, width)) = crate::fs::volume::extract_volume_base(name) {
        return Ok((
            parent,
            Layout {
                base,
                new_naming: true,
                width,
            },
            None,
        ));
    }

    if let Some(base) = legacy_volume_base(name) {
        return Ok((
            parent,
            Layout {
                base,
                new_naming: false,
                width: 1,
            },
            None,
        ));
    }

    Err(RarError::Format(format!(
        "{}: not a volume or recovery-volume name",
        path.display()
    )))
}

/// Whether a path (a data volume or any `.rev`) belongs to a legacy RAR
/// recovery set and should be dispatched to this module.
pub(crate) fn is_legacy_rev_set(path: &Path) -> RarResult<bool> {
    let mut file = fs::File::open(path)?;
    let mut head = [0u8; 8];
    let read = file.read(&mut head)?;
    if read >= 7 && head[..7] == *crate::detect::RAR4_SIGNATURE {
        return Ok(true);
    }
    if read >= 8 && head == *crate::format::rar5::RAR5_SIGNATURE {
        return Ok(false);
    }
    if read >= 7 && head[..7] == *crate::recovery::rev50::REV5_SIGNATURE {
        return Ok(false);
    }
    let is_rev = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rev"));
    // Let the legacy path try (and report a precise error if it is not
    // ours); REV5 files were already excluded by their signature.
    Ok(is_rev)
}

/// True when the volume's last seven bytes are zero, i.e. the trailer
/// layout loses nothing on rebuild (WinRAR's choice too).
fn use_trailer_format(volume_paths: &[PathBuf], sizes: &[u64]) -> RarResult<bool> {
    let mut probe = true;
    for (path, size) in volume_paths.iter().zip(sizes) {
        if *size < TRAILER_LEN as u64 {
            return Ok(false);
        }
        let mut file = fs::File::open(path)?;
        file.seek(SeekFrom::Start(size - TRAILER_LEN as u64))?;
        let mut tail = [0u8; TRAILER_LEN];
        file.read_exact(&mut tail)?;
        if tail.iter().any(|&byte| byte != 0) {
            probe = false;
            break;
        }
    }
    Ok(probe)
}

/// Build `.rev` recovery volumes for an existing RAR 1.5–4.x volume set,
/// matching WinRAR's `rv` output byte-for-byte.
pub(crate) fn build_recovery_volumes_for_set(
    volume_paths: &[PathBuf],
    rec_count: usize,
) -> RarResult<Vec<PathBuf>> {
    let nd = volume_paths.len();
    if nd < 2 {
        return Err(RarError::InvalidOption(
            "recovery volumes require a multi-volume archive".into(),
        ));
    }
    if nd + rec_count > MAX_CODEWORD {
        return Err(RarError::InvalidOption(format!(
            "legacy recovery volumes support at most {MAX_CODEWORD} data + recovery volumes"
        )));
    }
    let rec_count = rec_count.max(1);

    let parent = volume_paths[0]
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let (_, layout, _) = identify(&volume_paths[0])?;

    let mut sizes = Vec::with_capacity(nd);
    for path in volume_paths {
        sizes.push(fs::metadata(path)?.len());
    }
    let shard_len = *sizes.iter().max().unwrap_or(&0);
    if shard_len < (TRAILER_LEN + 1) as u64 {
        return Err(RarError::Format(
            "volumes are too small for recovery volumes".into(),
        ));
    }
    let format = if use_trailer_format(volume_paths, &sizes)? {
        Format::Trailer
    } else {
        Format::Legacy
    };
    let protected = match format {
        Format::Trailer => shard_len - TRAILER_LEN as u64,
        Format::Legacy => shard_len,
    };

    let coder = Rsc8::new(rec_count).map_err(map_coder)?;
    let mut readers = Vec::with_capacity(nd);
    for path in volume_paths {
        readers.push(fs::File::open(path)?);
    }
    let mut payloads: Vec<Vec<u8>> = vec![Vec::new(); rec_count];

    let mut offset = 0u64;
    while offset < protected {
        let want = (protected - offset).min(CHUNK as u64) as usize;
        let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(nd);
        for (index, reader) in readers.iter_mut().enumerate() {
            let mut chunk = vec![0u8; want];
            if offset < sizes[index] {
                let to_read = (sizes[index] - offset).min(want as u64) as usize;
                reader.seek(SeekFrom::Start(offset))?;
                reader.read_exact(&mut chunk[..to_read])?;
            }
            chunks.push(chunk);
        }
        let mut column = vec![0u8; nd];
        for position in 0..want {
            for (index, chunk) in chunks.iter().enumerate() {
                column[index] = chunk[position];
            }
            let encoded = coder.encode(&column);
            for (index, byte) in encoded.into_iter().enumerate() {
                payloads[index].push(byte);
            }
        }
        offset += want as u64;
    }

    let mut written = Vec::with_capacity(rec_count);
    for (k, payload) in payloads.iter().enumerate() {
        let meta = Meta {
            data_count: nd,
            rec_count,
            recovery_index: k,
        };
        let path = match format {
            Format::Trailer => layout.trailer_rev_path(&parent, k),
            Format::Legacy => layout.legacy_rev_path(&parent, k, &meta),
        };
        let mut file = Vec::with_capacity(payload.len() + TRAILER_LEN);
        file.extend_from_slice(payload);
        if format == Format::Trailer {
            write_trailer(&meta, payload, &mut file);
        }
        fs::write(&path, &file)?;
        written.push(path);
    }
    Ok(written)
}

/// Resolve the data-volume path of every slot, preferring the naming
/// family with the most existing files (falling back to the family the
/// recovery-volume name implies) and returning `(paths, missing_indices)`.
fn resolve_data_slots(
    parent: &Path,
    layout: &Layout,
    data_count: usize,
) -> (Vec<PathBuf>, Vec<usize>) {
    let fallback_new =
        |index: usize| -> PathBuf { parent.join(format!("{}.part{}.rar", layout.base, index + 1)) };
    let old_path = |index: usize| volume_path_rar4(parent, &layout.base, index + 1);

    let mut slots: Vec<Option<PathBuf>> = Vec::with_capacity(data_count);
    let mut new_hits = 0usize;
    let mut old_hits = 0usize;
    for index in 0..data_count {
        let mut found = new_data_path(parent, &layout.base, layout.width, index);
        if found.is_some() {
            new_hits += 1;
        } else {
            let legacy = old_path(index);
            if legacy.exists() {
                found = Some(legacy);
                old_hits += 1;
            }
        }
        slots.push(found);
    }

    let prefer_new = if new_hits != old_hits {
        new_hits > old_hits
    } else {
        layout.new_naming
    };
    let mut missing = Vec::new();
    let mut paths = Vec::with_capacity(data_count);
    for (index, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(path) => paths.push(path),
            None => {
                missing.push(index);
                paths.push(if prefer_new {
                    fallback_new(index)
                } else {
                    old_path(index)
                });
            }
        }
    }
    (paths, missing)
}

/// Rebuild missing and damaged volumes of a RAR 1.5–4.x set from its
/// `.rev` files (like WinRAR's `rc`). `path` may be any existing data
/// volume or any `.rev` of the set.
pub(crate) fn rebuild_missing_volumes(
    path: &Path,
    cancel: Option<&AtomicBool>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<Vec<PathBuf>> {
    let check_cancel = |cancel: Option<&AtomicBool>| -> RarResult<()> {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        Ok(())
    };
    check_cancel(cancel)?;

    let (parent, layout, _) = identify(path)?;
    let RecoverySet {
        layout: set_layout,
        meta,
        format,
        payloads: revs,
    } = collect_recovery_volumes(&parent, &layout.base)?;
    if revs.is_empty() {
        return Err(RarError::Format("no recovery volumes found".into()));
    }
    let shard_len = revs
        .iter()
        .map(|(_, payload)| payload.len() as u64)
        .max()
        .unwrap_or(0);
    if shard_len == 0 {
        return Err(RarError::Format("empty recovery volume".into()));
    }
    let protected = match format {
        Format::Trailer => shard_len.saturating_sub(TRAILER_LEN as u64),
        Format::Legacy => shard_len,
    };

    let (data_paths, missing) = resolve_data_slots(&parent, &set_layout, meta.data_count);
    let sizes: Vec<u64> = data_paths
        .iter()
        .map(|path| fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        .collect();

    let missing_recovery: Vec<usize> = (0..meta.rec_count)
        .filter(|index| !revs.iter().any(|(k, _)| k == index))
        .collect();
    let mut erasures: Vec<usize> = missing.clone();
    erasures.extend(missing_recovery.iter().map(|k| meta.data_count + k));
    erasures.sort_unstable();
    if erasures.len() > meta.rec_count {
        return Err(RarError::Format(format!(
            "{} volume(s) missing but only {} recovery volume(s) available",
            erasures.len(),
            meta.rec_count
        )));
    }

    let coder = Rsc8::new(meta.rec_count).map_err(map_coder)?;
    let codeword_len = meta.data_count + meta.rec_count;

    // Damage pass (WinRAR's "calculating checksums"): every protected
    // offset is checked; a whole corrupted volume shows up as a constant
    // set of error positions, located once and then treated as an erasure.
    let mut damaged: Vec<usize> = Vec::new();
    while let Some(positions) = locate_damage(
        &data_paths,
        &sizes,
        &missing,
        &revs,
        &meta,
        &coder,
        protected,
        &erasures,
        cancel,
    )? {
        let mut added = false;
        for position in positions {
            if !erasures.contains(&position) {
                // Damaged volumes are corrected as erasures (data and
                // recovery symbols alike).
                erasures.push(position);
                added = true;
            }
            if position < meta.data_count && !damaged.contains(&position) {
                damaged.push(position);
            }
        }
        erasures.sort_unstable();
        erasures.dedup();
        damaged.sort_unstable();
        if !added {
            return Err(RarError::Format(
                "recovery volumes cannot repair this damage".into(),
            ));
        }
        if erasures.len() > meta.rec_count {
            return Err(RarError::Format(
                "too many damaged or missing volumes for the recovery data".into(),
            ));
        }
    }

    let mut rebuild_indices: Vec<usize> = missing.clone();
    rebuild_indices.extend(damaged.iter().copied());
    rebuild_indices.sort_unstable();
    rebuild_indices.dedup();
    if rebuild_indices.is_empty() {
        return Ok(Vec::new());
    }

    // Stream the protected range, correcting every offset. Rebuilt volumes
    // go to temporary siblings first; the last volume is truncated at its
    // `ENDARC` block afterwards.
    let mut outputs: Vec<(usize, PathBuf, fs::File)> = Vec::new();
    for &index in &rebuild_indices {
        let final_path = data_paths[index].clone();
        let tmp = crate::fs::atomic::temp_sibling_path(&final_path);
        let file = crate::fs::atomic::read_write_create(&tmp)?;
        outputs.push((index, tmp, file));
    }

    let result = (|| -> RarResult<()> {
        let mut offset = 0u64;
        while offset < shard_len {
            check_cancel(cancel)?;
            if let Some(report) = progress.as_deref_mut() {
                report(offset, shard_len);
            }
            let want = (shard_len - offset).min(CHUNK as u64) as usize;
            let columns = load_chunk(
                &data_paths,
                &sizes,
                &missing,
                &revs,
                meta.rec_count,
                offset,
                want,
            )?;
            let mut codeword = vec![0u8; codeword_len];
            let mut rebuilt: Vec<Vec<u8>> = vec![Vec::with_capacity(want); rebuild_indices.len()];
            for position in 0..want {
                columns.codeword(position, &meta, &mut codeword);
                coder
                    .correct_erasures(&mut codeword, &erasures)
                    .map_err(map_coder)?;
                for (slot, &index) in rebuild_indices.iter().enumerate() {
                    let absolute = offset + position as u64;
                    let byte = if format == Format::Trailer && absolute >= protected {
                        // The trailer layout does not protect the tail; a
                        // rebuilt volume gets zeros there, like WinRAR.
                        0
                    } else {
                        codeword[index]
                    };
                    rebuilt[slot].push(byte);
                }
            }
            for (slot, (_, _, file)) in outputs.iter_mut().enumerate() {
                file.write_all(&rebuilt[slot]).map_err(RarError::Io)?;
            }
            offset += want as u64;
            if let Some(report) = progress.as_deref_mut() {
                report(offset.min(shard_len), shard_len);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for (_, tmp, _) in &outputs {
            let _ = fs::remove_file(tmp);
        }
        return Err(error);
    }

    let mut rebuilt = Vec::with_capacity(outputs.len());
    for (index, tmp, file) in outputs {
        file.sync_all().map_err(RarError::Io)?;
        drop(file);
        // Truncate at the `ENDARC` block when everything after it is zero
        // padding (only the last volume of a set can be short).
        if index == meta.data_count - 1 {
            let mut probe = fs::File::open(&tmp)?;
            if let Some(end) = endarc_end(&mut probe)?
                && end > 0
                && end < shard_len
            {
                let file = fs::File::options().write(true).open(&tmp)?;
                file.set_len(end).map_err(RarError::Io)?;
            }
        }
        let final_path = data_paths[index].clone();
        if damaged.contains(&index) && final_path.exists() {
            let bad = unique_bad_path(&final_path);
            fs::rename(&final_path, &bad)?;
        }
        crate::fs::atomic::replace_file(&tmp, &final_path)?;
        rebuilt.push(final_path);
    }
    Ok(rebuilt)
}

/// One chunk of every volume, loaded once per streaming step.
struct ChunkColumns<'a> {
    /// `None` for a missing data volume; other chunks are zero-padded past
    /// their volume's length.
    data: Vec<Option<Vec<u8>>>,
    /// One slice per recovery volume (`None` when absent).
    revs: Vec<Option<&'a [u8]>>,
}

impl ChunkColumns<'_> {
    fn codeword(&self, position: usize, meta: &Meta, codeword: &mut [u8]) {
        codeword.fill(0);
        for (index, chunk) in self.data.iter().enumerate() {
            if let Some(chunk) = chunk {
                codeword[index] = chunk[position];
            }
        }
        for (k, chunk) in self.revs.iter().enumerate() {
            if let Some(chunk) = chunk {
                codeword[meta.data_count + k] = chunk.get(position).copied().unwrap_or(0);
            }
        }
    }
}

/// Load one chunk of every volume for a streaming pass.
fn load_chunk<'a>(
    slot_paths: &[PathBuf],
    sizes: &[u64],
    missing: &[usize],
    revs: &'a [(usize, Vec<u8>)],
    rec_count: usize,
    offset: u64,
    want: usize,
) -> RarResult<ChunkColumns<'a>> {
    let mut data = Vec::with_capacity(slot_paths.len());
    for (index, path) in slot_paths.iter().enumerate() {
        if missing.contains(&index) {
            data.push(None);
            continue;
        }
        let mut chunk = vec![0u8; want];
        if offset < sizes[index] {
            let to_read = (sizes[index] - offset).min(want as u64) as usize;
            let mut file = fs::File::open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut chunk[..to_read])?;
        }
        data.push(Some(chunk));
    }
    let mut rev_chunks: Vec<Option<&[u8]>> = vec![None; rec_count];
    for (k, payload) in revs {
        let start = offset as usize;
        if start < payload.len() {
            let end = (start + want).min(payload.len());
            rev_chunks[*k] = payload.get(start..end);
        }
    }
    Ok(ChunkColumns {
        data,
        revs: rev_chunks,
    })
}

/// Scan every protected offset and return the error positions of the first
/// nonzero syndrome (whole corrupted volumes), or `None` when the set is
/// consistent with its parity.
#[allow(clippy::too_many_arguments)]
fn locate_damage(
    slot_paths: &[PathBuf],
    sizes: &[u64],
    missing: &[usize],
    revs: &[(usize, Vec<u8>)],
    meta: &Meta,
    coder: &Rsc8,
    protected: u64,
    erasures: &[usize],
    cancel: Option<&AtomicBool>,
) -> RarResult<Option<Vec<usize>>> {
    let mut codeword = vec![0u8; meta.data_count + meta.rec_count];
    let mut offset = 0u64;
    while offset < protected {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        let want = (protected - offset).min(CHUNK as u64) as usize;
        let columns = load_chunk(
            slot_paths,
            sizes,
            missing,
            revs,
            meta.rec_count,
            offset,
            want,
        )?;
        for position in 0..want {
            columns.codeword(position, meta, &mut codeword);
            if !erasures.is_empty() {
                coder
                    .correct_erasures(&mut codeword, erasures)
                    .map_err(map_coder)?;
            }
            let syndromes = coder.syndromes(&codeword);
            if syndromes.iter().any(|&value| value != 0) {
                let positions = coder
                    .locate_errors(&syndromes, codeword.len())
                    .map_err(map_coder)?;
                return Ok(Some(positions));
            }
        }
        offset += want as u64;
    }
    Ok(None)
}

/// Offset just past the `ENDARC` block when the bytes after it are all
/// zero, or `None` when the volume carries no parseable end block.
fn endarc_end(file: &mut fs::File) -> RarResult<Option<u64>> {
    let len = file.metadata()?.len();
    if len < 7 {
        return Ok(None);
    }
    let mut head = [0u8; 7];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut head)?;
    if head != *crate::detect::RAR4_SIGNATURE {
        return Ok(None);
    }
    let mut position = 7u64;
    while position + 7 <= len {
        file.seek(SeekFrom::Start(position))?;
        let mut base = [0u8; 7];
        file.read_exact(&mut base)?;
        let head_type = base[2];
        let flags = u16::from_le_bytes([base[3], base[4]]);
        let head_size = usize::from(u16::from_le_bytes([base[5], base[6]]));
        let header_len = if flags & LONG_BLOCK != 0 { 11usize } else { 7 };
        if head_size < header_len {
            return Ok(None);
        }
        let add_size = if flags & LONG_BLOCK != 0 {
            let mut add = [0u8; 4];
            file.read_exact(&mut add)?;
            u32::from_le_bytes(add) as u64
        } else {
            0
        };
        let total = head_size as u64 + add_size;
        if total == 0 {
            return Ok(None);
        }
        if head_type == ENDARC_HEAD {
            let end = position + total;
            if end > len {
                return Ok(None);
            }
            file.seek(SeekFrom::Start(end))?;
            let mut tail_len = len - end;
            let mut tail = [0u8; 64 * 1024];
            while tail_len > 0 {
                let want = tail_len.min(tail.len() as u64) as usize;
                file.read_exact(&mut tail[..want])?;
                if tail[..want].iter().any(|&byte| byte != 0) {
                    return Ok(None);
                }
                tail_len -= want as u64;
            }
            return Ok(Some(end));
        }
        position += total;
    }
    Ok(None)
}

/// First free `*.bad` sibling for a damaged volume (`x.r00` → `x.r00.bad`).
fn unique_bad_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!("{ext}.bad"))
        .unwrap_or_else(|| "bad".to_string());
    let mut candidate = path.with_extension(extension);
    let mut counter = 2usize;
    while candidate.exists() {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}.bad{counter}"))
            .unwrap_or_else(|| format!("bad{counter}"));
        candidate = path.with_extension(extension);
        counter += 1;
    }
    candidate
}

fn map_coder(error: rs8::Rs8Error) -> RarError {
    RarError::Format(format!("legacy recovery: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{Meta, NameKind, parse_trailer, rev_name_candidates, write_trailer};

    #[test]
    fn trailer_roundtrip_validates_counts_and_crc() {
        let meta = Meta {
            data_count: 4,
            rec_count: 2,
            recovery_index: 1,
        };
        let payload = b"parity payload";
        let mut file = payload.to_vec();
        write_trailer(&meta, payload, &mut file);
        assert_eq!(parse_trailer(&file), Some(meta));
        let mut corrupt = file.clone();
        corrupt[0] ^= 0xff;
        assert_eq!(parse_trailer(&corrupt), None);
    }

    #[test]
    fn rev_names_cover_all_four_shapes() {
        let cases = [
            ("s.part1.rev", NameKind::NewTrailer, "s", true, None),
            ("o1.rev", NameKind::OldTrailer, "o", false, None),
            (
                "o4_2_1.rev",
                NameKind::OldLegacy,
                "o",
                false,
                Some(Meta {
                    data_count: 4,
                    rec_count: 2,
                    recovery_index: 0,
                }),
            ),
            (
                "rev_oldstyle.part4_2_2.rev",
                NameKind::NewLegacy,
                "rev_oldstyle",
                true,
                Some(Meta {
                    data_count: 4,
                    rec_count: 2,
                    recovery_index: 1,
                }),
            ),
        ];
        for (name, kind, base, new_naming, meta) in cases {
            let candidates = rev_name_candidates(name);
            let Some(parsed) = candidates
                .iter()
                .find(|candidate| candidate.base == base)
                .cloned()
            else {
                panic!("{name}: no candidate with base {base}: {candidates:?}");
            };
            assert_eq!(parsed.kind, kind, "{name}");
            assert_eq!(parsed.base, base, "{name}");
            assert_eq!(parsed.new_naming, new_naming, "{name}");
            assert_eq!(parsed.meta, meta, "{name}");
        }
        assert!(rev_name_candidates("plain.rar").is_empty());
        assert!(rev_name_candidates("data.bin").is_empty());
    }

    #[test]
    fn digit_ending_bases_offer_both_splits() {
        // `mv4` + `4_1_1` reads as `mv` + `44_1_1` too; both candidates
        // must be offered so the caller can pick by existing volumes.
        let candidates = rev_name_candidates("mv44_1_1.rev");
        let bases: Vec<&str> = candidates.iter().map(|c| c.base.as_str()).collect();
        assert!(bases.contains(&"mv4"), "{bases:?}");
        assert!(bases.contains(&"mv"), "{bases:?}");
        let data_counts: Vec<usize> = candidates
            .iter()
            .filter_map(|c| c.meta.map(|m| m.data_count))
            .collect();
        assert!(data_counts.contains(&4), "{data_counts:?}");
        assert!(data_counts.contains(&44), "{data_counts:?}");
    }
}

#[test]
fn long_trailing_groups_do_not_overflow() {
    // A 20-digit group used to panic in `10usize.pow(20)` while
    // enumerating the ambiguous splits of a `.rev` name.
    let candidates = rev_name_candidates("mv4_10000000000000000000_1_1.rev");
    assert!(
        candidates
            .iter()
            .all(|candidate| !candidate.base.is_empty())
    );
}
