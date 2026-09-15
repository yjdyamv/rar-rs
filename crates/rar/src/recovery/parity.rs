//! Parity-file sets installed as one journaled commit.
//!
//! The recovery builders (REV5 `.rev`, legacy GF(2^8) `.rev`, rebuilt data
//! volumes) fill a set of files that must all land or none: each is staged as
//! a temporary sibling, and the set owns the "refuse a non-file final",
//! "install as one commit", "sweep the temps on failure" and "return the
//! final paths" rules once.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::error::{RarError, RarResult};
use crate::fs::atomic::{StagedSet, read_write_create, temp_sibling_path};

/// A set of parity files staged as temporary siblings and installed as one
/// journaled commit.
///
/// The lifecycle is [`StagedSet`]'s: opening the set recovers an interrupted
/// commit of the same parent/base, and a dropped, uncommitted set removes
/// its staged files. This value adds the recovery policy on top:
/// [`Self::stage`] mints the temp sibling and keeps its handle, and
/// [`Self::commit`] refuses a non-file at any final path and returns the
/// final paths in order.
pub(crate) struct ParitySet {
    staged: StagedSet,
    finals: Vec<PathBuf>,
}

impl ParitySet {
    /// Open a set for `parent`/`base`, recovering an interrupted commit of
    /// the same base first (idempotent).
    pub(crate) fn new(parent: &Path, base: &str) -> RarResult<Self> {
        Ok(Self {
            staged: StagedSet::new(parent, base)?,
            finals: Vec::new(),
        })
    }

    /// Create the staged sibling for `final_path` and return its path and
    /// write handle. The set removes it unless [`Self::commit`] succeeds;
    /// builders must close the handle before committing.
    pub(crate) fn stage(&mut self, final_path: &Path) -> RarResult<(PathBuf, File)> {
        let staged = temp_sibling_path(final_path);
        let file = read_write_create(&staged)?;
        self.finals.push(final_path.to_path_buf());
        self.staged.track(staged.clone(), final_path);
        Ok((staged, file))
    }

    /// Install every staged file as one journaled commit and return the
    /// final paths. A directory (or other non-file) at a final path is a
    /// conflict: `commit_files` would park and replace it, then strand the
    /// parked entry because only files are dropped on success.
    pub(crate) fn commit(mut self) -> RarResult<Vec<PathBuf>> {
        if let Some(conflict) = self
            .finals
            .iter()
            .find(|final_path| final_path.exists() && !final_path.is_file())
        {
            return Err(RarError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "{}: refusing to replace a non-file entry with a recovery volume",
                    conflict.display()
                ),
            )));
        }
        self.staged.commit()?;
        Ok(std::mem::take(&mut self.finals))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_installs_every_staged_file_and_returns_the_final_paths() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("set.part1.rev");
        let second = dir.path().join("set.part2.rev");
        std::fs::write(&first, b"old-1").unwrap();

        let mut set = ParitySet::new(dir.path(), "set").unwrap();
        let (_tmp, mut file) = set.stage(&first).unwrap();
        std::io::Write::write_all(&mut file, b"new-1").unwrap();
        drop(file);
        let (_tmp, mut file) = set.stage(&second).unwrap();
        std::io::Write::write_all(&mut file, b"new-2").unwrap();
        drop(file);

        let written = set.commit().unwrap();
        assert_eq!(written, vec![first.clone(), second.clone()]);
        assert_eq!(std::fs::read(&first).unwrap(), b"new-1");
        assert_eq!(std::fs::read(&second).unwrap(), b"new-2");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5"))
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    /// A directory at a final path is refused before anything moves and the
    /// staged temps are swept, so the directory survives.
    #[test]
    fn commit_refuses_a_non_file_final_and_sweeps_the_temps() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("set.part1.rev");
        std::fs::create_dir(&final_path).unwrap();

        let mut set = ParitySet::new(dir.path(), "set").unwrap();
        let (tmp, file) = set.stage(&final_path).unwrap();
        drop(file);

        let error = set.commit().unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert!(final_path.is_dir(), "the directory must survive");
        assert!(!tmp.exists(), "the staged temp must be swept");
    }

    /// A failed install rolls the finals back and drop removes every staged
    /// temp: no partial set, no leftovers.
    #[test]
    fn failed_commit_keeps_the_old_finals_and_sweeps_the_temps() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("set.part1.rev");
        let second = dir.path().join("set.part2.rev");
        std::fs::write(&first, b"old-1").unwrap();
        std::fs::write(&second, b"old-2").unwrap();

        let mut set = ParitySet::new(dir.path(), "set").unwrap();
        let (first_tmp, mut file) = set.stage(&first).unwrap();
        std::io::Write::write_all(&mut file, b"new-1").unwrap();
        drop(file);
        let (second_tmp, mut file) = set.stage(&second).unwrap();
        std::io::Write::write_all(&mut file, b"new-2").unwrap();
        drop(file);
        // Remove the second staged file so the install fails after the first
        // one already landed.
        std::fs::remove_file(&second_tmp).unwrap();

        assert!(set.commit().is_err());
        assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
        assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
        assert!(!first_tmp.exists() && !second_tmp.exists());
    }
}
