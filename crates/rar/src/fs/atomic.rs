//! Small I/O helpers shared across the crate: bounded reads, atomic
//! temp-sibling staging and file replacement.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

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
fn sync_parent_dir(parent: &Path) -> RarResult<()> {
    File::open(parent_dir(parent))?
        .sync_all()
        .map_err(RarError::Io)
}

#[cfg(not(unix))]
fn sync_parent_dir(_parent: &Path) -> RarResult<()> {
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

/// Journal format for the escaped (current) layout. Versioned so a foreign
/// or truncated journal is never misread as a commit plan.
const COMMIT_JOURNAL_VERSION: &str = "rar5commit v2";
/// Header of the unescaped format written by earlier builds; still parsed
/// (fields taken verbatim) so an in-flight journal survives an upgrade.
const COMMIT_JOURNAL_VERSION_V1: &str = "rar5commit v1";

/// Escape one journal field so the tab-separated, one-record-per-line format
/// stays unambiguous: `\`, `\t` and `\n` get a backslash form, and every
/// other control character is written as `\r` or `\xNN`. Control characters
/// are legal on Unix, so they are escaped rather than rejected (rejecting
/// them here failed a whole multi-volume commit after staging).
fn escape_journal_field(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                out.push_str("\\x");
                out.push(HEX[((c as u32 >> 4) & 0xF) as usize] as char);
                out.push(HEX[(c as u32 & 0xF) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out
}

/// Inverse of [`escape_journal_field`]; `None` marks a malformed field (an
/// unknown escape, a truncated `\xNN`, or a raw control character).
fn unescape_journal_field(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next()? {
                '\\' => out.push('\\'),
                't' => out.push('\t'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                'x' => {
                    let hi = chars.next()?.to_digit(16)?;
                    let lo = chars.next()?.to_digit(16)?;
                    out.push(char::from_u32(hi * 16 + lo)?);
                }
                _ => return None,
            },
            c if c.is_control() => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// A journal record may only name a plain sibling of the journal's
/// directory. Reject separators, `.`/`..`, NUL and Windows drive-relative
/// names (`C:evil`), all of which would make `parent.join(name)` leave
/// `parent` — a planted journal must never become a file primitive outside
/// the archive's own directory. A backslash is a legal filename character on
/// Unix (v2 escapes it, so records stay unambiguous) and only Windows treats
/// it as a separator.
fn plain_journal_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.contains('\0') {
        return false;
    }
    if name.contains('/') {
        return false;
    }
    #[cfg(windows)]
    if name.contains('\\') {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(part)), None) if part == OsStr::new(name)
    )
}

/// Serialize the commit plan: one `backup`/`install` record per file, names
/// relative to `parent` and escaped. Written before anything moves, via a
/// sibling rename so a torn write cannot leave a half journal for recovery
/// to misread.
fn write_commit_journal(
    parent: &Path,
    base: &str,
    backups: &[(PathBuf, PathBuf)],
    install: &[(PathBuf, PathBuf)],
) -> RarResult<()> {
    let mut text = String::from(COMMIT_JOURNAL_VERSION);
    text.push('\n');
    let mut push = |kind: &str, from: &Path, to: &Path| {
        if let (Some(from), Some(to)) = (from.file_name(), to.file_name()) {
            let from = escape_journal_field(&from.to_string_lossy());
            let to = escape_journal_field(&to.to_string_lossy());
            text.push_str(kind);
            text.push('\t');
            text.push_str(&from);
            text.push('\t');
            text.push_str(&to);
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
/// restored, and leftover staged files are discarded. A final is only removed
/// when it either had no pre-existing original (no backup record) or that
/// original was actually parked (the backup file exists): a kill between the
/// parks leaves later finals untouched *and* unbacked, and deleting them would
/// destroy the old set. Malformed records (unknown escapes, wrong field
/// counts, names that are not plain siblings) and unknown record kinds are
/// skipped and keep the journal in place, so nothing skipped is silently
/// forgotten; a journal with an unknown version header is left untouched
/// (recovery never guesses).
pub(crate) fn recover_interrupted_commit(parent: &Path, base: &str) -> RarResult<()> {
    let journal = journal_path(parent, base);
    // A crash between writing and renaming the journal leaves only this.
    let _ = fs::remove_file(journal.with_extension("journal.tmp"));
    let Ok(text) = fs::read_to_string(&journal) else {
        return Ok(());
    };
    let mut lines = text.lines();
    let escaped = match lines.next() {
        Some(COMMIT_JOURNAL_VERSION) => true,
        Some(COMMIT_JOURNAL_VERSION_V1) => false,
        _ => return Ok(()),
    };
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut installs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut malformed = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        // Exactly three fields: a raw tab or newline in a name makes the
        // record malformed, and it is skipped rather than guessed at.
        let (Some(kind), Some(from), Some(to), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            malformed = true;
            continue;
        };
        let (from, to) = if escaped {
            (unescape_journal_field(from), unescape_journal_field(to))
        } else {
            (Some(from.to_owned()), Some(to.to_owned()))
        };
        let (Some(from), Some(to)) = (from, to) else {
            malformed = true;
            continue;
        };
        if !plain_journal_name(&from) || !plain_journal_name(&to) {
            malformed = true;
            continue;
        }
        match kind {
            "backup" => backups.push((parent.join(from), parent.join(to))),
            "install" => installs.push((parent.join(from), parent.join(to))),
            _ => malformed = true,
        }
    }
    if commit_done_path(parent, base).exists() {
        for (backup, _) in &backups {
            let _ = fs::remove_file(backup);
        }
    } else {
        for (_, final_path) in &installs {
            let has_backup_record = backups.iter().any(|(_, final_)| final_ == final_path);
            let parked = backups
                .iter()
                .any(|(backup, final_)| final_ == final_path && backup.exists());
            if !has_backup_record || parked {
                let _ = fs::remove_file(final_path);
            }
        }
        for (backup, final_path) in &backups {
            if backup.exists() {
                let _ = restore_file(backup, final_path);
            }
        }
        for (staged, _) in &installs {
            let _ = fs::remove_file(staged);
        }
    }
    if !malformed {
        let _ = fs::remove_file(&journal);
        let _ = fs::remove_file(commit_done_path(parent, base));
    }
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
/// Staged bytes are flushed before each install and the parent directory
/// entry is flushed after the swap (Unix), so a power loss cannot persist a
/// rename without its contents.
pub(crate) fn commit_files(
    parent: &Path,
    base: &str,
    install: &[(PathBuf, PathBuf)],
    retire: &[PathBuf],
) -> RarResult<()> {
    if install.is_empty() && retire.is_empty() {
        return Ok(());
    }
    // A bare relative archive name has an empty (not absent) parent; treat
    // it as the current directory so the journal and the parent fsync land
    // in a real directory.
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
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
            // A missing backup means this final was never parked (the kill or
            // failure happened before its phase-1 rename): leave the original
            // untouched instead of treating the missing backup as "remove
            // whatever is there".
            if backup.exists() {
                let _ = restore_file(backup, final_path);
            }
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

    // Phase 2: install the staged files. Each staged file is flushed first:
    // the rename must not be able to become durable ahead of the bytes it
    // names.
    for (staged, final_path) in install {
        if let Err(error) = sync_file(staged).and_then(|()| replace_file(staged, final_path)) {
            rollback(&backups, &installed);
            let _ = fs::remove_file(journal_path(parent, base));
            return Err(error);
        }
        installed.push((final_path.clone(), staged.clone()));
    }

    // Make the swap durable before dropping the parked originals: the
    // journal, the parks and the installs all renamed entries in `parent`.
    if let Err(error) = sync_parent_dir(parent) {
        rollback(&backups, &installed);
        let _ = fs::remove_file(journal_path(parent, base));
        return Err(error);
    }

    // Phase 3: mark committed, then drop the parked originals. The marker
    // must be durable before any backup goes away: if a power loss persisted
    // the backup removals while losing the marker, recovery would see "not
    // done", delete the new installs, and have no originals left to restore.
    let marker = commit_done_path(parent, base);
    let marked = fs::write(&marker, b"")
        .map_err(RarError::Io)
        .and_then(|()| sync_file(&marker))
        .and_then(|()| sync_parent_dir(parent));
    // If the marker did not reach stable storage, keep the journal and the
    // backups so recovery rolls the old set back (there is no durable "done"
    // evidence), and report the failure.
    marked?;
    for (backup, _) in &backups {
        let _ = fs::remove_file(backup);
    }
    let _ = fs::remove_file(journal_path(parent, base));
    let _ = fs::remove_file(&marker);
    // Flush the removals as a batch: either the journal and marker removal
    // both persist (recovery is a no-op) or neither does (the durable marker
    // sends recovery down the "committed" path), never marker-gone while the
    // journal still names the removed backups.
    let _ = sync_parent_dir(parent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{commit_files, install_durable, read_write_create, replace_file};
    use std::path::Path;

    /// `Path::new("bare.rar").parent()` is `Some("")`: a bare relative
    /// archive name must still yield a real directory to journal and fsync
    /// in, or the Unix multi-volume commit rolls back after staging.
    #[test]
    fn parent_dir_normalizes_an_empty_parent_to_the_current_directory() {
        assert_eq!(super::parent_dir(Path::new("bare.rar")), Path::new("."));
        assert_eq!(super::parent_dir(Path::new("./bare.rar")), Path::new("."));
        assert_eq!(super::parent_dir(Path::new("dir")), Path::new("."));
        assert_eq!(
            super::parent_dir(Path::new("sub/bare.rar")),
            Path::new("sub")
        );
    }

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

    /// A kill between the phase-1 parks leaves some finals parked (backup
    /// file present) and later ones untouched (backup file missing even
    /// though the journal planned one). Recovery must never delete the
    /// untouched final: it is the only copy of the old data.
    #[test]
    fn recovery_leaves_an_unparked_final_alone_when_the_park_was_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let name =
            |path: &std::path::Path| path.file_name().unwrap().to_string_lossy().into_owned();
        let first = parent.join("set.part1.rar");
        let second = parent.join("set.part2.rar");
        let first_backup = parent.join(".set.part1.rar.rar5bak-x");
        // The second park never ran, so this planned backup does not exist.
        let planned_second_backup = parent.join(".set.part2.rar.rar5bak-x");
        let first_staged = parent.join(".set.part1.rar.rar5tmp-x");
        let second_staged = parent.join(".set.part2.rar.rar5tmp-x");
        std::fs::write(&first_backup, b"old-1").unwrap();
        std::fs::write(&second, b"old-2").unwrap();
        std::fs::write(&first_staged, b"new-1").unwrap();
        std::fs::write(&second_staged, b"new-2").unwrap();
        std::fs::write(
            super::journal_path(parent, "set"),
            format!(
                "rar5commit v1\n\
                 backup\t{}\t{}\n\
                 backup\t{}\t{}\n\
                 install\t{}\t{}\n\
                 install\t{}\t{}\n",
                name(&first_backup),
                name(&first),
                name(&planned_second_backup),
                name(&second),
                name(&first_staged),
                name(&first),
                name(&second_staged),
                name(&second),
            ),
        )
        .unwrap();

        super::recover_interrupted_commit(parent, "set").unwrap();

        // The parked original came back; the untouched final stayed put.
        assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
        assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
        assert!(!first_backup.exists());
        assert!(!first_staged.exists());
        assert!(!second_staged.exists());
        assert!(!super::journal_path(parent, "set").exists());
    }

    /// A kill after all parks but before any install: every final is
    /// missing and every backup exists, so recovery restores the complete
    /// old set and discards the staged files.
    #[test]
    fn recovery_restores_every_parked_final_when_no_install_ran() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let name =
            |path: &std::path::Path| path.file_name().unwrap().to_string_lossy().into_owned();
        let first = parent.join("set.part1.rar");
        let second = parent.join("set.part2.rar");
        let first_backup = parent.join(".set.part1.rar.rar5bak-x");
        let second_backup = parent.join(".set.part2.rar.rar5bak-x");
        let first_staged = parent.join(".set.part1.rar.rar5tmp-x");
        let second_staged = parent.join(".set.part2.rar.rar5tmp-x");
        std::fs::write(&first_backup, b"old-1").unwrap();
        std::fs::write(&second_backup, b"old-2").unwrap();
        std::fs::write(&first_staged, b"new-1").unwrap();
        std::fs::write(&second_staged, b"new-2").unwrap();
        std::fs::write(
            super::journal_path(parent, "set"),
            format!(
                "rar5commit v1\n\
                 backup\t{}\t{}\n\
                 backup\t{}\t{}\n\
                 install\t{}\t{}\n\
                 install\t{}\t{}\n",
                name(&first_backup),
                name(&first),
                name(&second_backup),
                name(&second),
                name(&first_staged),
                name(&first),
                name(&second_staged),
                name(&second),
            ),
        )
        .unwrap();

        super::recover_interrupted_commit(parent, "set").unwrap();

        assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
        assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
        assert!(!first_backup.exists());
        assert!(!second_backup.exists());
        assert!(!first_staged.exists());
        assert!(!second_staged.exists());
        assert!(!super::journal_path(parent, "set").exists());
    }

    /// A file installed where nothing pre-existed (no backup record) is the
    /// new set's own bytes and must still be removed on rollback.
    #[test]
    fn recovery_removes_an_installed_final_with_no_parked_original() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let final_path = parent.join("set.part1.rar");
        let staged = parent.join(".set.part1.rar.rar5tmp-x");
        std::fs::write(&final_path, b"new").unwrap();
        std::fs::write(
            super::journal_path(parent, "set"),
            format!(
                "rar5commit v1\ninstall\t{}\t{}\n",
                staged.file_name().unwrap().to_string_lossy(),
                final_path.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        super::recover_interrupted_commit(parent, "set").unwrap();

        assert!(!final_path.exists(), "a fresh install must be rolled back");
        assert!(!staged.exists());
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

    #[test]
    fn install_durable_replaces_the_destination_and_consumes_the_stage() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("stage.tmp");
        let dest = dir.path().join("archive.rar");
        std::fs::write(&src, b"durable").unwrap();
        std::fs::write(&dest, b"original").unwrap();

        install_durable(&src, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"durable");
        assert!(!src.exists());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5tmp") || name.contains("rar5bak"))
            .collect();
        assert!(leftovers.is_empty(), "install leftovers: {leftovers:?}");
    }

    #[test]
    fn journal_field_escaping_round_trips_controls() {
        for name in [
            "plain",
            "tab\there",
            "line\nbreak",
            "carriage\rreturn",
            "back\\slash",
            "controls\x01\x1f\x7f",
            "all\t\n\\of them\r\x01",
        ] {
            let escaped = super::escape_journal_field(name);
            assert!(
                !escaped.chars().any(char::is_control),
                "escaped field still holds a control character: {escaped:?}"
            );
            assert_eq!(
                super::unescape_journal_field(&escaped).as_deref(),
                Some(name)
            );
        }
        assert!(super::unescape_journal_field("unknown\\x").is_none());
        assert!(super::unescape_journal_field("\\x0").is_none());
        assert!(super::unescape_journal_field("\\xzz").is_none());
        assert!(super::unescape_journal_field("raw\ttab").is_none());
        assert!(super::unescape_journal_field("trailing\\").is_none());
    }

    #[test]
    fn journal_names_must_be_plain_siblings() {
        for bad in ["", ".", "..", "../x", "a/b", "nul\0"] {
            assert!(!super::plain_journal_name(bad), "{bad:?} accepted");
        }
        assert!(super::plain_journal_name("set.part1.rar"));
        assert!(super::plain_journal_name(".set.part1.rar.rar5bak-abc"));
        // A backslash is an ordinary character on Unix and a separator on
        // Windows; `C:evil` is drive-relative only on Windows.
        #[cfg(unix)]
        assert!(super::plain_journal_name("a\\b"));
        #[cfg(windows)]
        assert!(!super::plain_journal_name("a\\b"));
        #[cfg(windows)]
        assert!(!super::plain_journal_name("C:evil"));
    }

    #[test]
    fn recovery_ignores_bad_records_and_keeps_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("archive");
        std::fs::create_dir(&parent).unwrap();
        let outside = dir.path().join("victim");
        std::fs::write(&outside, b"keep").unwrap();

        let final_path = parent.join("set.part1.rar");
        std::fs::write(&final_path, b"new").unwrap();
        let backup = parent.join(".set.part1.rar.rar5bak-x");
        std::fs::write(&backup, b"old").unwrap();

        // The forward-slash traversal stays rejected on every platform; the
        // escaped backslash traversal is only a traversal on Windows (on Unix
        // it is an ordinary, harmless sibling name). The over-long and
        // unknown-kind records are malformed everywhere.
        let escaped_traversal = "..\\\\..\\\\victim";
        std::fs::write(
            super::journal_path(&parent, "set"),
            format!(
                "rar5commit v2\n\
                 install\t../../victim\t../../victim\n\
                 install\t{escaped_traversal}\t{escaped_traversal}\n\
                 install\thas\textra\tfields\n\
                 bogus\tone\ttwo\n\
                 backup\t{}\t{}\n",
                backup.file_name().unwrap().to_string_lossy(),
                final_path.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        super::recover_interrupted_commit(&parent, "set").unwrap();

        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        // The valid backup record still rolled the parked original back.
        assert_eq!(std::fs::read(&final_path).unwrap(), b"old");
        assert!(!backup.exists());
        // Skipped records keep the journal for inspection instead of being
        // silently forgotten and deleted.
        assert!(super::journal_path(&parent, "set").exists());
    }

    #[test]
    fn recovery_leaves_an_unknown_version_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let final_path = parent.join("set.part1.rar");
        std::fs::write(&final_path, b"new").unwrap();
        let journal = super::journal_path(parent, "set");

        for header in ["rar5commit v9", "rar5commit v1x", "not a journal"] {
            std::fs::write(
                &journal,
                format!("{header}\ninstall\tset.part1.rar\tset.part1.rar\n"),
            )
            .unwrap();
            super::recover_interrupted_commit(parent, "set").unwrap();
            assert_eq!(std::fs::read(&final_path).unwrap(), b"new");
            assert!(journal.exists(), "journal with {header:?} must be kept");
        }
    }

    #[test]
    fn write_commit_journal_escapes_control_characters() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let from = parent.join("bad\rname\x01");
        let to = parent.join("final.rar");

        super::write_commit_journal(parent, "set", &[(from, to)], &[]).unwrap();
        let text = std::fs::read_to_string(super::journal_path(parent, "set")).unwrap();
        assert_eq!(
            text.lines().count(),
            2,
            "records must stay on one line: {text:?}"
        );
        assert!(text.contains("bad\\rname\\x01"), "{text:?}");
    }

    #[cfg(unix)]
    #[test]
    fn journal_round_trips_control_and_backslash_names_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let backup = parent.join(".set.part1.rar.rar5bak-\t\n\r\x01\\");
        let final_path = parent.join("set.part1.rar");
        let staged = parent.join(".set.part1.rar.rar5tmp-x");
        std::fs::write(&backup, b"old").unwrap();
        std::fs::write(&final_path, b"new").unwrap();

        let install = vec![(staged, final_path.clone())];
        super::write_commit_journal(
            parent,
            "set",
            &[(backup.clone(), final_path.clone())],
            &install,
        )
        .unwrap();
        let text = std::fs::read_to_string(super::journal_path(parent, "set")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "records must stay on one line: {text:?}");
        assert!(
            lines
                .iter()
                .skip(1)
                .all(|line| line.split('\t').count() == 3),
            "malformed record: {text:?}"
        );
        assert_eq!(
            super::unescape_journal_field(lines[1].split('\t').nth(1).unwrap()).as_deref(),
            Some(backup.file_name().unwrap().to_str().unwrap()),
            "escaped field must round-trip: {text:?}"
        );

        super::recover_interrupted_commit(parent, "set").unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"old");
        assert!(!backup.exists());
        assert!(!super::journal_path(parent, "set").exists());
    }
}
