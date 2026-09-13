//! `-ol` / `-oh`: turn symbolic links and hard links into RAR5 redirect
//! members instead of storing their data.
//!
//! The first occurrence of a hard-link group is archived normally; every
//! later path with the same file identity becomes a type-4 redirect to the
//! first member's archive name. Symlinks become type-1 redirects holding
//! their target path.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use crate::info;
use crate::name_policy::Collected;

/// A redirect to append after the data members: `(name, type, target)`.
pub(crate) type LinkRedirect = (String, u64, String);

/// How `-oi` treats identical files.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdenticalMode {
    /// `-oi` / `-oi1`: store the first file, redirect the rest.
    Dedup,
    /// `-oi2`: like `Dedup`, but announce the groups before archiving.
    Announce,
    /// `-oi3`: list each group (size + names); no archive is created.
    List,
    /// `-oi4`: list the bare duplicate names (the first file is skipped).
    ListBare,
}

/// Parsed `-oi[0-4][:<minsize>]` switch.
pub(crate) struct IdenticalSpec {
    pub mode: IdenticalMode,
    /// Files smaller than this are never compared (default 64 KiB).
    pub min_size: u64,
}

/// Parse the normalized `--identical` value. `Ok(None)` means the switch
/// was absent or explicitly turned off (`-oi0` / `-oi-`).
pub(crate) fn parse_identical(spec: Option<&str>) -> Result<Option<IdenticalSpec>, String> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    let (mode_part, min_part) = match spec.split_once(':') {
        Some((mode, size)) => (mode, Some(size)),
        None => (spec, None),
    };
    let mode = match mode_part {
        "" | "1" => IdenticalMode::Dedup,
        "2" => IdenticalMode::Announce,
        "3" => IdenticalMode::List,
        "4" => IdenticalMode::ListBare,
        "0" | "-" => return Ok(None),
        other => return Err(format!("Unknown option: oi{other}")),
    };
    let min_size = match min_part {
        None | Some("") => 64 * 1024,
        Some(size) => parse_min_size(size)?,
    };
    Ok(Some(IdenticalSpec { mode, min_size }))
}

/// `-oi` size units: lowercase is binary (`k` = 1024), uppercase decimal
/// (`K` = 1000), matching Rar.txt.
fn parse_min_size(spec: &str) -> Result<u64, String> {
    let (digits, multiplier) = match spec.chars().last() {
        Some(unit) if unit.is_ascii_alphabetic() => {
            let multiplier = match unit {
                'b' | 'B' => 1,
                'k' => 1024,
                'K' => 1000,
                'm' => 1024 * 1024,
                'M' => 1_000_000,
                'g' => 1024 * 1024 * 1024,
                'G' => 1_000_000_000,
                't' => 1024u64.pow(4),
                'T' => 1_000_000_000_000,
                other => return Err(format!("Invalid -oi size unit: {other}")),
            };
            (&spec[..spec.len() - unit.len_utf8()], multiplier)
        }
        _ => (spec, 1),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("Invalid -oi size: {spec}"))?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("-oi size too large: {spec}"))
}

/// Groups of indices (into `collected`) whose files have identical contents,
/// in first-occurrence order; only groups with two or more members.
fn identical_groups(collected: &[Collected], min_size: u64) -> Vec<Vec<usize>> {
    let mut by_key: HashMap<(u64, u32), usize> = HashMap::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, c) in collected.iter().enumerate() {
        if c.is_dir {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&c.path) else {
            continue;
        };
        if meta.len() < min_size {
            continue;
        }
        let Ok(crc) = crc32_file(&c.path) else {
            continue;
        };
        match by_key.get(&(meta.len(), crc)) {
            Some(&group) => {
                // A CRC collision must not turn two different files into a
                // reference: compare the bytes before accepting the match.
                if files_equal(&collected[groups[group][0]].path, &c.path) {
                    groups[group].push(i);
                }
            }
            None => {
                by_key.insert((meta.len(), crc), groups.len());
                groups.push(vec![i]);
            }
        }
    }
    groups.retain(|group| group.len() > 1);
    groups
}

fn crc32_file(path: &Path) -> std::io::Result<u32> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&buf[..read]);
    }
}

fn files_equal(a: &Path, b: &Path) -> bool {
    let (Ok(mut left), Ok(mut right)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    let mut left_buf = [0u8; 64 * 1024];
    let mut right_buf = [0u8; 64 * 1024];
    loop {
        match (left.read(&mut left_buf), right.read(&mut right_buf)) {
            (Ok(0), Ok(0)) => return true,
            (Ok(n), Ok(m)) if n == m && left_buf[..n] == right_buf[..m] => {}
            _ => return false,
        }
    }
}

/// Turn every duplicate in an identical-file group into a type-5 "file
/// copy" redirect to the first member; `-oi2` announces the groups first.
pub(crate) fn apply_identical_redirects(
    collected: Vec<Collected>,
    spec: &IdenticalSpec,
) -> (Vec<Collected>, Vec<LinkRedirect>) {
    let groups = identical_groups(&collected, spec.min_size);
    if groups.is_empty() {
        return (collected, Vec::new());
    }
    if spec.mode == IdenticalMode::Announce {
        print_groups(&collected, &groups, false);
    }
    let mut copy = vec![false; collected.len()];
    let mut redirects = Vec::new();
    for group in &groups {
        let first = &collected[group[0]];
        for &i in &group[1..] {
            copy[i] = true;
            redirects.push((collected[i].name.clone(), 5, first.name.clone()));
        }
    }
    let kept = collected
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !copy[*i])
        .map(|(_, c)| c)
        .collect();
    (kept, redirects)
}

/// Print the identical groups for the listing modes (`-oi3` / `-oi4`).
pub(crate) fn print_identical_groups(collected: &[Collected], spec: &IdenticalSpec) {
    let groups = identical_groups(collected, spec.min_size);
    print_groups(collected, &groups, spec.mode == IdenticalMode::ListBare);
}

fn print_groups(collected: &[Collected], groups: &[Vec<usize>], bare: bool) {
    if bare {
        for group in groups {
            for &i in &group[1..] {
                info!("{}", collected[i].name);
            }
        }
        return;
    }
    for (g, group) in groups.iter().enumerate() {
        if g > 0 {
            info!("");
        }
        for &i in group {
            let size = std::fs::metadata(&collected[i].path)
                .map(|m| m.len())
                .unwrap_or(0);
            info!("{size:>12}  {}", collected[i].name);
        }
    }
    if !groups.is_empty() {
        let duplicates: usize = groups.iter().map(|g| g.len() - 1).sum();
        info!("{duplicates} found.");
    }
}

/// Split `collected` into data members plus link redirects.
///
/// `store_hardlinks` must already be gated on the target format: RAR4 has
/// no redirect records (`add_redirect` rejects them) and WinRAR's own
/// `-ma4 -oh` stores the files in full. `skip_links` (`-ol-`) drops
/// symbolic links entirely instead of storing or following them.
pub(crate) fn split_link_redirects(
    collected: Vec<Collected>,
    store_links: bool,
    store_hardlinks: bool,
    skip_links: bool,
) -> (Vec<Collected>, Vec<LinkRedirect>) {
    let mut redirects = Vec::new();
    let mut keep = Vec::with_capacity(collected.len());

    if store_links || skip_links {
        for c in collected {
            if c.is_dir {
                keep.push(c);
                continue;
            }
            match std::fs::symlink_metadata(&c.path) {
                Ok(m) if m.file_type().is_symlink() => {
                    if skip_links {
                        continue;
                    }
                    if let Ok(target) = std::fs::read_link(&c.path) {
                        redirects.push((c.name.clone(), 1, target.to_string_lossy().into_owned()));
                        continue;
                    }
                    keep.push(c);
                }
                _ => keep.push(c),
            }
        }
    } else {
        keep = collected;
    }

    if store_hardlinks {
        let mut seen: HashMap<(u64, u64), String> = HashMap::new();
        let mut data = Vec::with_capacity(keep.len());
        for c in keep {
            if c.is_dir {
                data.push(c);
                continue;
            }
            if let Some(id) = file_identity(&c.path) {
                if let Some(first) = seen.get(&id) {
                    redirects.push((c.name.clone(), 4, first.clone()));
                    continue;
                }
                seen.insert(id, c.name.clone());
            }
            data.push(c);
        }
        keep = data;
    }

    (keep, redirects)
}

/// File identity for hard-link detection: `(device, inode)` on Unix and
/// `(volume serial, file index)` on Windows. Other targets cannot detect
/// hard links and return `None`.
#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(windows)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    unsafe { CloseHandle(handle) };
    (ok != 0).then(|| {
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}
