//! Multi-volume archive discovery.

use std::path::{Path, PathBuf};

use crate::fs::volume::{extract_volume_base, legacy_volume_base};

/// Discover all volumes of a multi-volume RAR5 or legacy archive.
///
/// Given any volume path, returns a sorted list of all volume paths
/// starting from the first. Handles the `.partN.rar` naming convention
/// (zero-padded or not; WinRAR pads to the digit count of the total volume
/// count, e.g. `part01..part15`) and the legacy naming used by RAR 1.5–3.x
/// sets (first volume `x.rar`, then `x.r00`, `x.r01`, … `.r99`, then
/// `x.s00`, … — one letter per hundred volumes).
pub fn discover_volumes(path: &Path) -> Vec<PathBuf> {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return vec![path.to_path_buf()],
    };

    // Match .partN.rar naming (zero-padded or not).
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

    // Legacy volume naming: x.rar, x.r00, x.r01, …; extension letters
    // advance every hundred volumes (r, s, t, …).
    if let Some(base) = legacy_volume_base(&name) {
        let parent = path.parent().unwrap_or(Path::new("."));
        let mut volumes = Vec::new();
        let first = parent.join(format!("{base}.rar"));
        if first.exists() {
            volumes.push(first);
        }
        let mut found_any = !volumes.is_empty();
        for letter in b'r'..=b'z' {
            let mut any_in_run = false;
            for n in 0..100 {
                let vol = parent.join(format!("{base}.{}{:02}", letter as char, n));
                if vol.exists() {
                    volumes.push(vol);
                    any_in_run = true;
                    found_any = true;
                } else if any_in_run {
                    // A gap ends the run.
                    break;
                } else if !volumes.is_empty() && n == 0 {
                    // Next letter after a completed run.
                    break;
                }
            }
            if !any_in_run && !volumes.is_empty() {
                break;
            }
        }
        if found_any {
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
