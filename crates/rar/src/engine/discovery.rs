//! Multi-volume archive discovery.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Case-insensitive view of one directory's entries.
///
/// Foreign sets may spell their volume names in upper case (`MULTIVOL.RAR` +
/// `MULTIVOL.R00`, as the DOS-era rars fixtures do) while every probe below
/// is lower case. Windows folds case for free; a case-sensitive filesystem
/// does not, and the set then looks like a single truncated volume. The
/// index is consulted only when the exact probe is missing, and returns the
/// real on-disk spelling.
///
/// On Unix the keys are the raw name bytes, so a non-UTF-8 volume name can
/// be indexed and matched too.
struct SiblingIndex {
    #[cfg(unix)]
    names: HashMap<Vec<u8>, PathBuf>,
    #[cfg(not(unix))]
    names: HashMap<String, PathBuf>,
}

impl SiblingIndex {
    fn new(dir: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut names = HashMap::new();
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    // First spelling wins on a case collision (possible on a
                    // case-sensitive filesystem).
                    names
                        .entry(entry.file_name().as_bytes().to_ascii_lowercase())
                        .or_insert_with(|| entry.path());
                }
            }
            Self { names }
        }
        #[cfg(not(unix))]
        {
            let mut names = HashMap::new();
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        names
                            .entry(name.to_ascii_lowercase())
                            .or_insert_with(|| entry.path());
                    }
                }
            }
            Self { names }
        }
    }

    /// The real path for `candidate` (always a direct child of the indexed
    /// directory) when it exists under either spelling.
    fn resolve(&self, candidate: &Path) -> Option<PathBuf> {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let key = candidate.file_name()?.as_bytes().to_ascii_lowercase();
            self.names.get(&key).cloned()
        }
        #[cfg(not(unix))]
        {
            let name = candidate.file_name()?.to_str()?;
            self.names.get(&name.to_ascii_lowercase()).cloned()
        }
    }
}

/// Append an ASCII suffix to a name without a UTF-8 round trip: on Unix the
/// raw base bytes are kept, elsewhere the lossy spelling is used (Windows
/// UTF-16 names are always representable).
fn with_name_suffix(base: &OsStr, suffix: &str) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut name = base.as_bytes().to_vec();
        name.extend_from_slice(suffix.as_bytes());
        OsString::from_vec(name)
    }
    #[cfg(not(unix))]
    {
        let mut name = base.to_string_lossy().into_owned();
        name.push_str(suffix);
        OsString::from(name)
    }
}

/// Volume base and digit width of a `{base}.partN.rar` (or `.rev`) name.
/// On Unix the raw bytes are parsed, so a non-UTF-8 base survives; elsewhere
/// the existing `&str` parser runs on the lossy spelling.
#[cfg(unix)]
fn part_volume_base(name: &OsStr) -> Option<(OsString, usize)> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    const PART: &[u8] = b".part";
    let bytes = name.as_bytes();
    let stem = strip_archive_extension_bytes(bytes)?;
    let lower = stem.to_ascii_lowercase();
    let position = lower
        .windows(PART.len())
        .rposition(|window| window == PART)?;
    let base = &stem[..position];
    let tail = &stem[position + PART.len()..];
    if base.is_empty() || tail.is_empty() || !tail.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((OsString::from_vec(base.to_vec()), tail.len()))
}

#[cfg(not(unix))]
fn part_volume_base(name: &OsStr) -> Option<(OsString, usize)> {
    crate::fs::volume::extract_volume_base(&name.to_string_lossy())
        .map(|(base, width)| (OsString::from(base), width))
}

/// Strip a trailing `.rar` or `.rev` extension (ASCII case-insensitive) from
/// raw bytes.
#[cfg(unix)]
fn strip_archive_extension_bytes(name: &[u8]) -> Option<&[u8]> {
    let stem_len = name.len().checked_sub(4)?;
    let extension = &name[stem_len..];
    (extension.eq_ignore_ascii_case(b".rar") || extension.eq_ignore_ascii_case(b".rev"))
        .then(|| &name[..stem_len])
}

/// Legacy volume base (`x.rar` / `x.r00` / `x.s37`) from raw Unix bytes.
#[cfg(unix)]
fn legacy_volume_base_os(name: &OsStr) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bytes = name.as_bytes();
    let lower = bytes.to_ascii_lowercase();
    if let Some(base) = lower.strip_suffix(b".rar") {
        return Some(OsString::from_vec(bytes[..base.len()].to_vec()));
    }
    if lower.len() >= 5
        && lower[lower.len() - 4] == b'.'
        && lower[lower.len() - 3].is_ascii_lowercase()
        && lower[lower.len() - 3] >= b'r'
        && lower[lower.len() - 3] <= b'z'
        && lower[lower.len() - 2].is_ascii_digit()
        && lower[lower.len() - 1].is_ascii_digit()
    {
        let end = lower.len() - 4;
        return Some(OsString::from_vec(bytes[..end].to_vec()));
    }
    None
}

#[cfg(not(unix))]
fn legacy_volume_base_os(name: &OsStr) -> Option<OsString> {
    crate::fs::volume::legacy_volume_base(&name.to_string_lossy()).map(OsString::from)
}

/// A `{base}.partN.rar` candidate, zero-padded to the set's digit width.
fn part_volume_path(parent: &Path, base: &OsStr, number: u64, width: usize) -> PathBuf {
    let suffix = if width > 1 {
        format!(".part{number:0width$}.rar", width = width)
    } else {
        format!(".part{number}.rar")
    };
    parent.join(with_name_suffix(base, &suffix))
}

/// A legacy `{base}.{letter}{NN}` candidate (`.rar` is the first volume).
fn legacy_volume_path(parent: &Path, base: &OsStr, letter: u8, number: u8) -> PathBuf {
    parent.join(with_name_suffix(
        base,
        &format!(".{}{:02}", letter as char, number),
    ))
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
/// filesystems too, and on Unix they are matched as raw bytes so a base
/// that is not valid UTF-8 is discovered.
pub fn discover_volumes(path: &Path) -> Vec<PathBuf> {
    let Some(file_name) = path.file_name() else {
        return vec![path.to_path_buf()];
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
    if let Some((base, width)) = part_volume_base(file_name) {
        let mut volumes = Vec::new();
        let mut n = 1u64;
        loop {
            let vol = part_volume_path(parent, &base, n, width);
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
            let vol = part_volume_path(parent, &base, n, 1);
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
    if let Some(base) = legacy_volume_base_os(file_name) {
        let mut volumes = Vec::new();
        if let Some(first) = index.resolve(&parent.join(with_name_suffix(&base, ".rar"))) {
            volumes.push(first);
        }
        let mut found_any = !volumes.is_empty();
        for letter in b'r'..=b'z' {
            let mut any_in_run = false;
            for n in 0..100 {
                let vol = legacy_volume_path(parent, &base, letter, n);
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
    if let Some(stem) = path.file_stem() {
        let part1 = parent.join(with_name_suffix(stem, ".part1.rar"));
        if let Some(part1) = index.resolve(&part1)
            && part1 != path
        {
            return discover_volumes(&part1);
        }
        // Also probe zero-padded first volumes ({stem}.part01.rar ..
        // part0001.rar): sets written with 10+ volumes now carry the
        // padding themselves, and a caller may pass the base name.
        for width in 2..=4 {
            let suffix = format!(".part{:0width$}.rar", 1, width = width);
            let probe = parent.join(with_name_suffix(stem, &suffix));
            if let Some(probe) = index.resolve(&probe)
                && probe != path
            {
                return discover_volumes(&probe);
            }
        }
    }

    vec![path.to_path_buf()]
}
