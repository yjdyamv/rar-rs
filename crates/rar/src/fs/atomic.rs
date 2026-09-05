//! Small I/O helpers shared across the crate: bounded reads, atomic
//! temp-sibling staging and file replacement.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{RarError, RarResult};
/// Read until `buf` is full or EOF; returns the number of bytes read.
pub(crate) fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

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

#[cfg(test)]
mod tests {
    use super::{read_write_create, replace_file};

    #[test]
    fn staging_create_never_truncates_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stage.tmp");
        std::fs::write(&path, b"keep").unwrap();

        assert!(read_write_create(&path).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
    }

    #[test]
    fn failed_replace_preserves_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.tmp");
        let dest = dir.path().join("archive.rar");
        std::fs::write(&dest, b"original").unwrap();

        assert!(replace_file(&missing, &dest).is_err());
        assert_eq!(std::fs::read(dest).unwrap(), b"original");
    }

    #[test]
    fn replace_installs_the_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("stage.tmp");
        let dest = dir.path().join("archive.rar");
        std::fs::write(&src, b"replacement").unwrap();
        std::fs::write(&dest, b"original").unwrap();

        replace_file(&src, &dest).unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), b"replacement");
        assert!(!src.exists());
    }
}
