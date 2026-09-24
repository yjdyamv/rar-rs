//! `.rev` name recognition and resolution.
//!
//! Both naming families (`.partN.rar` sets and the old `.rar`/`.rNN` sets) and
//! both layouts are recognised from the file name alone; a name whose base ends
//! in digits is ambiguous, so every candidate parse is scored against the data
//! volumes that exist on disk.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{RarError, RarResult};

use super::layout::{Layout, data_slots_score};
use super::rs8::MAX_CODEWORD;
use super::trailer::{Format, Meta, parse_trailer_file};

/// The four on-disk `.rev` name shapes this module understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NameKind {
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
    pub(super) fn format(self) -> Format {
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
pub(super) struct RevName {
    pub(super) base: String,
    pub(super) new_naming: bool,
    /// Zero-padding width of the part number for new-naming sets (0 = the
    /// name did not carry one).
    pub(super) width: usize,
    pub(super) kind: NameKind,
    /// Metadata encoded in a legacy name.
    pub(super) meta: Option<Meta>,
}

impl RevName {
    /// The volume layout this parse describes; the candidate-to-probe
    /// mapping every caller shares.
    pub(super) fn layout(&self) -> Layout {
        Layout {
            base: self.base.clone(),
            new_naming: self.new_naming,
            width: self.width,
        }
    }
}

/// Split trailing decimal groups off a stem. Groups are returned
/// right-to-left (closest to the end first) so `x4_2_1` yields `[1, 2, 4]`.
pub(super) fn trailing_groups(stem: &str, count: usize) -> Option<(Vec<usize>, usize)> {
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
pub(super) fn strip_part_infix(base: &str) -> (&str, bool) {
    if let Some(stripped) = base.strip_suffix(".part")
        && !stripped.is_empty()
    {
        return (stripped, true);
    }
    (base, false)
}

/// Every plausible parse of a `.rev` file name, most specific first.
pub(super) fn rev_name_candidates(name: &str) -> Vec<RevName> {
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

/// Candidate part-number paddings for a `.partN.rar` probe: the layout's
/// own width first (when known), then 1..=5 digits. WinRAR pads the part
/// number to the digit count of the volume count; five digits cover the
/// format's 65535-volume maximum, so a fixed 1..=4 scan must not hide
/// `part00001.rar`.
pub(super) fn part_width_candidates(width: usize) -> Vec<usize> {
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
                let score = data_slots_score(parent, &candidate, &meta);
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
    if read >= 8 && head == *crate::detect::RAR5_SIGNATURE {
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

/// Old-naming trailer-format `.rev` path (`k` is the zero-based recovery
/// index): `{base}{N}.rev`.
pub(super) fn old_trailer_name(base: &str, k: usize) -> String {
    format!("{base}{}.rev", k + 1)
}

/// Old-naming legacy-format `.rev` path: the counts live in the name,
/// `{base}{data}_{rec}_{index}.rev`.
pub(super) fn old_legacy_name(base: &str, k: usize, data_count: usize, rec_count: usize) -> String {
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
        .ok_or_else(|| RarError::format(format!("{}: not a recovery path", first.display())))?;
    let candidates = rev_name_candidates(name);
    let parsed = candidates
        .first()
        .ok_or_else(|| RarError::format(format!("{name}: not a recovery-volume name")))?;
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
        return Err(RarError::format(format!(
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
pub(super) fn is_rar5_recovery_volume(path: &Path) -> bool {
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
pub(super) fn trailer_style(path: &Path) -> bool {
    parse_trailer_file(path).ok().flatten().is_some()
}
