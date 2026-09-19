//! In-crate test helpers on the engine.
//!
//! These used to be `#[cfg(test)] pub fn` on `RarArchive` inside
//! `format/shared/extract/read.rs`. They are engine-side test support — the
//! public surface reads through [`ArchiveReader`](crate::ArchiveReader) —
//! and the archive tests call them as methods, so they stay methods here
//! and delegate to the family free functions.

use crate::error::{RarError, RarResult};

use super::RarArchive;

impl RarArchive {
    /// Read a member with explicit limits (see [`crate::ExtractOptions`]).
    /// In-crate test helper: the public surface reads through
    /// [`ArchiveReader`](crate::ArchiveReader).
    pub fn read_with_options(
        &mut self,
        name: &str,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<Vec<u8>> {
        let target_idx = self
            .entries
            .iter()
            .position(|e| e.name() == name)
            .ok_or_else(|| RarError::MemberNotFound {
                name: name.to_string(),
            })?;
        crate::format::shared::extract::read::read_at_index_with_options(self, target_idx, opts)
    }

    /// Test the integrity of every member (like `rar t`): each member is
    /// decoded and its CRC32/BLAKE2sp verified without writing anything.
    /// Returns `(checked, failed)`; a nonzero `failed` is still `Ok` so
    /// callers can report per-member failures. Directories are skipped.
    /// In-crate test helper: the public surface uses
    /// [`ArchiveReader::verify`](crate::ArchiveReader::verify).
    pub fn test(&mut self) -> RarResult<(usize, usize)> {
        let mut checked = 0usize;
        let mut failed = 0usize;
        for index in 0..self.entries.len() {
            self.check_cancel()?;
            if self.entries[index].is_dir() {
                continue;
            }
            checked += 1;
            if crate::format::shared::extract::read::read_at_index_with_options(
                self,
                index,
                crate::options::ExtractOptions::default(),
            )
            .is_err()
            {
                failed += 1;
            }
        }
        Ok((checked, failed))
    }
}
