//! Opening, signature verification and the block scan.
//!
//! `open_read` drives the full scan (RAR5 blocks, RAR4 fallback, extra
//! volumes); `open_read_quick` first tries the quick-open locator and falls
//! back to the full scan. `parse_quick_open_payload` decodes the cached
//! header copies the locator points at.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::archive::{ArchiveEntry, RarArchive, StreamRecord, discover_volumes};
use crate::crypto;
use crate::detect::{SFX_SCAN_LIMIT, find_bytes};
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{ArchiveHeader, EndOfArchiveHeader};
use crate::format::rar5::vint;
use crate::format::rar5::{
    BLOCK_FLAG_DATA_CONTINUE_TO, BLOCK_FLAG_DATA_CONTINUES, BLOCK_TYPE_ARCHIVE_HEADER,
    BLOCK_TYPE_ENCRYPT_HEADER, BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_FILE_HEADER,
    BLOCK_TYPE_SERVICE_HEADER, MAX_METADATA_BYTES, RAR5_SIGNATURE,
};
use crate::format::shared::stream_mut;
use crate::model::{DataChunk, FileHeader};

impl RarArchive {
    pub(crate) fn open_read(&mut self) -> RarResult<()> {
        self.volume_paths = discover_volumes(&self.path);
        let f = File::open(&self.volume_paths[0])?;
        self.stream = Some(Box::new(f));
        self.verify_signature()?;
        if self.rar4 {
            self.scan_rar4_blocks()?;
        } else if self.volume_paths.len() > 1 {
            self.scan_all_volumes()?;
        } else {
            self.scan_blocks()?;
        }
        Ok(())
    }

    /// Open without a full block scan: read only the main archive header,
    /// resolve the quick-open record through the locator, and parse the
    /// cached file headers. Falls back to a full scan when the archive
    /// has no usable quick-open record (multi-volume, header-encrypted,
    /// no QO written, or a corrupt record).
    pub(crate) fn open_read_quick(&mut self) -> RarResult<()> {
        self.volume_paths = discover_volumes(&self.path);
        let f = File::open(&self.volume_paths[0])?;
        self.stream = Some(Box::new(f));
        self.verify_signature()?;
        if self.rar4 {
            // RAR4 has no quick-open record: always full-scan.
            self.scan_rar4_blocks()?;
            return Ok(());
        }
        if self.volume_paths.len() > 1 {
            self.scan_all_volumes()?;
            return Ok(());
        }
        if !self.try_quick_open_entries()? {
            // `try_quick_open_entries` may have consumed the leading
            // plaintext blocks (e.g. a -hp encryption header); rewind to
            // the archive start so the full scan sees them again.
            let stream = stream_mut(&mut self.stream)?;
            stream.seek(SeekFrom::Start(
                self.sfx_offset + RAR5_SIGNATURE.len() as u64,
            ))?;
            self.scan_blocks()?;
        }
        Ok(())
    }

    /// Try to populate [`Self::entries`] from the quick-open record.
    /// Returns `Ok(false)` when the archive has no usable record (the
    /// caller falls back to the full scan). QO-specific corruption falls
    /// back too; only genuine I/O errors propagate.
    fn try_quick_open_entries(&mut self) -> RarResult<bool> {
        // Header-encrypted archives never carry a QO record, and reading
        // their main header would need the derived key — bail out early.
        let first =
            match crate::format::rar5::headers::read_block(stream_mut(&mut self.stream)?, None)? {
                Some(meta) => meta,
                None => return Ok(false),
            };
        if first.block_type != BLOCK_TYPE_ARCHIVE_HEADER {
            return Ok(false);
        }
        let ah = ArchiveHeader::from_raw(&first.raw)?;
        let Some(qo_rel) = crate::format::rar5::headers::locator_quick_open_offset(&ah.extra_data)
        else {
            return Ok(false);
        };
        let qo_abs = self
            .sfx_offset
            .checked_add(RAR5_SIGNATURE.len() as u64)
            .and_then(|base| base.checked_add(qo_rel))
            .unwrap_or(u64::MAX);
        let stream = stream_mut(&mut self.stream)?;
        stream.seek(SeekFrom::Start(qo_abs))?;
        let Some(qo) = crate::format::rar5::headers::read_block(stream, None)? else {
            return Ok(false);
        };
        if qo.block_type != BLOCK_TYPE_SERVICE_HEADER {
            return Ok(false);
        }
        // The QO payload must fit entirely in memory; a hand-made header can
        // declare any size, so it is capped like every other service payload.
        if qo.raw.data_size > MAX_METADATA_BYTES {
            return Ok(false);
        }
        stream.seek(SeekFrom::Start(qo.data_offset))?;
        // Grown by the read rather than pre-sized: `take` bounds how much can
        // arrive, so the declared size alone never drives an allocation.
        let mut payload = Vec::new();
        stream
            .take(qo.raw.data_size)
            .read_to_end(&mut payload)
            .map_err(RarError::Io)?;
        if payload.len() as u64 != qo.raw.data_size {
            return Ok(false);
        }
        match parse_quick_open_payload(&payload, qo_abs) {
            Ok(entries) if !entries.is_empty() => {
                self.entries = entries;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn verify_signature(&mut self) -> RarResult<()> {
        // The signature must appear at the start for plain archives and
        // after the embedded stub for SFX archives (scan up to 8 MiB,
        // like the reference readers).
        let stream = stream_mut(&mut self.stream)?;
        let file_size = stream.seek(SeekFrom::End(0))?;
        stream.seek(SeekFrom::Start(0))?;
        let scan = file_size.min(SFX_SCAN_LIMIT as u64) as usize;
        let mut buf = vec![0u8; scan];
        let n = stream.read(&mut buf)?;
        buf.truncate(n);
        let rar5_pos = find_bytes(&buf, RAR5_SIGNATURE);
        let rar4_pos = find_bytes(&buf, crate::detect::RAR4_SIGNATURE);
        let (sfx_offset, is_rar4) = match (rar5_pos, rar4_pos) {
            (Some(r5), Some(r4)) => {
                if r4 < r5 {
                    (r4 as u64, true)
                } else {
                    (r5 as u64, false)
                }
            }
            (Some(r5), None) => (r5 as u64, false),
            (None, Some(r4)) => (r4 as u64, true),
            (None, None) => {
                return Err(RarError::Format(
                    "not a RAR archive (signature not found)".into(),
                ));
            }
        };
        self.sfx_offset = sfx_offset;
        self.rar4 = is_rar4;
        let sig_len = if is_rar4 {
            crate::detect::RAR4_SIGNATURE.len() as u64
        } else {
            RAR5_SIGNATURE.len() as u64
        };
        stream.seek(SeekFrom::Start(sfx_offset + sig_len))?;
        Ok(())
    }

    fn scan_blocks(&mut self) -> RarResult<()> {
        self.entries.clear();
        self.read_ctx_mut().streams.clear();

        // None until the plaintext archive-level encryption header arrives
        // (header-encrypted archives: every block after it is `[IV][AES-256-
        // CBC header]`).
        let mut encr_key: Option<[u8; 32]> = None;
        let mut last_file_index: Option<usize> = None;

        while let Some(meta) = crate::format::rar5::headers::read_block(
            stream_mut(&mut self.stream)?,
            encr_key.as_ref(),
        )? {
            self.check_cancel()?;
            let raw = &meta.raw;
            let stream_pos = stream_mut(&mut self.stream)?.stream_position()?;

            match raw.block_type {
                BLOCK_TYPE_ARCHIVE_HEADER => {
                    let _ah = ArchiveHeader::from_raw(raw)?;
                }
                BLOCK_TYPE_FILE_HEADER => {
                    let fh = FileHeader::from_raw(raw, stream_pos)?;
                    let chunk = DataChunk {
                        volume_index: 0,
                        data_offset: fh.data_offset,
                        packed_size: fh.packed_size,
                        crc32_val: fh.crc32_val,
                        is_final: true,
                        extra_data: fh.extra_data.clone(),
                    };
                    self.entries.push(ArchiveEntry {
                        header: fh,
                        chunks: vec![chunk],
                    });
                    last_file_index = Some(self.entries.len() - 1);
                }
                BLOCK_TYPE_SERVICE_HEADER
                    if raw.flags & crate::format::rar5::BLOCK_FLAG_DEPENDS_PREV != 0 =>
                {
                    // NTFS stream record ("STM"): the SUBDATA extra holds
                    // the stream name (":name"), the data area the content.
                    let name = self.service_block_name(&meta)?;
                    if name.as_deref() == Some("STM")
                        && let Some(owner_index) = last_file_index
                    {
                        let extra =
                            crate::format::rar5::headers::block_extra_area(&raw.header_data)?;
                        if let Some(stream_name) =
                            crate::format::rar5::headers::parse_service_subdata(&extra)
                            && !stream_name.is_empty()
                            && let Some((unpacked_size, method, dict_size_log)) =
                                crate::format::rar5::headers::parse_stream_params(&raw.header_data)
                        {
                            self.read_ctx_mut().streams.push(StreamRecord {
                                owner_index,
                                name: String::from_utf8_lossy(&stream_name).into_owned(),
                                data_offset: raw.data_offset,
                                data_size: raw.data_size,
                                unpacked_size,
                                method,
                                dict_size_log,
                            });
                        }
                    }
                }
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_ENCRYPT_HEADER => {
                    encr_key = Some(crypto::derive_header_key(raw, self.password.as_deref())?);
                }
                _ => {}
            }

            if raw.data_size > 0 {
                self.stream
                    .as_mut()
                    .unwrap()
                    .seek(SeekFrom::Start(meta.data_end))?;
            }
        }

        Ok(())
    }

    /// Scan a single-volume RAR 1.5–4.x archive (legacy fixed-width block
    /// headers). Multi-volume RAR4 sets use different naming and are not
    /// supported yet; opening one is reported clearly.
    fn scan_rar4_blocks(&mut self) -> RarResult<()> {
        self.entries.clear();
        let mut scan = crate::format::rar4::Rar4VolumeScan::default();
        let mut out = Vec::new();

        // Volume 0 is the already-open primary stream, positioned right
        // after the signature (SFX-aware). Later volumes open fresh and each
        // starts with its own 7-byte signature.
        scan.scan_volume(
            stream_mut(&mut self.stream)?,
            0,
            self.password.as_deref(),
            &mut out,
        )?;
        for (vol_idx, vol_path) in self.volume_paths.iter().enumerate().skip(1) {
            self.check_cancel()?;
            let mut stream = std::fs::File::open(vol_path)?;
            let mut sig = [0u8; 7];
            stream.read_exact(&mut sig)?;
            if &sig != crate::detect::RAR4_SIGNATURE {
                return Err(RarError::Format(format!(
                    "volume {} has a bad RAR4 signature",
                    vol_path.display()
                )));
            }
            scan.scan_volume(&mut stream, vol_idx, self.password.as_deref(), &mut out)?;
        }
        let archive_solid = scan.archive_solid;
        scan.finish()?;
        self.rar4_solid_archive = archive_solid;
        self.entries = out;
        Ok(())
    }

    ///
    /// Header-encrypted volume sets repeat the plaintext archive-level
    /// encryption header at the start of EVERY volume (WinRAR convention);
    /// every block after it is `[16-byte IV][AES-256-CBC encrypted
    /// header]`. The archive key is derived once per volume and reused for
    /// all of its blocks.
    fn scan_all_volumes(&mut self) -> RarResult<()> {
        self.entries.clear();
        let mut pending: Option<ArchiveEntry> = None;

        for (vol_idx, vol_path) in self.volume_paths.iter().enumerate() {
            let mut stream = File::open(vol_path)?;

            // Verify signature. The first volume may be an SFX stub, so the
            // archive begins at `sfx_offset` there; later volumes start at 0.
            if vol_idx == 0 && self.sfx_offset > 0 {
                stream.seek(SeekFrom::Start(self.sfx_offset))?;
            }
            let mut sig = [0u8; 8];
            stream.read_exact(&mut sig)?;
            if sig != *RAR5_SIGNATURE {
                return Err(RarError::Format(format!(
                    "volume {} has bad signature",
                    vol_path.display()
                )));
            }

            // None until this volume's plaintext encryption header arrives.
            let mut encr_key: Option<[u8; 32]> = None;

            while let Some(meta) =
                crate::format::rar5::headers::read_block(&mut stream, encr_key.as_ref())?
            {
                self.check_cancel()?;
                let raw = meta.raw;

                let stream_pos = stream.stream_position()?;

                match raw.block_type {
                    BLOCK_TYPE_ARCHIVE_HEADER => {
                        let _ah = ArchiveHeader::from_raw(&raw)?;
                    }
                    BLOCK_TYPE_FILE_HEADER => {
                        let fh = FileHeader::from_raw(&raw, stream_pos)?;
                        let continues_from = raw.flags & BLOCK_FLAG_DATA_CONTINUES != 0;
                        let continues_to = raw.flags & BLOCK_FLAG_DATA_CONTINUE_TO != 0;

                        let chunk = DataChunk {
                            volume_index: vol_idx,
                            data_offset: fh.data_offset,
                            packed_size: fh.packed_size,
                            crc32_val: fh.crc32_val,
                            is_final: !continues_to,
                            extra_data: fh.extra_data.clone(),
                        };

                        if continues_from {
                            if let Some(ref mut entry) = pending {
                                entry.chunks.push(chunk);
                                if !continues_to {
                                    // Final chunk: total packed size and the
                                    // final chunk's CRC (MAC'd when
                                    // encrypted). For encrypted members the
                                    // final chunk also carries the full extra
                                    // records (encryption with the hash-key
                                    // MAC bit, BLAKE2sp hash, time); the
                                    // reader must verify with those, so merge
                                    // them in when present.
                                    let total_packed: u64 =
                                        entry.chunks.iter().map(|c| c.packed_size).sum();
                                    entry.header.packed_size = total_packed;
                                    entry.header.crc32_val = fh.crc32_val;
                                    if !fh.extra_data.is_empty() {
                                        entry.header.extra_data = fh.extra_data.clone();
                                        entry.header.hash_type = fh.hash_type;
                                        entry.header.hash_value = fh.hash_value;
                                        entry.header.mtime_ns = fh.mtime_ns;
                                        entry.header.owner = fh.owner.clone();
                                        entry.header.group = fh.group.clone();
                                        entry.header.version = fh.version;
                                    }
                                    self.entries.push(pending.take().unwrap());
                                }
                            }
                        } else if continues_to {
                            pending = Some(ArchiveEntry {
                                header: fh,
                                chunks: vec![chunk],
                            });
                        } else {
                            self.entries.push(ArchiveEntry {
                                header: fh,
                                chunks: vec![chunk],
                            });
                        }
                    }
                    BLOCK_TYPE_END_ARCHIVE => {
                        let eoa = EndOfArchiveHeader::from_raw(&raw)?;
                        let _ = eoa;
                        break; // continue to next volume
                    }
                    BLOCK_TYPE_ENCRYPT_HEADER => {
                        encr_key = Some(crypto::derive_header_key(&raw, self.password.as_deref())?);
                    }
                    _ => {}
                }

                if raw.data_size > 0 {
                    stream.seek(SeekFrom::Start(raw.data_offset + raw.data_size))?;
                }
            }
        }

        // Keep the first volume open as the default stream
        self.stream = Some(Box::new(File::open(&self.volume_paths[0])?));
        Ok(())
    }
}

/// Parse a quick-open record payload into archive entries.
///
/// Payload layout (mirrors the writer):
/// ```text
/// repeat:
///   [entry CRC32] 4 bytes LE, over [body]
///   [body size] vint
///   [body] = [flags vint] [relative offset vint] [header size vint]
///            [complete file-header block bytes]
/// ```
///
/// `qo_abs` is the absolute position of the QO record; each entry's
/// `relative offset` points back to its original file header, from which
/// the data-area offset follows. Returns an error for any structural or
/// CRC violation (the caller falls back to a full scan).
fn parse_quick_open_payload(payload: &[u8], qo_abs: u64) -> RarResult<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    while off < payload.len() {
        if off + 4 > payload.len() {
            return Err(RarError::Format("quick-open: truncated entry CRC".into()));
        }
        let stored_crc = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        off += 4;
        let (body_size, n) = vint::decode_from_slice(payload, off)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        off += n;
        let body_end = off
            .checked_add(body_size as usize)
            .ok_or_else(|| RarError::Format("quick-open: body size overflow".into()))?;
        if body_end > payload.len() {
            return Err(RarError::Format("quick-open: truncated entry body".into()));
        }
        let actual = crc32fast::hash(&payload[off..body_end]);
        if actual != stored_crc {
            return Err(RarError::Crc {
                expected: stored_crc,
                actual,
                context: "quick-open entry".into(),
            });
        }
        let mut p = off;
        // flags vint (writer always emits 0 = file header)
        let (flags, fn_) = vint::decode_from_slice(payload, p)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        p += fn_;
        let (rel, rn) = vint::decode_from_slice(payload, p)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        p += rn;
        let (hdr_size, hn) = vint::decode_from_slice(payload, p)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        p += hn;
        let hdr_end = p
            .checked_add(hdr_size as usize)
            .ok_or_else(|| RarError::Format("quick-open: header size overflow".into()))?;
        if hdr_end > body_end {
            return Err(RarError::Format("quick-open: truncated file header".into()));
        }
        let raw = crate::format::rar5::headers::parse_block_bytes(&payload[p..hdr_end])?;
        if raw.block_type != BLOCK_TYPE_FILE_HEADER {
            return Err(RarError::Format("quick-open: unexpected block type".into()));
        }
        // The original file header sat `rel` bytes before the QO record;
        // its data area starts right after the header envelope.
        let header_abs = qo_abs.checked_sub(rel).ok_or_else(|| {
            RarError::Format("quick-open: relative offset points past the archive start".into())
        })?;
        let data_offset = header_abs + (hdr_end - p) as u64;
        // `stream_pos` carries the data-area offset, matching scan_blocks.
        let fh = FileHeader::from_raw(&raw, data_offset)?;
        let chunk = DataChunk {
            volume_index: 0,
            data_offset,
            packed_size: fh.packed_size,
            crc32_val: fh.crc32_val,
            is_final: true,
            extra_data: fh.extra_data.clone(),
        };
        let _ = flags;
        entries.push(ArchiveEntry {
            header: fh,
            chunks: vec![chunk],
        });
        off = body_end;
    }
    Ok(entries)
}
