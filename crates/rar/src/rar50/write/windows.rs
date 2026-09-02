//! Windows-specific filesystem FFI: NTFS alternate data streams and
//! timestamps that std does not expose. Compiles to nothing on non-Windows
//! targets; callers gate on `#[cfg(windows)]`.

use std::path::Path;

use crate::error::{RarError, RarResult};

/// Write an NTFS alternate data stream (`path` + `stream_name` like
/// `:custom1`) on Windows.
#[cfg(windows)]
pub(crate) fn write_windows_stream(path: &Path, stream_name: &str, data: &[u8]) -> RarResult<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_ALWAYS, WriteFile,
    };
    let mut full: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    for unit in stream_name.encode_utf16() {
        full.insert(full.len() - 1, unit);
    }
    let handle = unsafe {
        CreateFileW(
            full.as_ptr(),
            0x4000_0000 | 0x8000_0000, // GENERIC_WRITE | GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(RarError::Io(std::io::Error::last_os_error()));
    }
    let mut written = 0u32;
    let ok = unsafe {
        WriteFile(
            handle,
            data.as_ptr() as *const _,
            data.len().min(u32::MAX as usize) as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return Err(RarError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Set a file's creation time (Windows only) via `SetFileTime`.
#[cfg(windows)]
pub(crate) fn windows_set_creation_time(path: &Path, secs: u64, ns: u32) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING, SetFileTime,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let ft_100ns = (secs + 11_644_473_600) * 10_000_000 + u64::from(ns) / 100;
    let creation = FILETIME {
        dwLowDateTime: (ft_100ns & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ft_100ns >> 32) as u32,
    };
    let ok = unsafe { SetFileTime(handle, &creation, std::ptr::null(), std::ptr::null()) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Read a Windows file timestamp via `GetFileTime` (std exposes no
/// access/creation-time reader). `want_access` selects the last-access
/// time, otherwise the creation time. Returns unix (seconds, ns).
#[cfg(windows)]
pub(crate) fn windows_file_time(path: &Path, want_access: bool) -> Option<(u64, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileTime, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut access = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut write = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let ok = unsafe { GetFileTime(handle, &mut creation, &mut access, &mut write) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    let ft = if want_access {
        ((access.dwHighDateTime as u64) << 32) | access.dwLowDateTime as u64
    } else {
        ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64
    };
    Some((
        (ft / 10_000_000).saturating_sub(11_644_473_600),
        ((ft % 10_000_000) * 100) as u32,
    ))
}

/// Enumerate the NTFS alternate data streams of `path` on Windows:
/// `(stream_name_with_leading_colon, size)` pairs.
#[cfg(windows)]
pub(crate) fn enumerate_windows_streams(path: &Path) -> RarResult<Vec<(String, u64)>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard, WIN32_FIND_STREAM_DATA,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut data = WIN32_FIND_STREAM_DATA {
        cStreamName: [0u16; 296],
        StreamSize: 0,
    };
    let handle = unsafe {
        FindFirstStreamW(
            wide.as_ptr(),
            FindStreamInfoStandard,
            &mut data as *mut _ as *mut core::ffi::c_void,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Ok(Vec::new()); // no streams (or not NTFS)
    }
    let mut out = Vec::new();
    loop {
        let mut len = 0usize;
        while len < data.cStreamName.len() && data.cStreamName[len] != 0 {
            len += 1;
        }
        let name = String::from_utf16_lossy(&data.cStreamName[..len]);
        if !name.is_empty() {
            out.push((name, data.StreamSize as u64));
        }
        let ok = unsafe { FindNextStreamW(handle, &mut data as *mut _ as *mut core::ffi::c_void) };
        if ok == 0 {
            break;
        }
    }
    unsafe { CloseHandle(handle) };
    Ok(out)
}
