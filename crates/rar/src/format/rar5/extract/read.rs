//! Member reads by name or index, with and without explicit limits.

use std::io::Write;

use crate::archive::RarArchive;
use crate::error::{RarError, RarResult};

impl RarArchive {
    /// Read a member with explicit limits (see [`crate::ExtractOptions`]).
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
        self.read_at_index_with_options(target_idx, opts)
    }

    /// Read an entry selected by its archive-order catalog index.
    pub(crate) fn read_at_index_with_options(
        &mut self,
        target_idx: usize,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<Vec<u8>> {
        if target_idx >= self.entries.len() {
            return Err(RarError::InvalidState(
                "entry index is outside the current catalog".into(),
            ));
        }
        self.read_ctx_mut().extract_options = opts;
        self.validate_entry_limits(target_idx)?;
        if self.rar4 {
            return self.decode_rar4_at(target_idx);
        }
        if self.is_solid_chain_member(target_idx) {
            return self.decode_solid_through(target_idx);
        }
        self.decode_file_at(target_idx, None)
    }

    /// Stream one member's uncompressed content into `writer` (bounded
    /// memory: the member is decoded block by block, never materialized).
    /// Returns the number of bytes written. The default limits of
    /// [`ExtractOptions`] still apply (4 GiB per member); pass
    /// [`ExtractOptions`] with `max_unpacked_bytes: None` via
    /// [`Self::read_to_writer_with_options`] for arbitrarily large members.
    /// Solid-chain members decode the whole chain through the member, like
    /// [`Self::read`].
    pub fn read_to_writer(&mut self, name: &str, writer: &mut dyn Write) -> RarResult<u64> {
        self.read_to_writer_with_options(name, writer, crate::options::ExtractOptions::default())
    }

    /// [`Self::read_to_writer`] with explicit limits (see
    /// [`crate::ExtractOptions`]).
    pub fn read_to_writer_with_options(
        &mut self,
        name: &str,
        writer: &mut dyn Write,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<u64> {
        let target_idx = self
            .entries
            .iter()
            .position(|e| e.name() == name)
            .ok_or_else(|| RarError::MemberNotFound {
                name: name.to_string(),
            })?;
        self.read_to_writer_at_index_with_options(target_idx, writer, opts)
    }

    /// Stream an entry selected by its archive-order catalog index.
    pub(crate) fn read_to_writer_at_index_with_options(
        &mut self,
        target_idx: usize,
        writer: &mut dyn Write,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<u64> {
        if target_idx >= self.entries.len() {
            return Err(RarError::InvalidState(
                "entry index is outside the current catalog".into(),
            ));
        }
        self.read_ctx_mut().extract_options = opts;
        if self.rar4 {
            return self.decode_rar4_to(target_idx, writer);
        }
        if self.is_solid_chain_member(target_idx) {
            return self.decode_solid_through_to(target_idx, writer);
        }
        self.decode_file_to(target_idx, writer, None)
    }

    /// Test the integrity of every member (like `rar t`): each member is
    /// decoded and its CRC32/BLAKE2sp verified without writing anything.
    /// Returns `(checked, failed)`; a nonzero `failed` is still `Ok` so
    /// callers can report per-member failures. Directories are skipped.
    pub fn test(&mut self) -> RarResult<(usize, usize)> {
        let mut checked = 0usize;
        let mut failed = 0usize;
        for index in 0..self.entries.len() {
            self.check_cancel()?;
            if self.entries[index].is_dir() {
                continue;
            }
            checked += 1;
            if self
                .read_at_index_with_options(index, crate::options::ExtractOptions::default())
                .is_err()
            {
                failed += 1;
            }
        }
        Ok((checked, failed))
    }
}
