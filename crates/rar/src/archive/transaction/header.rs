//! Main-header rebuilding and the locator/record helpers.

use std::io::{Seek, SeekFrom};

use super::super::RarArchive;
use crate::detect::RAR5_SIGNATURE;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{ArchiveHeader, BlockMeta, split_main_extra};
use crate::format::rar5::{ARCHIVE_FLAG_LOCKED, ARCHIVE_FLAG_RECOVERY};

impl RarArchive {
    /// Rebuild the main archive header for the rewritten archive: original
    /// flags, original extra records with the locator replaced by a fresh
    /// quick-open / recovery locator. Returns the header position, the
    /// plaintext offsets of the preallocated quick-open and recovery offset
    /// fields (patched at the end), and the full header bytes.
    #[allow(clippy::type_complexity)] // the header position + locator fields
    pub(super) fn write_main_header(
        &mut self,
        meta: &BlockMeta,
        rr_percent: Option<u8>,
    ) -> RarResult<(u64, Option<usize>, Option<usize>, Vec<u8>)> {
        let ah = ArchiveHeader::from_raw(&meta.raw)?;
        if ah.flags & ARCHIVE_FLAG_LOCKED != 0 {
            return Err(RarError::ArchiveLocked);
        }
        let (had_qo, _had_rr, extra) = split_main_extra(&ah.extra_data)?;
        self.write_ctx_mut().locator.quick_open = had_qo && !self.header_encryption;
        // The recovery record is rebuilt when the original archive had one
        // (or when the caller forces it, e.g. the `rr` command).
        self.recovery_percent = rr_percent;

        let mut arch_flags = ah.flags & !ARCHIVE_FLAG_RECOVERY;
        if self.recovery_percent.is_some() {
            arch_flags |= ARCHIVE_FLAG_RECOVERY;
        }

        let quick_open = self.write_ctx().locator.quick_open;
        let recovery = self.recovery_percent.is_some();
        let (hdr, qo_field_pos, rr_field_pos) =
            crate::format::rar5::headers::locator::build_main_header(
                arch_flags, &extra, quick_open, recovery, None,
            );

        let main_start = self.stream.as_mut().unwrap().stream_position()?;
        self.write_block_header(&hdr)?;
        Ok((main_start, qo_field_pos, rr_field_pos, hdr))
    }

    /// Patch the rewritten main header with the real quick-open and/or
    /// recovery-record offsets and rewrite it in place (the offset fields
    /// were preallocated as fixed 5-byte vints, so the header length never
    /// changes).
    pub(super) fn patch_main_header(
        &mut self,
        qo_offset: Option<u64>,
        rr_offset: Option<u64>,
        main_start: u64,
        qo_field_pos: Option<usize>,
        rr_field_pos: Option<usize>,
        main_hdr: &[u8],
    ) -> RarResult<()> {
        let base = self.sfx_offset + RAR5_SIGNATURE.len() as u64;
        let mut hdr = main_hdr.to_vec();
        crate::format::rar5::headers::locator::patch_locator_fields(
            &mut hdr,
            qo_offset,
            rr_offset,
            qo_field_pos,
            rr_field_pos,
            base,
        )?;
        self.stream
            .as_mut()
            .unwrap()
            .seek(SeekFrom::Start(main_start))?;
        self.write_block_header(&hdr)?;
        self.stream.as_mut().unwrap().seek(SeekFrom::End(0))?;
        Ok(())
    }
}
