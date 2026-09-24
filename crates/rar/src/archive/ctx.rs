//! The engine traits for the archive engine.
//!
//! The traits themselves live in [`crate::engine`] — they have to, so that
//! the family code can name them without naming this module. Here they are
//! only wired up to [`RarArchive`], mostly by delegating to the inherent
//! methods the engine already had (`archive/engine.rs`), one `impl` per
//! capability trait so the grouping in `engine::ctx` is visible from the
//! implementation side too. `Engine` itself is a blanket impl over those six,
//! so there is nothing to implement for it here.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::crypto;
use crate::detect::ArchiveFamily;
use crate::engine::{
    ArchiveEntry, ArchiveStream, CatalogOps, EngineState, HeaderCryptoOps, Mode, Parts, ReadState,
    StreamOps, VolumeOps, WriteServices, WriteState,
};
use crate::error::RarResult;
use crate::write_progress::ProgressTracker;

use super::RarArchive;

impl EngineState for RarArchive {
    fn read_ctx(&self) -> &ReadState {
        self.read.as_ref().expect("read context not available")
    }

    fn read_ctx_mut(&mut self) -> &mut ReadState {
        self.read.as_mut().expect("read context not available")
    }

    fn write_ctx(&self) -> &WriteState {
        self.write.as_ref().expect("write context not available")
    }

    fn write_ctx_mut(&mut self) -> &mut WriteState {
        self.write.as_mut().expect("write context not available")
    }

    fn parts(&mut self) -> Parts<'_> {
        Parts {
            read: &mut self.read,
            entries: &mut self.entries,
            stream: &mut self.stream,
            volume_paths: &self.volume_paths,
            password: self.password.as_deref(),
            cancel: self.cancel.as_deref(),
        }
    }

    fn family(&self) -> ArchiveFamily {
        self.family
    }

    fn set_family(&mut self, family: ArchiveFamily) {
        self.family = family;
    }

    fn mode(&self) -> Mode {
        self.mode
    }

    fn is_rar4(&self) -> bool {
        self.family == ArchiveFamily::Rar15To40
    }

    fn is_rar13(&self) -> bool {
        self.family == ArchiveFamily::Rar13
    }
}

impl CatalogOps for RarArchive {
    fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    fn clear_catalog(&mut self) {
        self.entries.clear();
    }

    fn replace_catalog(&mut self, entries: Vec<ArchiveEntry>) {
        self.entries = entries;
    }

    fn reset_catalog_token(&mut self) -> RarResult<()> {
        self.read_ctx_mut().catalog_token = super::reader::allocate_catalog_token()?;
        Ok(())
    }

    fn push_entry(&mut self, entry: ArchiveEntry) {
        self.entries.push(entry);
    }
}

impl StreamOps for RarArchive {
    fn stream_mut(&mut self) -> RarResult<&mut Box<dyn ArchiveStream>> {
        self.stream.as_mut().ok_or_else(|| {
            crate::error::RarError::InvalidState("archive has no underlying stream".into())
        })
    }

    fn set_stream(&mut self, stream: Box<dyn ArchiveStream>) {
        self.stream = Some(stream);
    }
}

impl HeaderCryptoOps for RarArchive {
    fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    fn header_encryption(&self) -> bool {
        self.header_encryption
    }

    fn archive_encr(&self) -> Option<&crypto::EncryptionParams> {
        self.archive_encr.as_ref()
    }

    fn archive_keys(&self) -> Option<&crypto::DerivedKeys> {
        self.archive_keys.as_ref()
    }

    fn handle_archive_encrypt_header(&mut self, params: crypto::EncryptionParams) -> RarResult<()> {
        RarArchive::handle_archive_encrypt_header(self, params)
    }

    fn on_disk_header_len(&self, plain_len: u64) -> u64 {
        RarArchive::on_disk_header_len(self, plain_len)
    }

    fn write_block_header(&mut self, header_bytes: &[u8]) -> RarResult<()> {
        RarArchive::write_block_header(self, header_bytes)
    }
}

impl VolumeOps for RarArchive {
    fn path(&self) -> &Path {
        &self.path
    }

    fn volume_paths(&self) -> &[PathBuf] {
        &self.volume_paths
    }

    fn set_volume_paths(&mut self, paths: Vec<PathBuf>) {
        self.volume_paths = paths;
    }

    fn sfx_offset(&self) -> u64 {
        self.sfx_offset
    }

    fn set_sfx_offset(&mut self, offset: u64) {
        self.sfx_offset = offset;
    }

    fn archive_solid(&self) -> bool {
        self.archive_solid
    }

    fn set_archive_solid(&mut self, solid: bool) {
        self.archive_solid = solid;
    }

    fn start_next_volume(&mut self) -> RarResult<()> {
        RarArchive::start_next_volume(self)
    }

    fn start_next_volume_rar13(&mut self) -> RarResult<()> {
        RarArchive::start_next_volume_rar13(self)
    }

    fn bytes_written(&self) -> u64 {
        self.write_ctx().output.bytes_written
    }

    fn recovery_volume_reserve(&self, prefix_len: u64) -> u64 {
        RarArchive::recovery_volume_reserve(self, prefix_len)
    }

    fn add_bytes_written(&mut self, bytes: u64) {
        let ctx = self.write_ctx_mut();
        ctx.output.bytes_written = ctx.output.bytes_written.saturating_add(bytes);
    }

    fn current_volume_index(&self) -> usize {
        self.write_ctx().output.current_volume.saturating_sub(1)
    }
}

impl WriteServices for RarArchive {
    fn effective_threads(&self) -> usize {
        RarArchive::effective_threads(self)
    }

    fn progress_slot(&self) -> Option<(Arc<Mutex<ProgressTracker>>, usize)> {
        self.progress.clone().map(|p| (p, self.progress_member))
    }

    fn set_progress_member(&mut self, index: usize) {
        self.progress_member = index;
    }

    fn report_progress(&mut self, done: u64, member_total: u64) {
        if let Some((progress, member)) = self.progress_slot() {
            progress
                .lock()
                .expect("progress lock")
                .report(member, done, member_total);
        }
    }

    fn cancel_token(&self) -> Option<Arc<AtomicBool>> {
        self.cancel.clone()
    }

    fn cancel_flag(&self) -> Option<&AtomicBool> {
        self.cancel.as_deref()
    }

    fn record_quick_open_entry(&mut self, header_bytes: &[u8]) -> RarResult<()> {
        if !self.write_ctx().locator.quick_open {
            return Ok(());
        }
        let pos = self.stream_mut()?.stream_position()?;
        self.write_ctx_mut()
            .locator
            .quick_open_entries
            .push((pos, header_bytes.to_vec()));
        Ok(())
    }

    fn begin_solid_member(&mut self) -> bool {
        let chain_solid =
            self.write_ctx().solid.mode && self.write_ctx().solid.encoder_state.is_some();
        self.write_ctx_mut()
            .solid
            .encoder_state
            .get_or_insert_with(Default::default)
            .begin_member();
        chain_solid
    }
}
