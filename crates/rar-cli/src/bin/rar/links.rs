//! `-ol` / `-oh`: turn symbolic links and hard links into RAR5 redirect
//! members instead of storing their data.
//!
//! The first occurrence of a hard-link group is archived normally; every
//! later path with the same file identity becomes a type-4 redirect to the
//! first member's archive name. Symlinks become type-1 redirects holding
//! their target path.

use std::collections::HashMap;
use std::path::Path;

use crate::name_policy::Collected;

/// A redirect to append after the data members: `(name, type, target)`.
pub(crate) type LinkRedirect = (String, u64, String);

/// Split `collected` into data members plus link redirects.
///
/// `store_hardlinks` must already be gated on the target format: RAR4 has
/// no redirect records (`add_redirect` rejects them) and WinRAR's own
/// `-ma4 -oh` stores the files in full.
pub(crate) fn split_link_redirects(
    collected: Vec<Collected>,
    store_links: bool,
    store_hardlinks: bool,
) -> (Vec<Collected>, Vec<LinkRedirect>) {
    let mut redirects = Vec::new();
    let mut keep = Vec::with_capacity(collected.len());

    if store_links {
        for c in collected {
            if c.is_dir {
                keep.push(c);
                continue;
            }
            match std::fs::symlink_metadata(&c.path) {
                Ok(m) if m.file_type().is_symlink() => {
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
