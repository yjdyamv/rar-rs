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

/// Journal file recording an in-flight multi-file commit, next to the archive
/// being replaced. One per base name: writes to the same archive are
/// serialized by the caller.
fn journal_path(parent: &Path, base: &str) -> PathBuf {
    parent.join(format!(".{base}.rar5commit.journal"))
}

/// Marker written after every staged file is installed, before the backups
/// are dropped. Its presence tells recovery the new set won.
fn commit_done_path(parent: &Path, base: &str) -> PathBuf {
    parent.join(format!(".{base}.rar5commit.done"))
}

/// Serialize the commit plan: one `backup`/`install` record per file, names
/// relative to `parent`. Written before anything moves, via a sibling rename
/// so a torn write cannot leave a half journal for recovery to misread.
fn write_commit_journal(
    parent: &Path,
    base: &str,
    backups: &[(PathBuf, PathBuf)],
    install: &[(PathBuf, PathBuf)],
) -> RarResult<()> {
    let mut text = String::from("rar5commit v1\n");
    let mut push = |kind: &str, from: &Path, to: &Path| {
        if let (Some(from), Some(to)) = (from.file_name(), to.file_name()) {
            text.push_str(kind);
            text.push('\t');
            text.push_str(&from.to_string_lossy());
            text.push('\t');
            text.push_str(&to.to_string_lossy());
            text.push('\n');
        }
    };
    for (backup, final_path) in backups {
        push("backup", backup, final_path);
    }
    for (staged, final_path) in install {
        push("install", staged, final_path);
    }
    let path = journal_path(parent, base);
    let tmp = path.with_extension("journal.tmp");
    fs::write(&tmp, text.as_bytes())?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Roll back or finish an interrupted multi-file commit, if a journal is
/// present next to `base`.
///
/// Called before a write stages new output, so a process killed mid-commit
/// never leaves a mixed volume set behind. With the done marker present the
/// new set won and the parked originals are dropped; without it the commit had
/// not finished, so newly installed files are removed, parked originals are
/// restored, and leftover staged files are discarded. A corrupt journal is
/// left untouched (recovery never guesses).
pub(crate) fn recover_interrupted_commit(parent: &Path, base: &str) -> RarResult<()> {
    let journal = journal_path(parent, base);
    // A crash between writing and renaming the journal leaves only this.
    let _ = fs::remove_file(journal.with_extension("journal.tmp"));
    let Ok(text) = fs::read_to_string(&journal) else {
        return Ok(());
    };
    if !text.starts_with("rar5commit v1") {
        return Ok(());
    }
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut installs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for line in text.lines().skip(1) {
        let mut fields = line.split('\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some("backup"), Some(from), Some(to)) => {
                backups.push((parent.join(from), parent.join(to)));
            }
            (Some("install"), Some(from), Some(to)) => {
                installs.push((parent.join(from), parent.join(to)));
            }
            _ => {}
        }
    }
    if commit_done_path(parent, base).exists() {
        for (backup, _) in &backups {
            let _ = fs::remove_file(backup);
        }
    } else {
        for (_, final_path) in &installs {
            let _ = fs::remove_file(final_path);
        }
        for (backup, final_path) in &backups {
            let _ = restore_file(backup, final_path);
        }
        for (staged, _) in &installs {
            let _ = fs::remove_file(staged);
        }
    }
    let _ = fs::remove_file(&journal);
    let _ = fs::remove_file(commit_done_path(parent, base));
    Ok(())
}

/// Commit a multi-file write as one unit, journaled so a process kill between
/// the renames is recoverable (see [`recover_interrupted_commit`]).
///
/// `install` holds `(staged, final)` pairs; `retire` holds existing final
/// paths the new set does not overwrite (a previous, longer volume set, or
/// stale `.rev` files). Every pre-existing final is first parked on a hidden
/// sibling, then the staged files are moved onto their final names (the
/// retired files stay parked). A failure at any step rolls the whole set back:
/// installed files return to their staged names and every parked original
/// returns to its final name. The caller therefore observes either the
/// complete new set or the untouched old set, never a mix. On success the
/// parked files (replaced originals and retired extras) are deleted.
///
/// Staged files that were not installed are left in place for the caller's
/// cleanup. A process kill is covered by the journal, not by this function.
pub(crate) fn commit_files(
    parent: &Path,
    base: &str,
    install: &[(PathBuf, PathBuf)],
    retire: &[PathBuf],
) -> RarResult<()> {
    if install.is_empty() && retire.is_empty() {
        return Ok(());
    }
    let suffix = temp_suffix();
    // Plan the backups up front so the journal can name them before any file
    // moves.
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    for final_path in install
        .iter()
        .map(|(_, final_path)| final_path)
        .chain(retire.iter())
    {
        if final_path.exists() {
            backups.push((backup_sibling_path(final_path, &suffix), final_path.clone()));
        }
    }
    write_commit_journal(parent, base, &backups, install)?;

    // (final path, staged path) for everything already installed.
    let mut installed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let rollback = |backups: &[(PathBuf, PathBuf)], installed: &[(PathBuf, PathBuf)]| {
        for (final_path, staged) in installed.iter().rev() {
            let _ = restore_file(final_path, staged);
        }
        for (backup, final_path) in backups.iter().rev() {
            let _ = restore_file(backup, final_path);
        }
    };

    // Phase 1: park every pre-existing final (replaced or retired).
    for (backup, final_path) in &backups {
        if let Err(error) = fs::rename(final_path, backup) {
            rollback(&backups, &installed);
            let _ = fs::remove_file(journal_path(parent, base));
            return Err(RarError::Io(error));
        }
    }

    // Phase 2: install the staged files.
    for (staged, final_path) in install {
        if let Err(error) = replace_file(staged, final_path) {
            rollback(&backups, &installed);
            let _ = fs::remove_file(journal_path(parent, base));
            return Err(error);
        }
        installed.push((final_path.clone(), staged.clone()));
    }

    // Phase 3: mark committed, then drop the parked originals.
    let _ = fs::write(commit_done_path(parent, base), b"");
    for (backup, _) in &backups {
        let _ = fs::remove_file(backup);
    }
    let _ = fs::remove_file(journal_path(parent, base));
    let _ = fs::remove_file(commit_done_path(parent, base));
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
        assert!(commit_files(dir.path(), "set", &install, &[]).is_err());

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

        commit_files(
            dir.path(),
            "set",
            &[(staged, keep.clone())],
            std::slice::from_ref(&extra),
        )
        .unwrap();
        assert_eq!(std::fs::read(&keep).unwrap(), b"new-1");
        assert!(!extra.exists());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5commit") || name.contains("rar5bak"))
            .collect();
        assert!(leftovers.is_empty(), "commit leftovers: {leftovers:?}");
    }

    #[test]
    fn recovery_rolls_back_a_prepared_commit() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let final_path = parent.join("set.part1.rar");
        // The old file was parked and the new one installed, then the writer
        // was killed before the done marker was written.
        let backup = parent.join(".set.part1.rar.rar5bak-x");
        let staged = parent.join(".set.part1.rar.rar5tmp-x.part1.rar");
        std::fs::write(&backup, b"old").unwrap();
        std::fs::write(&final_path, b"new").unwrap();
        std::fs::write(
            super::journal_path(parent, "set"),
            format!(
                "rar5commit v1\nbackup\t{}\t{}\ninstall\t{}\t{}\n",
                backup.file_name().unwrap().to_string_lossy(),
                final_path.file_name().unwrap().to_string_lossy(),
                staged.file_name().unwrap().to_string_lossy(),
                final_path.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        super::recover_interrupted_commit(parent, "set").unwrap();

        assert_eq!(std::fs::read(&final_path).unwrap(), b"old");
        assert!(!backup.exists());
        assert!(!super::journal_path(parent, "set").exists());
    }

    #[test]
    fn recovery_keeps_the_new_set_when_committed() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let final_path = parent.join("set.part1.rar");
        let backup = parent.join(".set.part1.rar.rar5bak-y");
        let staged = parent.join(".set.part1.rar.rar5tmp-y.part1.rar");
        std::fs::write(&backup, b"old").unwrap();
        std::fs::write(&final_path, b"new").unwrap();
        std::fs::write(
            super::journal_path(parent, "set"),
            format!(
                "rar5commit v1\nbackup\t{}\t{}\ninstall\t{}\t{}\n",
                backup.file_name().unwrap().to_string_lossy(),
                final_path.file_name().unwrap().to_string_lossy(),
                staged.file_name().unwrap().to_string_lossy(),
                final_path.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();
        std::fs::write(super::commit_done_path(parent, "set"), b"").unwrap();

        super::recover_interrupted_commit(parent, "set").unwrap();

        assert_eq!(std::fs::read(&final_path).unwrap(), b"new");
        assert!(!backup.exists());
        assert!(!super::commit_done_path(parent, "set").exists());
        assert!(!super::journal_path(parent, "set").exists());
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
