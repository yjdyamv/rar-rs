//! Main-header rebuilding and the locator/record helpers.

use std::io::{Seek, SeekFrom};

use super::super::RarArchive;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{ArchiveHeader, BlockMeta, split_main_extra};
use crate::format::rar5::vint;
use crate::format::rar5::{
    ARCHIVE_FLAG_LOCKED, ARCHIVE_FLAG_RECOVERY, BLOCK_FLAG_DATA_AREA, BLOCK_FLAG_EXTRA_DATA,
    BLOCK_TYPE_ARCHIVE_HEADER, FILE_FLAG_CRC32, FILE_FLAG_TIME_UNIX, RAR5_SIGNATURE,
};

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
        let (had_qo, _had_rr, mut extra) = split_main_extra(&ah.extra_data)?;
        self.write_ctx_mut().quick_open = had_qo && !self.header_encryption;
        // The recovery record is rebuilt when the original archive had one
        // (or when the caller forces it, e.g. the `rr` command).
        self.recovery_percent = rr_percent;

        let mut arch_flags = ah.flags & !ARCHIVE_FLAG_RECOVERY;
        if self.recovery_percent.is_some() {
            arch_flags |= ARCHIVE_FLAG_RECOVERY;
        }

        let quick_open = self.write_ctx().quick_open;
        let recovery = self.recovery_percent.is_some();
        let (locator, qo_field_pos, rr_field_pos) =
            crate::format::rar5::headers::locator::build_locator_body(quick_open, recovery);
        // When neither QO nor RR is active, locator_flags == 0 and
        // the body is exactly 1 byte (the flags vint).  Omit the record
        // in that case, preserving the original conditional emit.
        if locator.len() > 1 {
            extra.extend(crate::format::rar5::headers::locator::frame_locator_record(
                &locator,
            ));
        }

        let mut block_flags = 0u64;
        if !extra.is_empty() {
            block_flags |= BLOCK_FLAG_EXTRA_DATA;
        }

        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_ARCHIVE_HEADER));
        body.extend(vint::encode(block_flags));
        if block_flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            body.extend(vint::encode(extra.len() as u64));
        }
        body.extend(vint::encode(arch_flags));
        body.extend(&extra);

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();
        let mut hdr = Vec::with_capacity(4 + header_content.len());
        hdr.extend(crc.to_le_bytes());
        hdr.extend(header_content);

        // Plaintext-relative offset of the locator offset fields inside the
        // locator body (see write_archive_header_with_locators).
        let field_base = 4usize
            + size_bytes.len()
            + vint::encoded_size(BLOCK_TYPE_ARCHIVE_HEADER)
            + vint::encoded_size(block_flags)
            + vint::encoded_size(extra.len() as u64)
            + vint::encoded_size(arch_flags)
            + vint::encoded_size(locator.len() as u64)
            + vint::encoded_size(crate::format::rar5::headers::locator::LOCATOR_TYPE);
        let qo_field_pos = qo_field_pos.map(|p| field_base + p);
        let rr_field_pos = rr_field_pos.map(|p| field_base + p);

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

    /// Recovery percentage carried by a dropped "RR" service block
    /// (service data record type 0x07, single byte).
    pub(crate) fn rr_percent_from_block(&self, meta: &BlockMeta) -> Option<u8> {
        let data = &meta.raw.header_data;
        let mut offset = 0usize;
        let (_, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n;
        let mut extra_size = 0usize;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (v, n) = vint::decode_from_slice(data, offset).ok()?;
            extra_size = v as usize;
            offset += n;
        }
        if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset).ok()?;
            offset += n;
        }
        // file flags, unpacked size, attributes, compression info, host OS
        for _ in 0..5 {
            let (_, n) = vint::decode_from_slice(data, offset).ok()?;
            offset += n;
        }
        let (name_len, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n + name_len as usize;
        if offset + extra_size > data.len() {
            return None;
        }
        let extra = &data[offset..offset + extra_size];
        let mut e = 0usize;
        let (rec_size, n) = vint::decode_from_slice(extra, e).ok()?;
        e += n;
        let rec_start = e;
        let (rec_type, n) = vint::decode_from_slice(extra, e).ok()?;
        let _ = n;
        if rec_type != 0x07 || rec_size == 0 {
            return None;
        }
        let data_end = rec_start + rec_size as usize;
        if data_end > extra.len() {
            return None;
        }
        Some(extra[data_end - 1])
    }

    /// Name of a service block (type 3), if parseable.
    pub(crate) fn service_block_name(&self, meta: &BlockMeta) -> RarResult<Option<String>> {
        let data = &meta.raw.header_data;
        let mut offset = 0usize;
        let (_, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block type: {e}")))?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block flags: {e}")))?;
        offset += n;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block extra size: {e}")))?;
            offset += n;
        }
        if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block data size: {e}")))?;
            offset += n;
        }
        // file flags, unpacked size, attributes, then fixed time/CRC32
        // fields, then compression info and host OS.
        let (file_flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block file flags: {e}")))?;
        offset += n;
        for _ in 0..2 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block field: {e}")))?;
            offset += n;
        }
        if file_flags & FILE_FLAG_TIME_UNIX != 0 {
            offset += 4;
        }
        if file_flags & FILE_FLAG_CRC32 != 0 {
            offset += 4;
        }
        for _ in 0..2 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block field: {e}")))?;
            offset += n;
        }
        let (name_len, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block name: {e}")))?;
        offset += n;
        let end = (offset + name_len as usize).min(data.len());
        Ok(Some(
            String::from_utf8_lossy(&data[offset..end]).into_owned(),
        ))
    }
}
