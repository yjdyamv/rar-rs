//! Recovery-set discovery, naming layout and data-volume slots.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{RarError, RarResult};
use crate::fs::volume::{legacy_volume_base, volume_part_width, volume_path_rar4};

use super::name::{NameKind, RevName, part_width_candidates, rev_name_candidates};
use super::rs8::MAX_CODEWORD;
use super::trailer::{Format, Meta, TRAILER_LEN, parse_trailer_file, parse_trailer_reader};

/// Data and recovery volume paths for one naming family.
#[derive(Clone, Debug)]
pub(super) struct Layout {
    pub(super) base: String,
    pub(super) new_naming: bool,
    /// Part-number padding used by `.partN.rar` sets (1 = `part1.rar`,
    /// 2 = `part01.rar`); ignored for old-naming sets.
    pub(super) width: usize,
}

impl Layout {
    /// Trailer-format `.rev` path (`k` is the zero-based recovery index).
    pub(super) fn trailer_rev_path(&self, parent: &Path, k: usize) -> PathBuf {
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
    pub(super) fn legacy_rev_path(&self, parent: &Path, k: usize, meta: &Meta) -> PathBuf {
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
pub(super) fn new_data_path(
    parent: &Path,
    base: &str,
    width: usize,
    index: usize,
) -> Option<PathBuf> {
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
pub(super) fn slot_exists(parent: &Path, layout: &Layout, index: usize) -> bool {
    if layout.new_naming {
        return new_data_path(parent, &layout.base, layout.width, index).is_some();
    }
    volume_path_rar4(parent, &layout.base, index + 1).exists()
}

/// Score a name parse by how many of its data volumes exist on disk.
///
/// Names that end in digits are ambiguous (`set44_2_1.rev` reads as `set`
/// plus data volume 44 or `set4` plus volume 4); every caller resolves them
/// by preferring the split whose data set is actually present — ties keep
/// the first candidate, like WinRAR's scan order.
pub(super) fn data_slots_score(parent: &Path, candidate: &RevName, meta: &Meta) -> usize {
    let probe = candidate.layout();
    (0..meta.data_count)
        .filter(|index| slot_exists(parent, &probe, *index))
        .count()
}

/// One `.rev` file's parity source. The bytes stay on disk and are seeked
/// stripe by stripe; for trailer-format files the seven trailer bytes read
/// back as zeros (those offsets carry no parity, matching WinRAR).
#[derive(Clone, Debug)]
pub(super) struct RevSource {
    pub(super) index: usize,
    pub(super) path: PathBuf,
    /// File length (= payload length; the trailer is part of the file but
    /// is not parity).
    pub(super) len: u64,
}

/// Everything `rc` needs about a recovery set: the naming layout, the
/// volume counts, the on-disk layout and the recovery payload sources
/// (sorted by recovery index).
pub(super) struct RecoverySet {
    pub(super) layout: Layout,
    pub(super) meta: Meta,
    pub(super) format: Format,
    pub(super) payloads: Vec<RevSource>,
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
pub(super) fn collect_recovery_volumes(parent: &Path, base: &str) -> RarResult<RecoverySet> {
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
            let score = data_slots_score(parent, &candidate, &file_meta);
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
pub(super) fn identify(path: &Path) -> RarResult<(PathBuf, Layout, Option<Meta>)> {
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
            let score = data_slots_score(&parent, &candidate, &file_meta);
            if best
                .as_ref()
                .is_none_or(|(best_score, _, _)| score > *best_score)
            {
                best = Some((score, candidate, Some(file_meta)));
            }
        }
        if let Some((_, parsed, file_meta)) = best {
            return Ok((parent, parsed.layout(), file_meta));
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

/// True when the volume's last seven bytes are zero, i.e. the trailer
/// layout loses nothing on rebuild (WinRAR's choice too).
pub(super) fn use_trailer_format(volume_paths: &[PathBuf], sizes: &[u64]) -> RarResult<bool> {
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
pub(super) fn recovery_name_layout(
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

/// Resolve the data-volume path of every slot, preferring the naming
/// family with the most existing files (falling back to the family the
/// recovery-volume name implies) and returning `(paths, missing_indices)`.
pub(super) fn resolve_data_slots(
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
