//! The staged single-file values: [`StagedFile`] (a fresh sibling, installed
//! by `commit` and cleaned up on drop while uncommitted) and [`StagedCopy`]
//! (the public same-directory copy seam).

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use crate::error::{RarError, RarResult};

use super::primitives::{install_durable, read_write_create, temp_sibling_path};

/// One staged file: a fresh sibling created atomically, installed over its
/// destination by [`Self::commit`], and removed on drop while uncommitted.
///
/// The value owns the "original untouched until success" rule for a single
/// file: staging happens next to the destination, the bytes are synced and
/// renamed by [`install_durable`], and every early return discards the
/// staged copy through `Drop`.
pub(crate) struct StagedFile {
    staged: PathBuf,
    dest: PathBuf,
    armed: bool,
}

impl StagedFile {
    /// Create a fresh staged sibling for `dest` and return its write handle.
    pub(crate) fn create(dest: &Path) -> RarResult<(Self, File)> {
        let staged = temp_sibling_path(dest);
        let file = read_write_create(&staged)?;
        Ok((Self::adopt(staged, dest), file))
    }

    /// Adopt a freshly reserved sibling path for `dest` ([`StagedCopy`]
    /// adopts the copy it just made).
    fn adopt(staged: PathBuf, dest: &Path) -> Self {
        Self {
            staged,
            dest: dest.to_path_buf(),
            armed: true,
        }
    }

    /// Path of the staged sibling, for writers that receive the path instead
    /// of the handle; prefer [`Self::commit`] to install it.
    pub(crate) fn path(&self) -> &Path {
        &self.staged
    }

    /// Install the staged bytes over the destination durably. A failure
    /// before the rename (for example the staged sync) keeps the staged file
    /// armed so drop can clean it; a failure after the rename has already
    /// consumed it. [`StagedSet`] restores its staged files on a failed
    /// commit.
    pub(crate) fn commit(&mut self) -> RarResult<()> {
        let result = install_durable(&self.staged, &self.dest);
        if result.is_ok() {
            self.armed = false;
        }
        result
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.staged);
        }
    }
}

/// A same-directory copy of an existing file, installed over the original by
/// [`Self::commit`] and removed on drop while uncommitted.
///
/// This is the public seam for "edit a copy, then replace the original" flows
/// (the `rar` CLI's update/create staging): [`Self::create`] reserves a fresh
/// sibling (`create_new`; no pre-existing file is ever followed or truncated)
/// and copies the original into it, callers run their operation against
/// [`Self::path`], and [`Self::commit`] installs the copy durably — syncing
/// the bytes, replacing the original, and fsyncing the parent directory
/// (Unix) — so a power loss cannot persist the rename ahead of the bytes it
/// names. A dropped, uncommitted copy is cleaned up.
pub struct StagedCopy {
    inner: StagedFile,
}

impl StagedCopy {
    /// Copy `dest` to a fresh sibling next to it.
    pub fn create(dest: &Path) -> RarResult<Self> {
        let staged = temp_sibling_path(dest);
        let file = read_write_create(&staged)?;
        drop(file);
        if let Err(error) = fs::copy(dest, &staged) {
            let _ = fs::remove_file(&staged);
            return Err(RarError::Io(error));
        }
        Ok(Self {
            inner: StagedFile::adopt(staged, dest),
        })
    }

    /// Path of the staged copy, for APIs that operate on a path.
    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    /// Install the copy over the original durably. A failure before the
    /// rename keeps the staged copy armed so drop can clean it; a failure
    /// after the rename has already consumed it.
    pub fn commit(&mut self) -> RarResult<()> {
        self.inner.commit()
    }
}
