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
use std::path::{Path, PathBuf};

use crate::engine::BatchEntry;
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

    /// Add raw bytes as a named member, dispatching on the container
    /// family.
    pub(crate) fn add_bytes(&mut self, arcname: &str, data: &[u8], level: u8) -> RarResult<()> {
        crate::format::shared::write_ops::add_bytes(self, arcname, data, level)
    }

    /// Add a path (file or directory) to the archive, dispatching on the
    /// container family.
    pub(crate) fn add(&mut self, path: impl AsRef<Path>, compression_level: u8) -> RarResult<()> {
        crate::format::shared::write_ops::add(self, path, compression_level)
    }

    /// [`Self::add`] under a caller-supplied archive name.
    pub(crate) fn add_as(
        &mut self,
        path: impl AsRef<Path>,
        arcname: &str,
        compression_level: u8,
    ) -> RarResult<()> {
        crate::format::shared::write_ops::add_as(self, path, arcname, compression_level)
    }

    /// Add a directory entry without recursing into its children.
    pub(crate) fn add_directory_only(
        &mut self,
        path: impl AsRef<Path>,
        arcname: &str,
    ) -> RarResult<()> {
        crate::format::shared::write_ops::add_directory_only(self, path, arcname)
    }

    /// Add a whole batch of entries, preserving archive order.
    pub(crate) fn add_batch(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
        crate::format::shared::write_ops::add_batch(self, entries)
    }

    /// Queue the RAR4 archive comment (emitted before the first member).
    pub(crate) fn set_rar4_writer_comment(&mut self, text: Option<Vec<u8>>) {
        crate::format::rar4::write::pipeline::set_rar4_writer_comment(self, text);
    }

    /// Emit the deferred RAR 1.3/1.4 main header (before the first member,
    /// or for an empty archive at close).
    pub(crate) fn emit_rar13_main_header(&mut self) -> RarResult<()> {
        crate::format::rar13::write::emit_rar13_main_header(self)
    }

    /// Add a RAR5 redirect (symlink / hardlink / file copy) member.
    pub(crate) fn add_redirect(
        &mut self,
        name: &str,
        redir_type: u64,
        target: &str,
    ) -> RarResult<()> {
        crate::format::rar5::write::add::add_redirect(self, name, redir_type, target)
    }

    /// [`Self::add_redirect`] carrying the link's modification time.
    pub(crate) fn add_redirect_with_time(
        &mut self,
        name: &str,
        redir_type: u64,
        target: &str,
        mtime: u32,
        mtime_ns: Option<u32>,
    ) -> RarResult<()> {
        crate::format::rar5::write::add::add_redirect_with_time(
            self, name, redir_type, target, mtime, mtime_ns,
        )
    }
}
