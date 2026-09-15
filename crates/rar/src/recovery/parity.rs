//! Parity-file sets installed as one journaled commit.
//!
//! The recovery builders (REV5 `.rev`, legacy GF(2^8) `.rev`, rebuilt data
//! volumes) fill a set of files that must all land or none: each is staged as
//! a temporary sibling, and the set owns the "refuse a non-file final",
//! "install as one commit", "sweep the temps on failure" and "return the
//! final paths" rules once.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use crate::error::{RarError, RarResult};

/// A set of parity files staged as temporary siblings and installed as one
/// journaled commit.
///
/// Opening the set recovers an interrupted commit of the same parent/base
/// first (the set commits through the same journal); [`Self::stage`] creates
/// one temp sibling per final path and hands back its handle; [`Self::track`]
/// adopts a file a builder already staged. [`Self::commit`] refuses a
/// non-file at any final path, installs the whole set through
/// [`crate::fs::atomic::commit_files`] and returns the final paths. A dropped,
/// uncommitted set removes its staged files, so a failed build or install
/// leaves existing finals untouched and no temps behind.
pub(crate) struct ParitySet {
    parent: PathBuf,
    base: String,
    install: Vec<(PathBuf, PathBuf)>,
    committed: bool,
}

impl ParitySet {
    /// Open a set for `parent`/`base`, recovering an interrupted commit of
    /// the same base first (idempotent).
    pub(crate) fn new(parent: &Path, base: &str) -> RarResult<Self> {
        let parent = if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        };
        crate::fs::atomic::recover_interrupted_commit(&parent, base)?;
        Ok(Self {
            parent,
            base: base.to_string(),
            install: Vec::new(),
            committed: false,
        })
    }

    /// Create the staged sibling for `final_path` and return its path and
    /// write handle. The set removes it unless [`Self::commit`] succeeds;
    /// builders must close the handle before committing.
    pub(crate) fn stage(&mut self, final_path: &Path) -> RarResult<(PathBuf, File)> {
        let staged = crate::fs::atomic::temp_sibling_path(final_path);
        let file = crate::fs::atomic::read_write_create(&staged)?;
        self.install
            .push((staged.clone(), final_path.to_path_buf()));
        Ok((staged, file))
    }

    /// Install every staged file as one journaled commit and return the
    /// final paths. A directory (or other non-file) at a final path is a
    /// conflict: `commit_files` would park and replace it, then strand the
    /// parked entry because only files are dropped on success.
    pub(crate) fn commit(mut self) -> RarResult<Vec<PathBuf>> {
        if let Some(conflict) = self
            .install
            .iter()
            .map(|(_, final_path)| final_path)
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
        let written: Vec<PathBuf> = self
            .install
            .iter()
            .map(|(_, final_path)| final_path.clone())
            .collect();
        crate::fs::atomic::commit_files(&self.parent, &self.base, &self.install, &[])?;
        self.committed = true;
        Ok(written)
    }
}

impl Drop for ParitySet {
    fn drop(&mut self) {
        if !self.committed {
            for (staged, _) in &self.install {
                let _ = fs::remove_file(staged);
            }
        }
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
