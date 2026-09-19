//! Thin engine-side wrappers over the family entry points.
//!
//! The role facades ([`ArchiveReader`](crate::ArchiveReader),
//! [`ArchiveWriter`](crate::ArchiveWriter), [`ArchiveEditor`](crate::ArchiveEditor))
//! must not name `format` internals — that is what
//! `tests/architecture_boundaries.rs::role_facades_stay_off_format_and_codec_internals`
//! pins. The family operations live in `format` as free functions taking
//! `&mut dyn Engine`; this module gives the engine back the method shape the
//! facades call, so `archive` owns the seam and the facades stay clean.

use std::io::Write;

use crate::error::RarResult;

use super::RarArchive;

impl RarArchive {
    /// Read an entry selected by its archive-order catalog index.
    pub(crate) fn read_at_index_with_options(
        &mut self,
        target_idx: usize,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<Vec<u8>> {
        crate::format::shared::extract::read::read_at_index_with_options(self, target_idx, opts)
    }

    /// Stream an entry selected by its archive-order catalog index.
    pub(crate) fn read_to_writer_at_index_with_options(
        &mut self,
        target_idx: usize,
        writer: &mut dyn Write,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<u64> {
        crate::format::shared::extract::read::read_to_writer_at_index_with_options(
            self, target_idx, writer, opts,
        )
    }
}
