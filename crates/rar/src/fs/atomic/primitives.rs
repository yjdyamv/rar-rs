//! Temp-sibling naming and single-file installation: unique temp names,
//! `create_new` staging, the platform `replace_file` variants and the fsync
//! ordering that makes an install durable.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{RarError, RarResult};

/// `std::process::id`, so derive uniqueness from the monotonic counter and
/// the system clock instead.
pub(crate) fn temp_suffix() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:x}{counter:x}")
}

/// Build a unique temporary sibling path for atomic extraction.
pub(crate) fn temp_sibling_path(dest_path: &Path) -> PathBuf {
    let file_name = dest_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entry".to_string());
    let tmp_name = format!(".{file_name}.rar5tmp-{}", temp_suffix());
    dest_path.with_file_name(tmp_name)
}

/// Create a new file for both reading and writing. Archive staging paths must
/// never follow or truncate a pre-existing file: callers generate a fresh
/// sibling name and receive `AlreadyExists` if it collides.
pub(crate) fn read_write_create(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

/// Copy exactly `limit` bytes from `reader` to `writer`.
pub(crate) fn copy_prefix(
    reader: &mut impl Read,
    writer: &mut impl Write,
    mut remaining: u64,
) -> io::Result<u64> {
    let mut buf = [0u8; 256 * 1024];
    let mut total = 0u64;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = reader.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source shrank while staging the write",
            ));
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
        remaining -= n as u64;
    }
    Ok(total)
}

/// Atomically replace `dest` with `src` without deleting `dest` first.
#[cfg(unix)]
pub(crate) fn replace_file(src: &Path, dest: &Path) -> RarResult<()> {
    fs::rename(src, dest).map_err(RarError::Io)
}

#[cfg(windows)]
pub(crate) fn replace_file(src: &Path, dest: &Path) -> RarResult<()> {
    use std::os::windows::ffi::OsStrExt;

    if !dest.exists() {
        return fs::rename(src, dest).map_err(RarError::Io);
    }
    let dest: Vec<u16> = dest.as_os_str().encode_wide().chain(Some(0)).collect();
    let src: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let replaced = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
            dest.as_ptr(),
            src.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        Err(RarError::Io(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

/// Replace `dest` with `src` on WASI (this crate's only non-unix,
/// non-windows target is `wasm32-wasip1-threads`).
///
/// WASI Preview 1 has no atomic "replace" primitive, yet every commit path
/// that reaches here replaces an *existing* archive (append/delete/comment
/// and recovery-record edits, overwrite-on-extract, repair in place).
///
/// Try a plain rename first: where the host rename already overwrites an
/// existing destination it stays atomic and no extra step runs — POSIX hosts
/// rename(2), and Node's WASI (uvwasi) maps `path_rename` to libuv's
/// `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING)` on Windows. Only when the
/// rename still fails and both files are present do we fall back to
/// delete-then-rename (the pre-hardening behavior), which is required by
/// WASI implementations whose rename refuses to overwrite (e.g. shims that
/// surface `EXIST`). The staged temp sibling `src` always exists at commit
/// time, so the `src.exists()` guard keeps the original destination intact
/// on unrelated failures (the data-loss edge fixed by the atomic hardening).
#[cfg(not(any(unix, windows)))]
pub(crate) fn replace_file(src: &Path, dest: &Path) -> RarResult<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) if dest.exists() && src.exists() => {
            fs::remove_file(dest)?;
            fs::rename(src, dest).map_err(RarError::Io)
        }
        Err(first) => Err(RarError::Io(first)),
    }
}

/// Flush a file's contents to stable storage. Staged files are synced before
/// they are renamed into place: otherwise a power loss can persist the rename
/// while the bytes it names are still only in the page cache.
pub(crate) fn sync_file(path: &Path) -> RarResult<()> {
    // A read-only handle is not enough on Windows (`FlushFileBuffers`
    // requires write access), and staged files are always writable.
    OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()
        .map_err(RarError::Io)
}

/// The directory holding `path`, with the empty parent that
/// `Path::parent` reports for a bare relative name (`Path::new("bare.rar")
/// .parent()` is `Some("")`, not `None`) normalized to `.`.
///
/// Staging, journaling and the Unix parent-directory fsync all need a real
/// directory: `File::open("")` fails with ENOENT, which used to roll a
/// bare-name multi-volume create back after every volume was staged (the
/// archive vanished and the command errored). Windows/WASI no-op the
/// directory fsync, so only Unix hosts observed it.
pub(crate) fn parent_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Flush a directory's entries after renames so the new names survive a
/// power loss. Windows has no portable directory handle to fsync with (and
/// its replace APIs order metadata), so this is a no-op off Unix.
#[cfg(unix)]
pub(super) fn sync_parent_dir(parent: &Path) -> RarResult<()> {
    File::open(parent_dir(parent))?
        .sync_all()
        .map_err(RarError::Io)
}

#[cfg(not(unix))]
pub(super) fn sync_parent_dir(_parent: &Path) -> RarResult<()> {
    Ok(())
}

/// Install `src` over `dest` durably: flush the staged bytes first, replace
/// the destination, then flush the parent directory entry (Unix), so a power
/// loss cannot persist the rename ahead of the bytes it names or lose the new
/// name. Used by the single-volume commit and the archive lock path.
pub(crate) fn install_durable(src: &Path, dest: &Path) -> RarResult<()> {
    sync_file(src)?;
    replace_file(src, dest)?;
    sync_parent_dir(&parent_dir(dest))
}
