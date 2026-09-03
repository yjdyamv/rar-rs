//! Multi-volume discovery and volume-path helpers.

use std::path::{Path, PathBuf};

/// Discover all volumes of a multi-volume RAR5 archive.
///
/// Given any volume path, returns a sorted list of all volume paths
/// starting from part1. Uses `.partN.rar` naming convention.
pub fn discover_volumes(path: &Path) -> Vec<PathBuf> {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return vec![path.to_path_buf()],
    };

    // Match .partN.rar naming (zero-padded or not; WinRAR pads to the
    // digit count of the total volume count, e.g. part01..part15).
    if let Some((base, width)) = extract_volume_base(&name) {
        let parent = path.parent().unwrap_or(Path::new("."));
        let mut volumes = Vec::new();
        let mut n = 1u64;
        loop {
            let vol = parent.join(if width > 1 {
                format!("{base}.part{:0width$}.rar", n, width = width)
            } else {
                format!("{base}.part{n}.rar")
            });
            if vol.exists() {
                volumes.push(vol);
                n += 1;
            } else {
                break;
            }
        }
        if !volumes.is_empty() {
            return volumes;
        }
        // Fall back to the unpadded enumeration (mixed/odd sets).
        let mut n = 1u64;
        loop {
            let vol = parent.join(format!("{base}.part{n}.rar"));
            if vol.exists() {
                volumes.push(vol);
                n += 1;
            } else {
                break;
            }
        }
        if !volumes.is_empty() {
            return volumes;
        }
    }

    // Check if path itself names a single-volume file that has a .part1.rar sibling
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let parent = path.parent().unwrap_or(Path::new("."));
        let part1 = parent.join(format!("{stem}.part1.rar"));
        if part1.exists() && part1 != path {
            return discover_volumes(&part1);
        }
        // Also probe zero-padded first volumes ({stem}.part01.rar ..
        // part0001.rar): sets written with 10+ volumes now carry the
        // padding themselves, and a caller may pass the base name.
        for width in 2..=4 {
            let probe = parent.join(format!("{stem}.part{:0width$}.rar", 1, width = width));
            if probe.exists() && probe != path {
                return discover_volumes(&probe);
            }
        }
    }

    vec![path.to_path_buf()]
}

/// Extract volume base from a filename like `archive.part3.rar` → `archive`.
/// Extract the volume base and the zero-padding width of the part number
/// from a name like `archive.part3.rar` → `("archive", 1)` or
/// `archive.part03.rar` → `("archive", 2)`. WinRAR pads the number to the
/// digit count of the total volume count (part01..part15), so both forms
/// must be discoverable.
fn extract_volume_base(name: &str) -> Option<(String, usize)> {
    // Case-insensitive match for .partN.rar
    let lower = name.to_lowercase();
    if let Some(idx) = lower.find(".part") {
        let after = &lower[idx + 5..];
        if let Some(rar_idx) = after.find(".rar") {
            let num_str = &after[..rar_idx];
            if num_str.chars().all(|c| c.is_ascii_digit()) && !num_str.is_empty() {
                return Some((name[..idx].to_string(), num_str.len()));
            }
        }
    }
    None
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
