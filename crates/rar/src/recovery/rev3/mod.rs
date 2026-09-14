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
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rs8::{MAX_CODEWORD, Rsc8};

use crate::error::{RarError, RarResult};
use crate::format::rar4::{ENDARC_HEAD, LONG_BLOCK};
use crate::fs::volume::{legacy_volume_base, volume_part_width, volume_path_rar4};

/// Metadata trailer length of the RAR 4.20+ `.rev` layout.
const TRAILER_LEN: usize = 7;
/// Streaming chunk for parity building and reconstruction.
const CHUNK: usize = 1024 * 1024;

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
#[cfg(test)]
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

/// Streaming [`parse_trailer`]: the CRC covers `bytes[..len - 4]`, so it is
/// verified through a bounded read instead of materializing the file. I/O
/// failures surface as `Err` so callers can decide whether to skip the file.
fn parse_trailer_reader(file: &mut fs::File) -> io::Result<Option<Meta>> {
    let len = file.metadata()?.len();
    if len < TRAILER_LEN as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(len - TRAILER_LEN as u64))?;
    let mut tail = [0u8; TRAILER_LEN];
    file.read_exact(&mut tail)?;
    let stored = u32::from_le_bytes(tail[3..7].try_into().unwrap());
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut remaining = len - 4;
    let mut buf = vec![0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..want])?;
        hasher.update(&buf[..want]);
        remaining -= want as u64;
    }
    if hasher.finalize() != stored {
        return Ok(None);
    }
    let meta = Meta {
        data_count: usize::from(tail[0]) + 1,
        rec_count: usize::from(tail[1]) + 1,
        recovery_index: usize::from(tail[2]),
    };
    Ok(meta.valid().then_some(meta))
}

/// Parse the trailer of the `.rev` file at `path` through a bounded read.
fn parse_trailer_file(path: &Path) -> io::Result<Option<Meta>> {
    let mut file = fs::File::open(path)?;
    parse_trailer_reader(&mut file)
}

/// Append the 7-byte trailer for `payload` to `out`.
#[cfg(test)]
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

/// Candidate part-number paddings for a `.partN.rar` probe: the layout's
/// own width first (when known), then 1..=5 digits. WinRAR pads the part
/// number to the digit count of the volume count; five digits cover the
/// format's 65535-volume maximum, so a fixed 1..=4 scan must not hide
/// `part00001.rar`.
fn part_width_candidates(width: usize) -> Vec<usize> {
    let mut widths = Vec::with_capacity(6);
    if width > 0 {
        widths.push(width);
    }
    for candidate in 1..=5 {
        if !widths.contains(&candidate) {
            widths.push(candidate);
        }
    }
    widths
}

/// First existing `.partN.rar` probe for a slot (padded or unpadded).
fn new_data_path(parent: &Path, base: &str, width: usize, index: usize) -> Option<PathBuf> {
    part_width_candidates(width)
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

/// One `.rev` file's parity source. The bytes stay on disk and are seeked
/// stripe by stripe; for trailer-format files the seven trailer bytes read
/// back as zeros (those offsets carry no parity, matching WinRAR).
#[derive(Clone, Debug)]
struct RevSource {
    index: usize,
    path: PathBuf,
    /// File length (= payload length; the trailer is part of the file but
    /// is not parity).
    len: u64,
}

/// Everything `rc` needs about a recovery set: the naming layout, the
/// volume counts, the on-disk layout and the recovery payload sources
/// (sorted by recovery index).
struct RecoverySet {
    layout: Layout,
    meta: Meta,
    format: Format,
    payloads: Vec<RevSource>,
}

/// Locate every `.rev` file belonging to `base` under `parent`, in any of
/// the four name shapes.
///
/// Trailer-format files have their trailer bytes zeroed (those seven
/// offsets carry no parity, matching WinRAR); legacy files contribute
/// their full bytes. The names are heuristic, so same-base files can
/// describe different (stale) sets: they are grouped by the set they
/// describe and only the best-scoring group is used, while files of other
/// groups are skipped instead of aborting recovery.
fn collect_recovery_volumes(parent: &Path, base: &str) -> RarResult<RecoverySet> {
    /// One same-base `.rev` file resolved to its best name parse.
    struct RevFile {
        path: PathBuf,
        len: u64,
        new_naming: bool,
        width: usize,
        kind: NameKind,
        meta: Meta,
        /// Number of the described set's data volumes that exist on disk.
        score: usize,
    }
    /// Same-base files that describe one recovery set.
    struct RevGroup {
        format: Format,
        data_count: usize,
        rec_count: usize,
        new_naming: bool,
        score: usize,
        members: Vec<usize>,
    }

    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let mut files: Vec<RevFile> = Vec::new();
    for entry in fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Filter by name before reading: unrelated files (and unreadable
        // entries such as directories) must neither be slurped into memory
        // nor abort recovery.
        let candidates = rev_name_candidates(name);
        if !candidates
            .iter()
            .any(|candidate| candidate.base.eq_ignore_ascii_case(base))
        {
            continue;
        }
        // Only trailer-format candidates need the trailer CRC parsed; the
        // trailer is streamed in bounded chunks, not materialized.
        let needs_trailer = candidates.iter().any(|candidate| {
            candidate.base.eq_ignore_ascii_case(base) && candidate.kind.format() == Format::Trailer
        });
        let trailer = if needs_trailer {
            match fs::File::open(&path) {
                Ok(mut file) => parse_trailer_reader(&mut file).ok().flatten(),
                Err(_) => continue,
            }
        } else {
            None
        };
        let Ok(len) = fs::metadata(&path).map(|metadata| metadata.len()) else {
            continue;
        };

        // The name is ambiguous when the base ends in digits; keep the
        // candidate whose data volumes actually exist.
        let mut best: Option<(usize, RevName, Meta)> = None;
        for candidate in candidates {
            if !candidate.base.eq_ignore_ascii_case(base) {
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
        let Some((score, parsed, meta)) = best else {
            continue;
        };
        files.push(RevFile {
            path,
            len,
            new_naming: parsed.new_naming,
            width: parsed.width,
            kind: parsed.kind,
            meta,
            score,
        });
    }

    // Group by the set each file describes; the best-scoring group wins
    // (ties prefer the group with more files, then the first encountered).
    let mut groups: Vec<RevGroup> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let format = file.kind.format();
        if let Some(group) = groups.iter_mut().find(|group| {
            group.format == format
                && group.data_count == file.meta.data_count
                && group.rec_count == file.meta.rec_count
                && group.new_naming == file.new_naming
        }) {
            group.score += file.score;
            group.members.push(index);
        } else {
            groups.push(RevGroup {
                format,
                data_count: file.meta.data_count,
                rec_count: file.meta.rec_count,
                new_naming: file.new_naming,
                score: file.score,
                members: vec![index],
            });
        }
    }
    let mut chosen: Option<RevGroup> = None;
    for group in groups {
        let better = chosen.as_ref().is_none_or(|best| {
            (group.score, group.members.len()) > (best.score, best.members.len())
        });
        if better {
            chosen = Some(group);
        }
    }
    let Some(chosen) = chosen else {
        return Err(RarError::Format(format!(
            "{}: no recovery volumes found",
            parent.join(base).display()
        )));
    };

    // Name rebuilt/repaired volumes after the data set's own base, not the
    // recovery file's casing (they match case-insensitively). The width
    // comes from the best-scoring member when members disagree.
    let mut layout = Layout {
        base: base.to_string(),
        new_naming: chosen.new_naming,
        width: files[chosen.members[0]].width,
    };
    let mut layout_score = files[chosen.members[0]].score;
    for &index in &chosen.members[1..] {
        if files[index].score > layout_score {
            layout_score = files[index].score;
            layout.width = files[index].width;
        }
    }

    let mut payloads = Vec::with_capacity(chosen.members.len());
    for &index in &chosen.members {
        let file = &files[index];
        payloads.push(RevSource {
            index: file.meta.recovery_index,
            path: file.path.clone(),
            len: file.len,
        });
    }

    payloads.sort_by_key(|source| source.index);
    let mut seen = vec![false; chosen.rec_count];
    for source in &payloads {
        if source.index >= chosen.rec_count || std::mem::replace(&mut seen[source.index], true) {
            return Err(RarError::Format(
                "duplicate or out-of-range recovery volume".into(),
            ));
        }
    }
    Ok(RecoverySet {
        layout,
        meta: Meta {
            data_count: chosen.data_count,
            rec_count: chosen.rec_count,
            recovery_index: 0,
        },
        format: chosen.format,
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
        let trailer = parse_trailer_file(path).ok().flatten();
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

/// Whether `name` parses as a RAR 1.5–4.x `.rev` file belonging to the
/// volume set based at `base`. Ambiguous legacy names (`set44_2_1.rev` can
/// read as `set` + data 44 or `set4` + data 4) are resolved with the same
/// data-volume scoring as [`collect_recovery_volumes`], so a stale scan
/// never claims another set's parity files. Matching is ASCII
/// case-insensitive, like the official tools on Windows.
pub(crate) fn rev_name_belongs_to_set(parent: &Path, base: &str, name: &str) -> bool {
    let mut trailer_owned = false;
    let mut best: Option<(usize, String)> = None;
    for candidate in rev_name_candidates(name) {
        match candidate.kind.format() {
            Format::Trailer => {
                if candidate.base.eq_ignore_ascii_case(base) {
                    trailer_owned = true;
                }
            }
            Format::Legacy => {
                let Some(meta) = candidate.meta else {
                    continue;
                };
                let probe = Layout {
                    base: candidate.base.clone(),
                    new_naming: candidate.new_naming,
                    width: candidate.width,
                };
                let score = (0..meta.data_count)
                    .filter(|index| slot_exists(parent, &probe, *index))
                    .count();
                if best
                    .as_ref()
                    .is_none_or(|(best_score, _)| score > *best_score)
                {
                    best = Some((score, candidate.base));
                }
            }
        }
    }
    if trailer_owned {
        return true;
    }
    best.is_some_and(|(_, candidate_base)| candidate_base.eq_ignore_ascii_case(base))
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
    if read >= 8 && head == *crate::recovery::rev50::REV5_SIGNATURE {
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

/// Identify the recovery layout and naming family for a data-volume set: the
/// `.rev` name shape (trailer vs legacy, new vs old naming) follows the
/// sizes' tails, so a final-name computation must use the same inputs as the
/// builder.
fn recovery_name_layout(
    volume_paths: &[PathBuf],
) -> RarResult<(PathBuf, Layout, Format, Vec<u64>)> {
    let parent = volume_paths[0]
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let (_, layout, _) = identify(&volume_paths[0])?;
    let mut sizes = Vec::with_capacity(volume_paths.len());
    for path in volume_paths {
        sizes.push(fs::metadata(path)?.len());
    }
    let format = if use_trailer_format(volume_paths, &sizes)? {
        Format::Trailer
    } else {
        Format::Legacy
    };
    Ok((parent, layout, format, sizes))
}

/// Old-naming trailer-format `.rev` path (`k` is the zero-based recovery
/// index): `{base}{N}.rev`.
fn old_trailer_name(base: &str, k: usize) -> String {
    format!("{base}{}.rev", k + 1)
}

/// Old-naming legacy-format `.rev` path: the counts live in the name,
/// `{base}{data}_{rec}_{index}.rev`.
fn old_legacy_name(base: &str, k: usize, data_count: usize, rec_count: usize) -> String {
    format!("{base}{data_count}_{rec_count}_{}.rev", k + 1)
}

/// The canonical `.rev` names for a set whose data volumes are named
/// `final_base` (+ the legacy `.rar`/`.rNN` suffixes), keeping the name
/// shape the builder chose for the staged recovery files.
///
/// The staged files are named after a temporary base but carry the same
/// layout: a trailer-format `.rev` is recognised by its trailer bytes, a
/// legacy-format one by the counts encoded in its name. Only the base name
/// and, for a `.partN.rar` set, the part padding change at install time, so
/// the names are derived from the staged files themselves rather than
/// re-detecting the layout from the not-yet-installed final volumes.
///
/// `part_width` is the zero-padding the final `.partN.rar` data volumes use
/// (RAR5); `None` keeps the padding of the generated name.
pub(crate) fn canonical_recovery_names(
    final_base: &str,
    staged_revs: &[PathBuf],
    part_width: Option<usize>,
) -> RarResult<Vec<String>> {
    let Some(first) = staged_revs.first() else {
        return Ok(Vec::new());
    };
    let name = first
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| RarError::Format(format!("{}: not a recovery path", first.display())))?;
    let candidates = rev_name_candidates(name);
    let parsed = candidates
        .first()
        .ok_or_else(|| RarError::Format(format!("{name}: not a recovery-volume name")))?;
    // RAR5 recovery volumes (`REV5` signature) use the `.partN.rev` scheme
    // with the data set's padding; the legacy codec's `.rev` layout is only
    // chosen for a real legacy set.
    if is_rar5_recovery_volume(first) {
        let width = part_width.unwrap_or(parsed.width).max(1);
        return Ok((1..=staged_revs.len())
            .map(|k| format!("{}.part{:0width$}.rev", final_base, k))
            .collect());
    }
    // The legacy layout is recognised from the staged file itself: trailer
    // bytes for the trailer layout, the name-encoded counts for the legacy
    // layout.
    let trailer = trailer_style(first);
    let legacy_name = candidates.iter().find_map(|candidate| candidate.meta);
    if !trailer && legacy_name.is_none() {
        return Err(RarError::Format(format!(
            "{name}: legacy recovery name without counts"
        )));
    }
    Ok((0..staged_revs.len())
        .map(|k| {
            if trailer {
                if parsed.new_naming {
                    let width = part_width.unwrap_or(parsed.width).max(1);
                    format!("{}.part{:0width$}.rev", final_base, k + 1)
                } else {
                    old_trailer_name(final_base, k)
                }
            } else {
                let meta = legacy_name.expect("checked above");
                old_legacy_name(final_base, k, meta.data_count, meta.rec_count)
            }
        })
        .collect())
}

/// Whether `path` starts with the RAR5 recovery-volume signature.
fn is_rar5_recovery_volume(path: &Path) -> bool {
    let mut head = [0u8; 8];
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    matches!(file.read(&mut head), Ok(8)) && head == *crate::recovery::rev50::REV5_SIGNATURE
}

/// Whether the staged `.rev` file at `path` uses the trailer layout (its
/// last bytes parse as a valid trailer). The trailer CRC is streamed over
/// the file, so a multi-GB recovery volume is never materialized.
fn trailer_style(path: &Path) -> bool {
    parse_trailer_file(path).ok().flatten().is_some()
}

/// Build `.rev` recovery volumes for an existing RAR 1.5–4.x volume set,
/// matching WinRAR's `rv` output byte-for-byte. Each file is built under a
/// temporary sibling and installed only once the whole set is complete, so
/// a failure leaves existing `.rev` files untouched; the final paths are
/// returned.
pub(crate) fn build_recovery_volumes_for_set(
    volume_paths: &[PathBuf],
    rec_count: usize,
) -> RarResult<Vec<PathBuf>> {
    build_recovery_volumes_for_set_chunked(volume_paths, rec_count, CHUNK)
}

/// [`build_recovery_volumes_for_set`] with an explicit stripe size (tests
/// lower it to exercise multi-stripe runs on small volume sets).
fn build_recovery_volumes_for_set_chunked(
    volume_paths: &[PathBuf],
    rec_count: usize,
    chunk: usize,
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

    let (parent, layout, format, sizes) = recovery_name_layout(volume_paths)?;
    let shard_len = *sizes.iter().max().unwrap_or(&0);
    if shard_len < (TRAILER_LEN + 1) as u64 {
        return Err(RarError::Format(
            "volumes are too small for recovery volumes".into(),
        ));
    }
    let protected = match format {
        Format::Trailer => shard_len - TRAILER_LEN as u64,
        Format::Legacy => shard_len,
    };

    let coder = Rsc8::new(rec_count).map_err(map_coder)?;
    let mut readers = Vec::with_capacity(nd);
    for path in volume_paths {
        readers.push(fs::File::open(path)?);
    }

    // Create the `.rev` files as temporary siblings and fill them stripe
    // by stripe; the trailer (when the layout has one) is appended after
    // the last stripe. The temps are installed over the final paths only
    // after the whole parity set is built, so a failure leaves existing
    // `.rev` files untouched.
    struct RevOutput {
        final_path: PathBuf,
        tmp_path: PathBuf,
        file: fs::File,
        meta: Meta,
        payload_crc: crc32fast::Hasher,
    }
    let mut outputs = Vec::with_capacity(rec_count);
    for k in 0..rec_count {
        let meta = Meta {
            data_count: nd,
            rec_count,
            recovery_index: k,
        };
        let path = match format {
            Format::Trailer => layout.trailer_rev_path(&parent, k),
            Format::Legacy => layout.legacy_rev_path(&parent, k, &meta),
        };
        let tmp_path = crate::fs::atomic::temp_sibling_path(&path);
        match fs::File::create(&tmp_path) {
            Ok(file) => outputs.push(RevOutput {
                final_path: path,
                tmp_path,
                file,
                meta,
                payload_crc: crc32fast::Hasher::new(),
            }),
            Err(error) => {
                for output in &outputs {
                    let _ = fs::remove_file(&output.tmp_path);
                }
                let _ = fs::remove_file(&tmp_path);
                return Err(RarError::Io(error));
            }
        }
    }

    let result = (|| -> RarResult<()> {
        let mut offset = 0u64;
        while offset < protected {
            let want = (protected - offset).min(chunk as u64) as usize;
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
            let mut parity: Vec<Vec<u8>> = vec![vec![0u8; want]; rec_count];
            let mut column = vec![0u8; nd];
            for position in 0..want {
                for (index, chunk) in chunks.iter().enumerate() {
                    column[index] = chunk[position];
                }
                let encoded = coder.encode(&column);
                for (index, byte) in encoded.into_iter().enumerate() {
                    parity[index][position] = byte;
                }
            }
            for (output, bytes) in outputs.iter_mut().zip(&parity) {
                output.payload_crc.update(bytes);
                output.file.write_all(bytes)?;
            }
            offset += want as u64;
        }

        // Trailer layout: the seven trailer bytes carry the counts and a
        // CRC over the payload plus the first three trailer bytes.
        if format == Format::Trailer {
            for output in outputs.iter_mut() {
                let head = [
                    (output.meta.data_count - 1) as u8,
                    (output.meta.rec_count - 1) as u8,
                    output.meta.recovery_index as u8,
                ];
                let mut hasher =
                    std::mem::replace(&mut output.payload_crc, crc32fast::Hasher::new());
                hasher.update(&head);
                output.file.write_all(&head)?;
                output.file.write_all(&hasher.finalize().to_le_bytes())?;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for output in &outputs {
            let _ = fs::remove_file(&output.tmp_path);
        }
        return Err(error);
    }

    // The whole parity set is built: close the staged files and install
    // them as one transaction. `commit_files` parks every pre-existing
    // final, installs the set and rolls the old files back if any install
    // fails, so a failure cannot leave a half-replaced parity set.
    let mut install: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(outputs.len());
    let mut written: Vec<PathBuf> = Vec::with_capacity(outputs.len());
    for output in outputs {
        drop(output.file);
        install.push((output.tmp_path, output.final_path.clone()));
        written.push(output.final_path);
    }
    // A directory (or other non-file) at a final path is a conflict:
    // `commit_files` would park and replace it, then strand the parked
    // entry because only files are dropped on success.
    if let Some(conflict) = install
        .iter()
        .map(|(_, final_path)| final_path)
        .find(|final_path| final_path.exists() && !final_path.is_file())
    {
        for (tmp, _) in &install {
            let _ = fs::remove_file(tmp);
        }
        return Err(RarError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{}: refusing to replace a non-file entry with a recovery volume",
                conflict.display()
            ),
        )));
    }
    if let Err(error) = crate::fs::atomic::commit_files(&parent, &layout.base, &install, &[]) {
        for (tmp, _) in &install {
            let _ = fs::remove_file(tmp);
        }
        return Err(error);
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
    let old_path = |index: usize| volume_path_rar4(parent, &layout.base, index + 1);

    let mut slots: Vec<Option<PathBuf>> = Vec::with_capacity(data_count);
    let mut new_hits = 0usize;
    let mut old_hits = 0usize;
    // A legacy `.rev` name carries no part padding, so recover it from the
    // first existing data volume; reconstructed volumes must keep the
    // set's own padding (`part02.rar`, not `part2.rar`).
    let mut new_width = layout.width;
    for index in 0..data_count {
        let mut found = new_data_path(parent, &layout.base, layout.width, index);
        if let Some(path) = &found {
            new_hits += 1;
            if new_width == 0 {
                new_width = volume_part_width(path);
            }
        } else {
            let legacy = old_path(index);
            if legacy.exists() {
                found = Some(legacy);
                old_hits += 1;
            }
        }
        slots.push(found);
    }
    let fallback_new = |index: usize| -> PathBuf {
        let width = new_width.max(1);
        parent.join(format!(
            "{}.part{:0width$}.rar",
            layout.base,
            index + 1,
            width = width
        ))
    };

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
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<Vec<PathBuf>> {
    rebuild_missing_volumes_chunked(path, cancel, progress, CHUNK)
}

/// [`rebuild_missing_volumes`] with an explicit stripe size (tests lower it
/// to exercise multi-stripe runs on small volume sets).
fn rebuild_missing_volumes_chunked(
    path: &Path,
    cancel: Option<&AtomicBool>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
    chunk: usize,
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
    let shard_len = revs.iter().map(|source| source.len).max().unwrap_or(0);
    if shard_len == 0 {
        return Err(RarError::Format("empty recovery volume".into()));
    }
    let protected = match format {
        Format::Trailer => shard_len.saturating_sub(TRAILER_LEN as u64),
        Format::Legacy => shard_len,
    };

    // Open every recovery volume once; the parity bytes are seeked per
    // stripe. Trailer-format footers read back as zeros.
    let zero_from = (format == Format::Trailer).then_some(protected);
    let mut rev_readers: Vec<(usize, fs::File, u64)> = Vec::with_capacity(revs.len());
    for source in &revs {
        rev_readers.push((source.index, fs::File::open(&source.path)?, source.len));
    }

    let (data_paths, missing) = resolve_data_slots(&parent, &set_layout, meta.data_count);
    let sizes: Vec<u64> = data_paths
        .iter()
        .map(|path| fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        .collect();

    let missing_recovery: Vec<usize> = (0..meta.rec_count)
        .filter(|index| !revs.iter().any(|source| source.index == *index))
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
        &mut rev_readers,
        &meta,
        &coder,
        protected,
        &erasures,
        cancel,
        chunk,
        zero_from,
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
            let want = (shard_len - offset).min(chunk as u64) as usize;
            let columns = load_chunk(
                &data_paths,
                &sizes,
                &missing,
                &mut rev_readers,
                meta.rec_count,
                offset,
                want,
                zero_from,
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

    // Finalize the staged rebuilds (sync + ENDARC truncate), park every
    // damaged original as `*.bad`, then install the whole set as one
    // journaled commit: either every rebuilt volume lands or none does. A
    // failure restores the parks, so a damaged original is never lost.
    commit_rebuilt_volumes(
        &data_paths,
        &damaged,
        outputs,
        meta.data_count - 1,
        shard_len,
    )
}

/// Finalize and install a set of rebuilt volumes as one journaled commit.
///
/// `outputs` holds `(volume index, staged path, write handle)` for each
/// rebuilt volume; a damaged original is parked as `*.bad` and kept there
/// on success. On any failure the parks are renamed back and the staged
/// files are removed on drop.
fn commit_rebuilt_volumes(
    data_paths: &[PathBuf],
    damaged: &[usize],
    outputs: Vec<(usize, PathBuf, fs::File)>,
    last_index: usize,
    shard_len: u64,
) -> RarResult<Vec<PathBuf>> {
    let mut set = crate::fs::atomic::StagedSet::new(
        &crate::fs::atomic::parent_dir(&data_paths[0]),
        &crate::archive::volume_base_of(&data_paths[0]),
    )?;
    let mut parked: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut rebuilt = Vec::with_capacity(outputs.len());
    let staged = (|| -> RarResult<()> {
        for (index, tmp, file) in outputs {
            file.sync_all().map_err(RarError::Io)?;
            drop(file);
            // Truncate at the `ENDARC` block when everything after it is
            // zero padding (only the last volume of a set can be short).
            if index == last_index {
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
                fs::rename(&final_path, &bad).map_err(RarError::Io)?;
                parked.push((bad, final_path.clone()));
            }
            set.track(tmp, &final_path);
            rebuilt.push(final_path);
        }
        Ok(())
    })();
    if let Err(error) = staged {
        restore_parked(&parked);
        return Err(error);
    }
    if let Err(error) = set.commit() {
        restore_parked(&parked);
        return Err(error);
    }
    Ok(rebuilt)
}

/// Put every parked damaged original back at its final path (newest park
/// first), used when the rebuild transaction fails.
fn restore_parked(parked: &[(PathBuf, PathBuf)]) {
    for (bad, original) in parked.iter().rev() {
        let _ = fs::rename(bad, original);
    }
}

/// One chunk of every volume, loaded once per streaming step.
struct ChunkColumns {
    /// `None` for a missing data volume; other chunks are zero-padded past
    /// their volume's length.
    data: Vec<Option<Vec<u8>>>,
    /// One zero-padded chunk per recovery volume (`None` when absent).
    revs: Vec<Option<Vec<u8>>>,
}

impl ChunkColumns {
    fn codeword(&self, position: usize, meta: &Meta, codeword: &mut [u8]) {
        codeword.fill(0);
        for (index, chunk) in self.data.iter().enumerate() {
            if let Some(chunk) = chunk {
                codeword[index] = chunk[position];
            }
        }
        for (k, chunk) in self.revs.iter().enumerate() {
            if let Some(chunk) = chunk {
                codeword[meta.data_count + k] = chunk[position];
            }
        }
    }
}

/// Fill `buf` with the parity bytes at `offset..offset + buf.len()` of a
/// `.rev` file. Bytes past the file (or past `zero_from`, the trailer
/// region of a trailer-format file) read as zero, matching the previous
/// in-memory zero-padded payloads.
fn read_rev_range(
    file: &mut fs::File,
    len: u64,
    zero_from: Option<u64>,
    offset: u64,
    buf: &mut [u8],
) -> RarResult<()> {
    buf.fill(0);
    let end = offset.saturating_add(buf.len() as u64);
    let readable_end = end.min(len).min(zero_from.unwrap_or(u64::MAX));
    if readable_end > offset {
        let n = (readable_end - offset) as usize;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut buf[..n])?;
    }
    Ok(())
}

/// Load one chunk of every volume for a streaming pass.
#[allow(clippy::too_many_arguments)]
fn load_chunk(
    slot_paths: &[PathBuf],
    sizes: &[u64],
    missing: &[usize],
    revs: &mut [(usize, fs::File, u64)],
    rec_count: usize,
    offset: u64,
    want: usize,
    zero_from: Option<u64>,
) -> RarResult<ChunkColumns> {
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
    let mut rev_chunks: Vec<Option<Vec<u8>>> = vec![None; rec_count];
    for (k, file, len) in revs.iter_mut() {
        let mut chunk = vec![0u8; want];
        read_rev_range(file, *len, zero_from, offset, &mut chunk)?;
        rev_chunks[*k] = Some(chunk);
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
    revs: &mut [(usize, fs::File, u64)],
    meta: &Meta,
    coder: &Rsc8,
    protected: u64,
    erasures: &[usize],
    cancel: Option<&AtomicBool>,
    chunk: usize,
    zero_from: Option<u64>,
) -> RarResult<Option<Vec<usize>>> {
    let mut codeword = vec![0u8; meta.data_count + meta.rec_count];
    let mut offset = 0u64;
    while offset < protected {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        let want = (protected - offset).min(chunk as u64) as usize;
        let columns = load_chunk(
            slot_paths,
            sizes,
            missing,
            revs,
            meta.rec_count,
            offset,
            want,
            zero_from,
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
    use super::rs8::Rsc8;
    use super::{
        Meta, NameKind, build_recovery_volumes_for_set_chunked, collect_recovery_volumes,
        commit_rebuilt_volumes, parse_trailer, parse_trailer_file, part_width_candidates,
        rebuild_missing_volumes_chunked, rev_name_candidates, trailer_style, write_trailer,
    };
    use super::{RarError, TRAILER_LEN};
    use std::path::{Path, PathBuf};

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

    /// Scanning a directory with a subdirectory and an unrelated file must
    /// not abort recovery (the subdirectory used to be read and fail).
    #[test]
    fn collect_recovery_volumes_ignores_non_rev_entries() {
        let dir = tempfile::tempdir().unwrap();
        // A legacy-format name carries its metadata, so no data volumes
        // need to exist for collection to succeed.
        std::fs::write(dir.path().join("set4_1_1.rev"), b"parity payload").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("unrelated.bin"), vec![0u8; 4096]).unwrap();

        let set = collect_recovery_volumes(dir.path(), "set").unwrap();
        assert_eq!(set.meta.data_count, 4);
        assert_eq!(set.meta.rec_count, 1);
        assert_eq!(set.payloads.len(), 1);
        assert_eq!(set.payloads[0].len, 14);
        assert_eq!(set.payloads[0].path, dir.path().join("set4_1_1.rev"));
    }

    /// A same-base `.rev` describing a different (stale) set must be
    /// ignored, not abort recovery: the best-scoring metadata group wins.
    #[test]
    fn collect_recovery_volumes_skips_stray_same_base_metadata() {
        let dir = tempfile::tempdir().unwrap();
        // Three data volumes exist; the real set protects all three, the
        // stray same-base file protects only two.
        for index in 1..=3 {
            std::fs::write(
                dir.path().join(format!("set.part{index}.rar")),
                vec![0u8; 64],
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("set.part3_1_1.rev"), b"real parity").unwrap();
        std::fs::write(dir.path().join("set.part2_1_1.rev"), b"stray parity").unwrap();

        let set = collect_recovery_volumes(dir.path(), "set").unwrap();
        assert_eq!(set.meta.data_count, 3);
        assert_eq!(set.meta.rec_count, 1);
        assert_eq!(set.payloads.len(), 1);
        assert_eq!(set.payloads[0].path, dir.path().join("set.part3_1_1.rev"));
    }

    #[test]
    fn part_width_candidates_cover_five_digit_sets() {
        assert_eq!(part_width_candidates(5), vec![5, 1, 2, 3, 4]);
        assert_eq!(part_width_candidates(2), vec![2, 1, 3, 4, 5]);
        assert_eq!(part_width_candidates(0), vec![1, 2, 3, 4, 5]);
    }

    /// `trailer_style` must accept a valid trailer, reject a CRC-corrupted
    /// one, and reject a legacy-format file, all through the file-backed
    /// streaming parser.
    #[test]
    fn trailer_style_detects_trailer_and_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let meta = Meta {
            data_count: 4,
            rec_count: 2,
            recovery_index: 0,
        };
        let payload = vec![0x5au8; 4096];
        let mut trailer_file = payload.clone();
        write_trailer(&meta, &payload, &mut trailer_file);
        let trailer_path = dir.path().join("set.part1.rev");
        std::fs::write(&trailer_path, &trailer_file).unwrap();

        assert_eq!(
            parse_trailer_file(&trailer_path).unwrap(),
            parse_trailer(&trailer_file)
        );
        assert!(trailer_style(&trailer_path));

        // A flipped payload byte fails the streamed CRC check.
        let mut corrupt = trailer_file.clone();
        corrupt[0] ^= 0xff;
        let corrupt_path = dir.path().join("corrupt.part1.rev");
        std::fs::write(&corrupt_path, &corrupt).unwrap();
        assert!(!trailer_style(&corrupt_path));

        // A legacy-format file carries no trailer.
        let legacy_path = dir.path().join("set4_1_1.rev");
        std::fs::write(&legacy_path, b"parity payload").unwrap();
        assert!(!trailer_style(&legacy_path));
    }

    /// Deterministic non-archive byte patterns: the builder only reads the
    /// volume files, so plain files exercise the `.rev` codec directly.
    fn write_fake_volumes(dir: &Path, sizes: &[u64]) -> Vec<PathBuf> {
        write_fake_volumes_padded(dir, sizes, 1)
    }

    /// [`write_fake_volumes`] with an explicit part-number padding.
    fn write_fake_volumes_padded(dir: &Path, sizes: &[u64], padding: usize) -> Vec<PathBuf> {
        let mut volumes = Vec::with_capacity(sizes.len());
        for (i, &size) in sizes.iter().enumerate() {
            let path = dir.join(format!(
                "set.part{:0padding$}.rar",
                i + 1,
                padding = padding
            ));
            let mut bytes = vec![0u8; size as usize];
            let mut state = 0x1234_5678u32.wrapping_add(i as u32 + 1);
            for byte in &mut bytes {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *byte = (state >> 16) as u8;
            }
            std::fs::write(&path, &bytes).unwrap();
            volumes.push(path);
        }
        volumes
    }

    /// Staging temp names the builder may have leaked into `dir`.
    fn temp_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5tmp"))
            .collect()
    }

    /// Commit-transaction names (`rar5bak`/`rar5commit`) left in `dir`.
    fn commit_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5bak") || name.contains("rar5commit"))
            .collect()
    }

    /// Reference parity for the legacy (full-parity) layout, computed with
    /// full zero-padded volume shards in memory.
    fn legacy_reference(volumes: &[PathBuf], rec_count: usize) -> Vec<Vec<u8>> {
        let chunks: Vec<Vec<u8>> = volumes
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let shard_len = chunks.iter().map(Vec::len).max().unwrap();
        let coder = Rsc8::new(rec_count).unwrap();
        let mut payloads: Vec<Vec<u8>> = vec![Vec::with_capacity(shard_len); rec_count];
        let mut column = vec![0u8; chunks.len()];
        for offset in 0..shard_len {
            for (index, chunk) in chunks.iter().enumerate() {
                column[index] = chunk.get(offset).copied().unwrap_or(0);
            }
            for (payload, byte) in payloads.iter_mut().zip(coder.encode(&column)) {
                payload.push(byte);
            }
        }
        payloads
    }

    /// A 64-byte stripe over ~1 KiB volumes spans many stripes; the written
    /// parity must equal the buffered reference, and a missing volume must
    /// rebuild byte-identically through the streaming reader.
    #[test]
    fn streaming_build_and_rebuild_match_buffered_reference() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 1024, 700]);
        let expected = legacy_reference(&volumes, 8);

        let written = build_recovery_volumes_for_set_chunked(&volumes, 8, 64).unwrap();
        assert_eq!(written.len(), 8);
        for (k, path) in written.iter().enumerate() {
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                format!("set.part{}_{}_{}.rev", volumes.len(), 8, k + 1),
                "the builder must return the final paths"
            );
            let actual = std::fs::read(path).unwrap();
            assert_eq!(actual.len(), expected[k].len(), "rev {k} length");
            let diff = actual.iter().zip(&expected[k]).position(|(a, b)| a != b);
            assert_eq!(diff, None, "rev {k} first difference");
        }
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "successful build left temps: {:?}",
            temp_leftovers(dir.path())
        );

        // Only the last volume of a set may be short, so a full middle
        // volume is the reconstructable victim.
        let missing = std::fs::read(&volumes[1]).unwrap();
        std::fs::remove_file(&volumes[1]).unwrap();
        let rebuilt = rebuild_missing_volumes_chunked(&volumes[0], None, None, 64).unwrap();
        assert_eq!(rebuilt, vec![volumes[1].clone()]);
        assert_eq!(std::fs::read(&volumes[1]).unwrap(), missing);
    }

    /// A five-digit part width: a legacy `.rev` name carries no padding, so
    /// the data-volume probe must cover five digits and a reconstructed
    /// missing volume must keep the set's own padding.
    #[test]
    fn five_digit_legacy_set_rebuilds_with_padded_names() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes_padded(dir.path(), &[1024, 1024, 700], 5);
        let original = std::fs::read(&volumes[1]).unwrap();
        let revs = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap();
        // Plain files do not end in zeros: the builder picks the legacy
        // full-parity layout, whose name carries no part padding.
        assert_eq!(
            revs[0].file_name().unwrap().to_string_lossy(),
            "set.part3_2_1.rev"
        );

        std::fs::remove_file(&volumes[1]).unwrap();
        let rebuilt = rebuild_missing_volumes_chunked(&volumes[0], None, None, 64).unwrap();
        assert_eq!(rebuilt, vec![volumes[1].clone()]);
        assert_eq!(std::fs::read(&volumes[1]).unwrap(), original);
    }

    /// A failed streaming build removes the temps it wrote and leaves a
    /// pre-existing `.rev` untouched.
    #[test]
    fn streaming_build_failure_leaves_existing_revs_and_removes_temps() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 1024]);
        // A pre-existing `.rev` for the second output: the failed build
        // must leave it byte-identical.
        let existing = dir.path().join("set.part2_2_2.rev");
        let keep = b"pre-existing parity".to_vec();
        std::fs::write(&existing, &keep).unwrap();
        // Occupy the first final path with a directory so installing the
        // first completed temp fails after the whole parity set is built.
        std::fs::create_dir(dir.path().join("set.part2_2_1.rev")).unwrap();

        let error = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            keep,
            "the pre-existing .rev must survive the failed build"
        );
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
    }

    /// The partial-install regression: with the conflict on a *later* final
    /// path, the old loop had already replaced the first pre-existing `.rev`
    /// when the install failed. The transactional install must leave it
    /// byte-identical and remove every temp.
    #[test]
    fn streaming_build_partial_install_rolls_back_existing_revs() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 1024]);
        let existing = dir.path().join("set.part2_2_1.rev");
        let keep = b"pre-existing parity one".to_vec();
        std::fs::write(&existing, &keep).unwrap();
        // Occupy the second final path so the install cannot complete after
        // the first `.rev` would have been replaced.
        std::fs::create_dir(dir.path().join("set.part2_2_2.rev")).unwrap();

        let error = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            keep,
            "the first .rev must be rolled back"
        );
        assert!(
            dir.path().join("set.part2_2_2.rev").is_dir(),
            "the conflicting directory must stay untouched"
        );
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
        assert!(
            commit_leftovers(dir.path()).is_empty(),
            "commit leftovers: {:?}",
            commit_leftovers(dir.path())
        );
    }

    /// A failure inside the install transaction (the journal temp path is
    /// occupied) leaves every pre-existing `.rev` untouched and sweeps all
    /// staged temps.
    #[test]
    fn streaming_build_commit_failure_keeps_existing_revs_and_removes_temps() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 1024]);
        let first = dir.path().join("set.part2_2_1.rev");
        let second = dir.path().join("set.part2_2_2.rev");
        let keep_first = b"pre-existing parity one".to_vec();
        let keep_second = b"pre-existing parity two".to_vec();
        std::fs::write(&first, &keep_first).unwrap();
        std::fs::write(&second, &keep_second).unwrap();
        // `commit_files` writes its journal through this exact sibling name.
        std::fs::create_dir(dir.path().join(".set.rar5commit.journal.tmp")).unwrap();

        let error = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(std::fs::read(&first).unwrap(), keep_first);
        assert_eq!(std::fs::read(&second).unwrap(), keep_second);
        // Drop the planted conflict so only transaction leftovers remain.
        std::fs::remove_dir(dir.path().join(".set.rar5commit.journal.tmp")).unwrap();
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
        assert!(
            commit_leftovers(dir.path()).is_empty(),
            "commit leftovers: {:?}",
            commit_leftovers(dir.path())
        );
    }

    /// A failure after the damaged original was parked as `*.bad` (the
    /// staged rebuild cannot be synced) must restore the original bytes and
    /// leave no `.bad` copy behind.
    #[test]
    fn failed_install_after_bad_rename_restores_the_damaged_original() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("set.part2.rar");
        let damaged = b"damaged volume bytes".to_vec();
        std::fs::write(&final_path, &damaged).unwrap();
        // The staged rebuild never exists, so the commit fails after
        // `final_path` was renamed to `set.part2.rar.bad`.
        let missing_tmp = dir.path().join(".set.part2.rar.rar5tmp-x");
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&final_path)
            .unwrap();

        let error = commit_rebuilt_volumes(
            std::slice::from_ref(&final_path),
            &[0],
            vec![(0, missing_tmp, handle)],
            1,
            0,
        )
        .unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            damaged,
            "the damaged original must be restored"
        );
        assert!(
            !dir.path().join("set.part2.rar.bad").exists(),
            "the parked copy must be moved back, not left behind"
        );
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
    }

    /// On success the damaged original stays parked as `*.bad` and the
    /// rebuilt temp lands at the final path.
    #[test]
    fn install_rebuilt_volume_keeps_the_damaged_original_as_bad() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("set.part2.rar");
        std::fs::write(&final_path, b"damaged").unwrap();
        let tmp = dir.path().join(".set.part2.rar.rar5tmp-x");
        std::fs::write(&tmp, b"rebuilt").unwrap();
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tmp)
            .unwrap();

        let rebuilt = commit_rebuilt_volumes(
            std::slice::from_ref(&final_path),
            &[0],
            vec![(0, tmp.clone(), handle)],
            1,
            0,
        )
        .unwrap();
        assert_eq!(rebuilt, vec![final_path.clone()]);
        assert_eq!(std::fs::read(&final_path).unwrap(), b"rebuilt");
        assert_eq!(
            std::fs::read(dir.path().join("set.part2.rar.bad")).unwrap(),
            b"damaged"
        );
        assert!(!tmp.exists(), "the staged temp must be consumed");
    }

    /// Trailer-format builds append a valid trailer after the streamed
    /// payload, and the rebuild path ignores those seven non-parity bytes.
    #[test]
    fn streaming_trailer_build_writes_valid_trailer() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 900, 1100]);
        // Zero tails select the trailer layout (WinRAR's ENDARC padding).
        for path in &volumes {
            let mut bytes = std::fs::read(path).unwrap();
            bytes.extend_from_slice(&[0u8; TRAILER_LEN]);
            std::fs::write(path, &bytes).unwrap();
        }
        let originals: Vec<Vec<u8>> = volumes
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let shard_len = originals.iter().map(Vec::len).max().unwrap();

        let written = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap();
        assert_eq!(written.len(), 2);
        for (k, path) in written.iter().enumerate() {
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                format!("set.part{}.rev", k + 1),
                "the builder must return the final paths"
            );
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(bytes.len(), shard_len);
            let mut file = std::fs::File::open(path).unwrap();
            assert_eq!(
                super::parse_trailer_reader(&mut file).unwrap(),
                Some(Meta {
                    data_count: 3,
                    rec_count: 2,
                    recovery_index: k,
                })
            );
        }
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "successful build left temps: {:?}",
            temp_leftovers(dir.path())
        );

        let missing = originals[2].clone();
        std::fs::remove_file(&volumes[2]).unwrap();
        let rebuilt = rebuild_missing_volumes_chunked(&volumes[0], None, None, 64).unwrap();
        assert_eq!(rebuilt, vec![volumes[2].clone()]);
        assert_eq!(std::fs::read(&volumes[2]).unwrap(), missing);
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
