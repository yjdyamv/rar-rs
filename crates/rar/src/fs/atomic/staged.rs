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

/// A staged copy of a whole RAR4 volume set, installed over the original
/// volumes by [`Self::commit`] and cleaned up on drop while uncommitted.
///
/// The multi-volume counterpart of [`StagedCopy`]: every file of the set
/// (and its `.rev` parity volumes) is copied into a hidden directory next to
/// the originals, operations run against [`Self::path`] (the staged first
/// volume), and [`Self::commit`] installs the resulting volumes as one
/// journaled [`commit_files`] transaction, retiring the old volumes and any
/// stale parity files. A whole-set rewrite (RAR4 append/delete) is therefore
/// all-or-nothing even though it touches several files.
pub struct StagedVolumeSet {
    dir: PathBuf,
    first: PathBuf,
    parent: PathBuf,
    base: String,
}

impl StagedVolumeSet {
    /// Copy the volume set containing `dest` into a hidden staging directory
    /// next to it and return the staged first volume from [`Self::path`].
    pub fn create(dest: &Path) -> RarResult<Self> {
        let volumes = crate::discover_volumes(dest);
        if volumes.is_empty() {
            return Err(RarError::format(format!(
                "cannot stage {}: no volumes found",
                dest.display()
            )));
        }
        let parent = crate::fs::atomic::parent_dir(&volumes[0]).to_path_buf();
        let base = crate::fs::volume::volume_base_of(&volumes[0]);
        let dir = parent.join(format!(
            ".{base}.stage-{}",
            crate::fs::atomic::temp_suffix()
        ));
        fs::create_dir_all(&dir).map_err(RarError::Io)?;
        {
            let copy = |path: &Path| -> RarResult<()> {
                let name = path
                    .file_name()
                    .ok_or_else(|| RarError::format("volume path without a file name"))?;
                fs::copy(path, dir.join(name)).map_err(RarError::Io)?;
                Ok(())
            };
            for path in &volumes {
                copy(path)?;
            }
            // Carry the parity volumes so a repack inside the staging
            // directory rebuilds them for the new data instead of leaving
            // stale parity.
            for rev in crate::fs::volume::stale_volume_paths(
                &parent,
                &base,
                true,
                &volumes,
                &crate::recovery::rev3::rev_name_belongs_to_set,
            ) {
                let is_rev = rev
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rev"));
                if is_rev {
                    copy(&rev)?;
                }
            }
        }
        Ok(Self {
            first: dir.join(
                volumes[0]
                    .file_name()
                    .expect("a discovered volume always has a file name"),
            ),
            dir,
            parent,
            base,
        })
    }

    /// Path of the staged first volume, for APIs that operate on a path.
    pub fn path(&self) -> &Path {
        &self.first
    }

    /// Install the staged volumes over the originals as one journaled commit
    /// and retire the old volumes plus stale `.rev` files. A failure keeps
    /// the staging directory armed so drop can clean it.
    pub fn commit(&mut self) -> RarResult<()> {
        // The operation may have repacked the set, which can rename the first
        // volume (padding, `.rar` vs `.partNN`); re-discover from the base
        // name instead of trusting the path handed out by `path()`.
        let staged = crate::discover_volumes(&self.dir.join(format!("{}.rar", self.base)));
        if staged.is_empty() {
            return Err(RarError::format("staged volume set has no volumes"));
        }
        let mut install: Vec<(PathBuf, PathBuf)> = staged
            .iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .expect("a discovered volume always has a file name");
                (path.clone(), self.parent.join(name))
            })
            .collect();
        let mut keep: Vec<PathBuf> = install.iter().map(|(_, dest)| dest.clone()).collect();
        // Parity volumes rebuilt inside the staging directory are installed
        // as part of the same transaction.
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let is_rev = path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rev"));
                if is_rev
                    && crate::recovery::rev3::rev_name_belongs_to_set(
                        &self.parent,
                        &self.base,
                        name,
                    )
                {
                    let dest = self.parent.join(name);
                    keep.push(dest.clone());
                    install.push((path, dest));
                }
            }
        }
        let retire = crate::fs::volume::stale_volume_paths(
            &self.parent,
            &self.base,
            true,
            &keep,
            &crate::recovery::rev3::rev_name_belongs_to_set,
        );
        crate::fs::atomic::commit_files(&self.parent, &self.base, &install, &retire)?;
        Ok(())
    }
}

impl Drop for StagedVolumeSet {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
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
