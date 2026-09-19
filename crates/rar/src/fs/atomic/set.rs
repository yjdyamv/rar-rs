//! The journaled multi-file transaction: [`StagedSet`], the commit phases
//! (park, install, mark committed) and the recovery that rolls an interrupted
//! commit back or forward.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{RarError, RarResult};

use super::journal::{
    COMMIT_JOURNAL_VERSION, COMMIT_JOURNAL_VERSION_V1, COMMIT_JOURNAL_VERSION_V2,
    backup_sibling_path, commit_done_path, journal_path, plain_journal_name, restore_file,
    unescape_journal_name, write_commit_journal,
};
use super::primitives::{replace_file, sync_file, sync_parent_dir, temp_suffix};

/// A set of staged files installed as one journaled commit.
///
/// Opening the set recovers an interrupted commit of the same parent/base so
/// no caller can forget it; [`Self::track`] adopts files a lower-level
/// builder staged; [`Self::park`] records an existing file that moves to a
/// caller-chosen name as part of the same commit; [`Self::commit`] runs the
/// whole set through [`commit_files`]. A dropped, uncommitted set removes its
/// staged files.
pub(crate) struct StagedSet {
    parent: PathBuf,
    base: String,
    install: Vec<(PathBuf, PathBuf)>,
    /// `(parked, final)` pairs: existing files renamed aside by the commit.
    parks: Vec<(PathBuf, PathBuf)>,
    committed: bool,
}

impl StagedSet {
    /// Open a set for `parent`/`base`, recovering an interrupted commit of
    /// the same base first (idempotent).
    pub(crate) fn new(parent: &Path, base: &str) -> RarResult<Self> {
        let parent = if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        };
        recover_interrupted_commit(&parent, base)?;
        Ok(Self {
            parent,
            base: base.to_string(),
            install: Vec::new(),
            parks: Vec::new(),
            committed: false,
        })
    }

    /// Adopt a file that is already staged on disk (created by a lower-level
    /// builder) as one entry of the set. The path's lifetime is transferred:
    /// an uncommitted set removes it on drop.
    pub(crate) fn track(&mut self, staged: PathBuf, final_path: &Path) {
        self.install.push((staged, final_path.to_path_buf()));
    }

    /// Park the existing file at `final_path` as `parked` when the set
    /// commits: on rollback the parked original returns to `final_path`; on
    /// success it stays at `parked` (unlike a replaced original, which is
    /// deleted). The rename is journaled before it happens, so a process
    /// kill cannot strand the file at `parked`.
    ///
    /// The park also stands in for the replaced original's backup: a final
    /// that is both parked and installed is not hidden-backed-up a second
    /// time.
    pub(crate) fn park(&mut self, final_path: &Path, parked: PathBuf) {
        self.parks.push((parked, final_path.to_path_buf()));
    }

    /// Install every tracked file as one journaled commit. On failure the
    /// set is rolled back and its staged files are kept (still uncommitted)
    /// until drop.
    pub(crate) fn commit(&mut self) -> RarResult<()> {
        let result = commit_files_impl(&self.parent, &self.base, &self.install, &[], &self.parks);
        if result.is_ok() {
            self.committed = true;
        }
        result
    }
}

impl Drop for StagedSet {
    fn drop(&mut self) {
        if !self.committed {
            for (staged, _) in &self.install {
                let _ = fs::remove_file(staged);
            }
        }
    }
}

/// Roll back or finish an interrupted multi-file commit, if a journal is
/// present next to `base`.
///
/// Called before a write stages new output, so a process killed mid-commit
/// never leaves a mixed volume set behind. With the done marker present the
/// new set won, the parked originals are dropped and explicit parks stay at
/// their parked names; without it the commit had not finished, so newly
/// installed files are removed, parked originals (and explicit parks) are
/// restored, and leftover staged files are discarded. A final is only removed
/// when it either had no pre-existing original (no backup/park record) or that
/// original was actually parked (the backup/park file exists): a kill between
/// the parks leaves later finals untouched *and* unbacked, and deleting them
/// would destroy the old set. Malformed records (unknown escapes, wrong field
/// counts, names that are not plain siblings) and unknown record kinds are
/// skipped and keep the journal in place, so nothing skipped is silently
/// forgotten; a journal with an unknown version header is left untouched
/// (recovery never guesses).
pub(crate) fn recover_interrupted_commit(parent: &Path, base: &str) -> RarResult<()> {
    let journal = journal_path(parent, base);
    // A crash between writing and renaming the journal leaves only this.
    let _ = fs::remove_file(journal.with_extension("journal.tmp"));
    let Ok(text) = fs::read_to_string(&journal) else {
        // No journal: any done marker is stale (the commit that wrote it
        // removed its journal). Clear it so a later commit killed mid-write
        // cannot be misread as finished.
        let _ = fs::remove_file(commit_done_path(parent, base));
        return Ok(());
    };
    let mut lines = text.lines();
    let escaped = match lines.next() {
        Some(COMMIT_JOURNAL_VERSION) | Some(COMMIT_JOURNAL_VERSION_V2) => true,
        Some(COMMIT_JOURNAL_VERSION_V1) => false,
        _ => return Ok(()),
    };
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut installs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut parks: Vec<(PathBuf, PathBuf)> = Vec::new();
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
        let (from, to): (Option<OsString>, Option<OsString>) = if escaped {
            (unescape_journal_name(from), unescape_journal_name(to))
        } else {
            (Some(OsString::from(from)), Some(OsString::from(to)))
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
            "park" => parks.push((parent.join(from), parent.join(to))),
            "install" => installs.push((parent.join(from), parent.join(to))),
            _ => malformed = true,
        }
    }
    if commit_done_path(parent, base).exists() {
        for (backup, _) in &backups {
            let _ = fs::remove_file(backup);
        }
    } else {
        // A final has a recorded original when a backup or a park names it;
        // that original was actually parked when the file exists.
        let has_original_record = |final_path: &Path| {
            backups.iter().any(|(_, final_)| final_ == final_path)
                || parks.iter().any(|(_, final_)| final_ == final_path)
        };
        let original_parked = |final_path: &Path| {
            backups
                .iter()
                .any(|(backup, final_)| final_ == final_path && backup.exists())
                || parks
                    .iter()
                    .any(|(parked, final_)| final_ == final_path && parked.exists())
        };
        for (_, final_path) in &installs {
            let has_record = has_original_record(final_path);
            let parked = original_parked(final_path);
            if !has_record || parked {
                let _ = fs::remove_file(final_path);
            }
        }
        for (backup, final_path) in &backups {
            if backup.exists() {
                let _ = restore_file(backup, final_path);
            }
        }
        for (parked, final_path) in &parks {
            if parked.exists() {
                let _ = restore_file(parked, final_path);
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
    commit_files_impl(parent, base, install, retire, &[])
}

/// [`commit_files`] with explicit parks: `(parked, final)` pairs move an
/// existing final to a caller-chosen name as part of the same journaled
/// commit. A park is restored on rollback and kept on success (unlike the
/// hidden backups, which are deleted on success); a final that is both parked
/// and installed is not backed up a second time.
fn commit_files_impl(
    parent: &Path,
    base: &str,
    install: &[(PathBuf, PathBuf)],
    retire: &[PathBuf],
    parks: &[(PathBuf, PathBuf)],
) -> RarResult<()> {
    if install.is_empty() && retire.is_empty() && parks.is_empty() {
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
    // moves. A final with an explicit park already has a caller-owned
    // parking spot, so it is not backed up again.
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    for final_path in install
        .iter()
        .map(|(_, final_path)| final_path)
        .chain(retire.iter())
    {
        if parks.iter().any(|(_, final_)| final_ == final_path) {
            continue;
        }
        if final_path.exists() {
            backups.push((backup_sibling_path(final_path, &suffix), final_path.clone()));
        }
    }
    // A marker left by a commit killed between removing the journal and the
    // marker must not make this commit look finished to recovery; clear it
    // before this commit's journal exists.
    let _ = fs::remove_file(commit_done_path(parent, base));
    write_commit_journal(parent, base, &backups, install, parks)?;

    // (final path, staged path) for everything already installed.
    let mut installed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let rollback = |backups: &[(PathBuf, PathBuf)],
                    parks: &[(PathBuf, PathBuf)],
                    installed: &[(PathBuf, PathBuf)]| {
        for (final_path, staged) in installed.iter().rev() {
            let _ = restore_file(final_path, staged);
        }
        for (backup, final_path) in backups.iter().rev() {
            // A missing backup means this final was never parked (the kill
            // or failure happened before its phase-1 rename): leave the
            // original untouched instead of treating the missing backup as
            // "remove whatever is there".
            if backup.exists() {
                let _ = restore_file(backup, final_path);
            }
        }
        for (parked, final_path) in parks.iter().rev() {
            if parked.exists() {
                let _ = restore_file(parked, final_path);
            }
        }
    };

    // Phase 1: park every pre-existing final (explicit parks first, then the
    // replaced/retired originals).
    for (parked, final_path) in parks {
        if let Err(error) = fs::rename(final_path, parked) {
            rollback(&backups, parks, &installed);
            let _ = fs::remove_file(journal_path(parent, base));
            return Err(RarError::Io(error));
        }
    }
    for (backup, final_path) in &backups {
        if let Err(error) = fs::rename(final_path, backup) {
            rollback(&backups, parks, &installed);
            let _ = fs::remove_file(journal_path(parent, base));
            return Err(RarError::Io(error));
        }
    }

    // Phase 2: install the staged files. Each staged file is flushed first:
    // the rename must not be able to become durable ahead of the bytes it
    // names.
    for (staged, final_path) in install {
        if let Err(error) = sync_file(staged).and_then(|()| replace_file(staged, final_path)) {
            rollback(&backups, parks, &installed);
            let _ = fs::remove_file(journal_path(parent, base));
            return Err(error);
        }
        installed.push((final_path.clone(), staged.clone()));
    }

    // Make the swap durable before dropping the parked originals: the
    // journal, the parks and the installs all renamed entries in `parent`.
    if let Err(error) = sync_parent_dir(parent) {
        rollback(&backups, parks, &installed);
        let _ = fs::remove_file(journal_path(parent, base));
        return Err(error);
    }

    // Phase 3: mark committed, then drop the parked originals. The marker
    // must be durable before any backup goes away: if a power loss persisted
    // the backup removals while losing the marker, recovery would see "not
    // done", delete the new installs, and have no originals left to restore.
    // Explicit parks stay: they are the caller's damaged-original copies.
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
