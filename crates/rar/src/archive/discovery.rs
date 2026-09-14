//! Multi-volume archive discovery.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::fs::volume::{extract_volume_base, legacy_volume_base};

/// Case-insensitive view of one directory's entries.
///
/// Foreign sets may spell their volume names in upper case (`MULTIVOL.RAR` +
/// `MULTIVOL.R00`, as the DOS-era rars fixtures do) while every probe below
/// is lower case. Windows folds case for free; a case-sensitive filesystem
/// does not, and the set then looks like a single truncated volume. The
/// index is consulted only when the exact probe is missing, and returns the
/// real on-disk spelling.
struct SiblingIndex {
    names: HashMap<String, PathBuf>,
}

impl SiblingIndex {
    fn new(dir: &Path) -> Self {
        let mut names = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    // First spelling wins on a case collision (possible on a
                    // case-sensitive filesystem).
                    names
                        .entry(name.to_ascii_lowercase())
                        .or_insert_with(|| entry.path());
                }
            }
        }
        Self { names }
    }

    /// The real path for `candidate` (always a direct child of the indexed
    /// directory) when it exists under either spelling.
    fn resolve(&self, candidate: &Path) -> Option<PathBuf> {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        let name = candidate.file_name()?.to_str()?;
        self.names.get(&name.to_ascii_lowercase()).cloned()
    }
}

/// Discover all volumes of a multi-volume RAR5 or legacy archive.
///
/// Given any volume path, returns a sorted list of all volume paths
/// starting from the first. Handles the `.partN.rar` naming convention
/// (zero-padded or not; WinRAR pads to the digit count of the total volume
/// count, e.g. `part01..part15`) and the legacy naming used by RAR 1.5–3.x
/// sets (first volume `x.rar`, then `x.r00`, `x.r01`, … `.r99`, then
/// `x.s00`, … — one letter per hundred volumes). Volume names are matched
/// ASCII-case-insensitively so upper-case sets open on case-sensitive
/// filesystems too.
pub fn discover_volumes(path: &Path) -> Vec<PathBuf> {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return vec![path.to_path_buf()],
    };
    let parent = path.parent().unwrap_or(Path::new("."));
    // `read_dir("")` is invalid; a bare file name still means the current
    // directory.
    let scan_dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let index = SiblingIndex::new(scan_dir);

    // Match .partN.rar naming (zero-padded or not).
    if let Some((base, width)) = extract_volume_base(&name) {
        let mut volumes = Vec::new();
        let mut n = 1u64;
        loop {
            let vol = parent.join(if width > 1 {
                format!("{base}.part{:0width$}.rar", n, width = width)
            } else {
                format!("{base}.part{n}.rar")
            });
            if let Some(vol) = index.resolve(&vol) {
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
            if let Some(vol) = index.resolve(&vol) {
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
        let mut volumes = Vec::new();
        if let Some(first) = index.resolve(&parent.join(format!("{base}.rar"))) {
            volumes.push(first);
        }
        let mut found_any = !volumes.is_empty();
        for letter in b'r'..=b'z' {
            let mut any_in_run = false;
            for n in 0..100 {
                let vol = parent.join(format!("{base}.{}{:02}", letter as char, n));
                if let Some(vol) = index.resolve(&vol) {
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
        let part1 = parent.join(format!("{stem}.part1.rar"));
        if let Some(part1) = index.resolve(&part1)
            && part1 != path
        {
            return discover_volumes(&part1);
        }
        // Also probe zero-padded first volumes ({stem}.part01.rar ..
        // part0001.rar): sets written with 10+ volumes now carry the
        // padding themselves, and a caller may pass the base name.
        for width in 2..=4 {
            let probe = parent.join(format!("{stem}.part{:0width$}.rar", 1, width = width));
            if let Some(probe) = index.resolve(&probe)
                && probe != path
            {
                return discover_volumes(&probe);
            }
        }
    }

    vec![path.to_path_buf()]
}
