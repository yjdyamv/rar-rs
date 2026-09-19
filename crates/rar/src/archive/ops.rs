//! Thin engine-side wrappers over the family entry points.
//!
//! The role facades ([`ArchiveReader`](crate::ArchiveReader),
//! [`ArchiveWriter`](crate::ArchiveWriter), [`ArchiveEditor`](crate::ArchiveEditor))
//! must not name `format` internals — that is what
//! `tests/architecture_boundaries.rs::role_facades_stay_off_format_and_codec_internals`
//! pins. The family operations live in `format` as free functions taking
//! `&mut dyn Engine`; this module gives the engine back the method shape the
//! facades call, so `archive` owns the seam and the facades stay clean.
//!
//! The legacy write entry points below are still called with method syntax
//! by code that has not been converted yet (`archive/create.rs`,
//! `format/shared/write_ops.rs`), so they keep a method-shaped seam here.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::RarResult;

use super::{ExtractionReport, RarArchive};

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

    /// Extract every member, returning what was written and what the
    /// skip-existing policy left untouched.
    pub(crate) fn extract_all_with_options(
        &mut self,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<ExtractionReport> {
        crate::format::shared::extract::members::extract_all_with_options(self, dest_dir, opts)
    }

    /// Extract one member selected by its archive-order catalog index.
    pub(crate) fn extract_at_index_with_options(
        &mut self,
        idx: usize,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<PathBuf> {
        crate::format::shared::extract::members::extract_at_index_with_options(
            self, idx, dest_dir, opts,
        )
    }

    /// [`Self::extract_at_index_with_options`] recording into a shared
    /// report, for batch callers.
    pub(crate) fn extract_index_with_options(
        &mut self,
        idx: usize,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
        report: &mut ExtractionReport,
    ) -> RarResult<PathBuf> {
        crate::format::shared::extract::members::extract_index_with_options(
            self, idx, dest_dir, opts, report,
        )
    }

    /// Engine-side seam for [`crate::format::rar13::write`].
    pub(crate) fn emit_rar13_main_header(&mut self) -> RarResult<()> {
        crate::format::rar13::write::emit_rar13_main_header(self)
    }

    /// Engine-side seam for [`crate::format::rar13::write`].
    pub(crate) fn add_rar13_data(
        &mut self,
        name: String,
        data: Vec<u8>,
        level: u8,
        mtime: u32,
        mtime_ns: u32,
        comment: Option<Vec<u8>>,
    ) -> RarResult<()> {
        crate::format::rar13::write::add_rar13_data(
            self, name, data, level, mtime, mtime_ns, comment,
        )
    }

    /// Engine-side seam for [`crate::format::rar13::write`].
    pub(crate) fn add_file_rar13(
        &mut self,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<()> {
        crate::format::rar13::write::add_file_rar13(self, path, arcname, level)
    }

    /// Engine-side seam for [`crate::format::rar13::write`].
    pub(crate) fn write_rar13_dir_entry(
        &mut self,
        name: &str,
        mtime: u32,
        mtime_ns: u32,
    ) -> RarResult<()> {
        crate::format::rar13::write::write_rar13_dir_entry(self, name, mtime, mtime_ns)
    }
}
