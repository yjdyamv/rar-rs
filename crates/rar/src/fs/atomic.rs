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

/// Hidden sibling used to park a destination file during [`commit_files`].
fn backup_sibling_path(dest: &Path, suffix: &str) -> PathBuf {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entry".to_string());
    dest.with_file_name(format!(".{file_name}.rar5bak-{suffix}"))
}

/// Move `src` onto `dest`, replacing an existing destination. Only the
/// rollback path uses this, where discarding the newer bytes is the point,
/// so a plain rename plus a remove-retry is correct.
fn restore_file(src: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) if dest.exists() => {
            fs::remove_file(dest)?;
            fs::rename(src, dest)
        }
        Err(error) => Err(error),
    }
}

/// Commit a multi-file write as one unit.
///
/// `install` holds `(staged, final)` pairs; `retire` holds existing final
/// paths the new set does not overwrite (a previous, longer volume set).
/// Every pre-existing final is first parked on a hidden sibling, then the
/// staged files are moved onto their final names (the retired files stay
/// parked). A failure at any step rolls the whole set back: installed files
/// return to their staged names and every parked original returns to its
/// final name. The caller therefore observes either the complete new set or
/// the untouched old set, never a mix. On success the parked files (replaced
/// originals and retired extras) are deleted.
///
/// Staged files that were not installed are left in place for the caller's
/// cleanup. This is atomic with respect to *errors*; surviving a process kill
/// between renames still needs an on-disk journal.
pub(crate) fn commit_files(install: &[(PathBuf, PathBuf)], retire: &[PathBuf]) -> RarResult<()> {
    if install.is_empty() && retire.is_empty() {
        return Ok(());
    }
    let suffix = temp_suffix();
    // (parked path, final path) for everything that must be restored.
    let mut parked: Vec<(PathBuf, PathBuf)> = Vec::new();
    // (final path, staged path) for everything already installed.
    let mut installed: Vec<(PathBuf, PathBuf)> = Vec::new();

    let rollback = |parked: &[(PathBuf, PathBuf)], installed: &[(PathBuf, PathBuf)]| {
        for (final_path, staged) in installed.iter().rev() {
            let _ = restore_file(final_path, staged);
        }
        for (backup, final_path) in parked.iter().rev() {
            let _ = restore_file(backup, final_path);
        }
    };

    // Phase 1: park every pre-existing final (replaced or retired).
    for final_path in install
        .iter()
        .map(|(_, final_path)| final_path)
        .chain(retire.iter())
    {
        if !final_path.exists() {
            continue;
        }
        let backup = backup_sibling_path(final_path, &suffix);
        if let Err(error) = fs::rename(final_path, &backup) {
            rollback(&parked, &installed);
            return Err(RarError::Io(error));
        }
        parked.push((backup, final_path.clone()));
    }

    // Phase 2: install the staged files.
    for (staged, final_path) in install {
        if let Err(error) = replace_file(staged, final_path) {
            rollback(&parked, &installed);
            return Err(error);
        }
        installed.push((final_path.clone(), staged.clone()));
    }

    // Phase 3: success — the parked originals and retired extras are garbage.
    for (backup, _) in &parked {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{commit_files, read_write_create, replace_file};

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
    fn commit_files_restores_the_old_set_when_a_later_install_fails() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("set.part1.rar");
        let second = dir.path().join("set.part2.rar");
        std::fs::write(&first, b"old-1").unwrap();
        std::fs::write(&second, b"old-2").unwrap();
        let staged_first = dir.path().join(".stage-1");
        std::fs::write(&staged_first, b"new-1").unwrap();
        // The second staged file is deliberately missing so the install
        // fails after the first file already landed.
        let staged_second = dir.path().join(".stage-2");

        let install = vec![
            (staged_first.clone(), first.clone()),
            (staged_second, second.clone()),
        ];
        assert!(commit_files(&install, &[]).is_err());

        assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
        assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
        // The installed file went back to its staged name for the caller's
        // cleanup instead of being lost or left at the final path.
        assert_eq!(std::fs::read(&staged_first).unwrap(), b"new-1");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5bak"))
            .collect();
        assert!(leftovers.is_empty(), "backup leftovers: {leftovers:?}");
    }

    #[test]
    fn commit_files_replaces_and_retires_a_set() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("set.part1.rar");
        let extra = dir.path().join("set.part2.rar");
        std::fs::write(&keep, b"old-1").unwrap();
        std::fs::write(&extra, b"old-2").unwrap();
        let staged = dir.path().join(".stage-1");
        std::fs::write(&staged, b"new-1").unwrap();

        commit_files(&[(staged, keep.clone())], std::slice::from_ref(&extra)).unwrap();
        assert_eq!(std::fs::read(&keep).unwrap(), b"new-1");
        assert!(!extra.exists());
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
