//! Volume naming policy: base extraction, part-number path builders and
//! the `.partN.rar` name parser shared by archive discovery, creation and
//! the recovery-volume machinery.

use std::path::{Path, PathBuf};

/// Strip a trailing `.rar` or `.rev` extension (ASCII case-insensitive).
/// ASCII lowering preserves byte offsets, so the returned slice is valid
/// for the original name (a Unicode `to_lowercase` can expand characters).
fn strip_archive_extension(name: &str) -> Option<&str> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".rar") || lower.ends_with(".rev") {
        Some(&name[..name.len() - 4])
    } else {
        None
    }
}

/// Split a `{base}.part{N}` stem (extension already stripped) at its LAST
/// `.part`: returns `(base, digit width)` when the trailing segment is all
/// ASCII digits and the base is non-empty. The last occurrence matters for
/// bases that themselves contain `.part` (`my.partition.part2.rar` →
/// `("my.partition", 1)`). Matching is ASCII case-insensitive; the returned
/// base slices the original `name`, preserving its casing.
fn split_part_stem(name: &str) -> Option<(&str, usize)> {
    let lower = name.to_ascii_lowercase();
    let (base, tail) = lower.rsplit_once(".part")?;
    if base.is_empty() || tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&name[..base.len()], tail.len()))
}

/// Extract the volume base and the zero-padding width of the part number
/// from a name like `archive.part3.rar` → `("archive", 1)` or
/// `archive.part03.rar` → `("archive", 2)`. WinRAR pads the number to the
/// digit count of the total volume count (part01..part15), so both forms
/// must be discoverable.
pub(crate) fn extract_volume_base(name: &str) -> Option<(String, usize)> {
    let stem = strip_archive_extension(name)?;
    split_part_stem(stem).map(|(base, width)| (base.to_string(), width))
}

/// Volume base of an archive path, stripping `.partN.rar` or `.rar`
/// suffixes (used by the recovery-volume machinery).
pub(crate) fn volume_base_of(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    if let Some((base, _)) = extract_volume_base(name) {
        return base;
    }
    if let Some(stem) = name.strip_suffix(".rar") {
        return stem.to_string();
    }
    if let Some(stem) = name.strip_suffix(".RAR") {
        return stem.to_string();
    }
    name.to_string()
}

/// Zero-padding width of the part number in a volume name
/// (`archive.part03.rar` → 2, `archive.part3.rar` → 1). Used to name
/// `.rev` files identically to their volume set.
pub(crate) fn volume_part_width(path: &Path) -> usize {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(extract_volume_base)
        .map(|(_, w)| w)
        .unwrap_or(1)
}

pub(crate) fn volume_path(parent: &Path, base: &str, part_num: usize) -> PathBuf {
    parent.join(format!("{base}.part{part_num}.rar"))
}

/// Legacy volume naming ceiling: `{base}.rar` plus `r00`..`z99` — 901
/// volumes. Larger sets cannot be named (or discovered) in the legacy
/// `.rNN` scheme.
pub(crate) const LEGACY_VOLUME_MAX: usize = 901;

/// Legacy RAR 1.5–3.x volume name: the first volume is `{base}.rar`, then
/// `{base}.r00`, `{base}.r01`, … `.r99`, then `{base}.s00`, … — one extension
/// letter per hundred volumes, matching WinRAR's RAR4 multi-volume naming.
pub(crate) fn volume_path_rar4(parent: &Path, base: &str, part_num: usize) -> PathBuf {
    if part_num <= 1 {
        return parent.join(format!("{base}.rar"));
    }
    let idx = part_num - 2;
    let letter = b'r' + (idx / 100) as u8;
    let num = idx % 100;
    parent.join(format!("{base}.{}{:02}", letter as char, num))
}

/// Legacy volume base from `x.rar` / `x.r00` / `x.s37` (case-insensitive),
/// the inverse of [`volume_path_rar4`].
pub(crate) fn legacy_volume_base(name: &str) -> Option<String> {
    // ASCII lowercasing keeps byte offsets valid for slicing `name`.
    let lower = name.to_ascii_lowercase();
    if let Some(base) = lower.strip_suffix(".rar") {
        return Some(name[..base.len()].to_string());
    }
    let bytes = lower.as_bytes();
    if bytes.len() >= 5
        && bytes[bytes.len() - 4] == b'.'
        && bytes[bytes.len() - 3].is_ascii_lowercase()
        && bytes[bytes.len() - 3] >= b'r'
        && bytes[bytes.len() - 3] <= b'z'
        && bytes[bytes.len() - 2].is_ascii_digit()
        && bytes[bytes.len() - 1].is_ascii_digit()
    {
        let end = bytes.len() - 4;
        return Some(name[..end].to_string());
    }
    None
}

/// Volume path with the part number zero-padded to `width` digits
/// (`part01.rar` for width 2), matching WinRAR's naming for sets of 10
/// or more volumes.
pub(crate) fn volume_path_padded(
    parent: &Path,
    base: &str,
    part_num: usize,
    width: usize,
) -> PathBuf {
    parent.join(format!("{base}.part{part_num:0width$}.rar"))
}

/// Base name of a `{base}.partN.rar` volume file, if the name parses as one.
fn part_volume_base(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".rar")
        .or_else(|| name.strip_suffix(".RAR"))?;
    split_part_stem(stem).map(|(base, _)| base)
}

/// Base name of a `{base}.partN.rev` recovery-volume file, if the name parses
/// as one.
fn part_recovery_base(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".rev")
        .or_else(|| name.strip_suffix(".REV"))?;
    split_part_stem(stem).map(|(base, _)| base)
}

/// Existing volume files of the set based at `base` that `keep` does not
/// cover, in the naming family selected by `rar4`.
///
/// The multi-volume commit uses this to retire leftover files from a
/// previous set: a shrinking overwrite must not leave stale parts behind,
/// and an overwrite that no longer requests recovery volumes must not leave
/// the old `.rev` files behind either. Only files that parse as volumes (or
/// `.rev` recovery volumes — RAR5 and legacy, the latter resolved by data
/// volumes) of `base` are returned, so the new set's own staged temporaries
/// never match.
pub(crate) fn stale_volume_paths(
    parent: &Path,
    base: &str,
    rar4: bool,
    keep: &[PathBuf],
) -> Vec<PathBuf> {
    // A bare archive name has an empty `parent()`; it still means the current
    // directory, and the `keep` paths were built the same way, so compare by
    // file name rather than by (differently prefixed) full path.
    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let keep_names: Vec<&std::ffi::OsStr> =
        keep.iter().filter_map(|path| path.file_name()).collect();
    let mut stale = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return stale;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name() else {
            continue;
        };
        if keep_names.contains(&file_name) {
            continue;
        }
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let matches = if rar4 {
            // Legacy data volumes plus their `.rev` recovery volumes: an
            // overwrite that drops `-rv` must retire the old parity files.
            legacy_volume_base(name).as_deref() == Some(base)
                || crate::recovery::rev3::rev_name_belongs_to_set(dir, base, name)
        } else {
            // The old set's `.rev` recovery volumes go with it: they are
            // regenerated after the new data volumes commit.
            part_volume_base(name) == Some(base) || part_recovery_base(name) == Some(base)
        };
        if matches {
            stale.push(path);
        }
    }
    stale.sort();
    stale
}

#[cfg(test)]
mod tests {
    use super::stale_volume_paths;
    use std::path::Path;

    #[test]
    fn part_base_splits_at_the_last_part_segment() {
        assert_eq!(
            super::extract_volume_base("my.partition.part2.rar"),
            Some(("my.partition".to_string(), 1))
        );
        assert_eq!(
            super::extract_volume_base("my.part2.rar"),
            Some(("my".to_string(), 1))
        );
        // `part01`: part number 1, zero-padding width 2.
        assert_eq!(
            super::extract_volume_base("set.part01.rar"),
            Some(("set".to_string(), 2))
        );
        // Not part-volume names: the `.part` inside `partition` is not the
        // segment separator, and `.rar`/`.rNN` legacy naming is untouched.
        assert_eq!(super::extract_volume_base("my.partition.rar"), None);
        assert_eq!(super::extract_volume_base("archive.rar"), None);
        assert_eq!(super::extract_volume_base("archive.r00"), None);
        assert_eq!(
            super::legacy_volume_base("archive.rar").as_deref(),
            Some("archive")
        );
        assert_eq!(
            super::legacy_volume_base("archive.r00").as_deref(),
            Some("archive")
        );
    }

    #[test]
    fn volume_base_of_agrees_with_part_discovery() {
        assert_eq!(
            super::volume_base_of(Path::new("dir/my.partition.part2.rar")),
            "my.partition"
        );
        assert_eq!(
            super::volume_base_of(Path::new("dir/my.partition.rar")),
            "my.partition"
        );
        assert_eq!(super::volume_base_of(Path::new("dir/my.part2.rar")), "my");
        assert_eq!(
            super::volume_base_of(Path::new("dir/archive.rar")),
            "archive"
        );
        // The legacy `.rNN` fallback still strips only `.rar`.
        assert_eq!(
            super::volume_base_of(Path::new("dir/archive.r00")),
            "archive.r00"
        );
    }

    #[test]
    fn stale_scan_ignores_staged_temporaries_and_kept_parts() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        std::fs::write(parent.join("set.part01.rar"), b"old").unwrap();
        std::fs::write(parent.join("set.part02.rar"), b"old").unwrap();
        std::fs::write(parent.join(".set.rar.rar5tmp-1.part1.rar"), b"stage").unwrap();
        std::fs::write(parent.join("other.part01.rar"), b"other").unwrap();

        let keep = vec![parent.join("set.part01.rar")];
        let stale = stale_volume_paths(parent, "set", false, &keep);
        let names: Vec<String> = stale
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["set.part02.rar"]);
    }

    #[test]
    fn stale_scan_matches_legacy_rar4_volume_names() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        std::fs::write(parent.join("set.rar"), b"first").unwrap();
        std::fs::write(parent.join("set.r00"), b"second").unwrap();
        std::fs::write(parent.join("set.r01"), b"third").unwrap();

        let keep = vec![parent.join("set.rar")];
        let stale = stale_volume_paths(parent, "set", true, &keep);
        let names: Vec<String> = stale
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["set.r00", "set.r01"]);
    }

    #[test]
    fn rev_ownership_resolves_ambiguous_bases_and_case() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        for name in ["set4.rar", "set4.r00", "set4.r01", "set4.r02"] {
            std::fs::write(parent.join(name), b"x").unwrap();
        }
        // `set44_2_1.rev` parses as `set` + data 44 or `set4` + data 4; the
        // existing data volumes decide.
        assert!(!crate::recovery::rev3::rev_name_belongs_to_set(
            parent,
            "set",
            "set44_2_1.rev"
        ));
        assert!(crate::recovery::rev3::rev_name_belongs_to_set(
            parent,
            "set4",
            "set44_2_1.rev"
        ));
        assert!(crate::recovery::rev3::rev_name_belongs_to_set(
            parent,
            "set4",
            "SET44_2_1.REV"
        ));
        assert!(!crate::recovery::rev3::rev_name_belongs_to_set(
            parent,
            "set4",
            "other4_2_1.rev"
        ));
    }

    #[test]
    fn stale_scan_retires_legacy_recovery_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        std::fs::write(parent.join("set.rar"), b"first").unwrap();
        std::fs::write(parent.join("set.r00"), b"second").unwrap();
        // The four legacy `.rev` name shapes for this base.
        std::fs::write(parent.join("set.part2.rev"), b"rev").unwrap();
        std::fs::write(parent.join("set2.rev"), b"rev").unwrap();
        std::fs::write(parent.join("set3_2_1.rev"), b"rev").unwrap();
        std::fs::write(parent.join("set.part3_2_1.rev"), b"rev").unwrap();
        // A different base and a non-rev file must survive.
        std::fs::write(parent.join("set-extra.rev"), b"other").unwrap();
        std::fs::write(parent.join("set.txt"), b"other").unwrap();

        let keep = vec![parent.join("set.rar"), parent.join("set.r00")];
        let stale = stale_volume_paths(parent, "set", true, &keep);
        let names: Vec<String> = stale
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "set.part2.rev",
                "set.part3_2_1.rev",
                "set2.rev",
                "set3_2_1.rev"
            ]
        );
    }
}

#[test]
fn unicode_names_do_not_panic_on_case_folding() {
    // `İ` lowercases to two chars, so a lowercase-derived byte index is
    // not a valid index into the original name.
    let name = "İİİİİ.rar";
    assert_eq!(legacy_volume_base(name).as_deref(), Some("İİİİİ"));
    assert_eq!(
        extract_volume_base("İİİİİ.part07.rar"),
        Some(("İİİİİ".to_string(), 2))
    );
}
