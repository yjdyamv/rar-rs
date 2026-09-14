//! RAR5 block scanning, quick-open resolution and catalog building.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::archive::{ArchiveEntry, RarArchive, StreamRecord};
use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{ArchiveHeader, EndOfArchiveHeader, RawBlock};
use crate::format::rar5::vint;
use crate::format::rar5::{
    BLOCK_FLAG_DATA_CONTINUE_TO, BLOCK_FLAG_DATA_CONTINUES, BLOCK_TYPE_ARCHIVE_HEADER,
    BLOCK_TYPE_ENCRYPT_HEADER, BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_FILE_HEADER,
    BLOCK_TYPE_SERVICE_HEADER, MAX_METADATA_BYTES, RAR5_SIGNATURE,
};
use crate::format::shared::extract::{MAX_CATALOG_ENTRIES, check_entry_cap};
use crate::format::shared::stream_mut;
use crate::model::{DataChunk, FileHeader};

/// Ceiling on how many data chunks one continuing member may accumulate
/// across volumes. A real set contributes at most one chunk per volume, so
/// the bound sits far above any archival use; without it a crafted set of
/// tiny continuation headers grows one member's chunk vector (and the
/// cloned extra records it holds) without bound.
const MAX_MEMBER_CHUNKS: usize = 1_000_000;

/// Reject a continuing member that would grow past `max` chunks.
fn check_chunk_cap(count: usize, max: usize, member: &str) -> RarResult<()> {
    if count >= max {
        return Err(RarError::Format(format!(
            "member {member} exceeds the {max}-chunk ceiling"
        )));
    }
    Ok(())
}

/// Convert a quick-open declared size to `usize`, rejecting lengths that do
/// not fit the host address space: on 32-bit targets `as usize` would
/// truncate `2^32 + N` to `N`, so the entry CRC would be verified over — and
/// the embedded header parsed from — a range other than the declared one.
fn qo_size_to_usize(size: u64, what: &str) -> RarResult<usize> {
    usize::try_from(size).map_err(|_| RarError::LimitExceeded {
        limit: size,
        context: format!("quick-open: {what} overflows host address space"),
    })
}

impl RarArchive {
    /// Full RAR5 scan: multi-volume sets rescan every volume, single-volume
    /// archives scan their block sequence.
    pub(crate) fn open_read_rar5(&mut self) -> RarResult<()> {
        if self.volume_paths.len() > 1 {
            self.scan_all_volumes()
        } else {
            self.scan_blocks()
        }
    }

    /// RAR5 quick-open: read only the main archive header, resolve the
    /// quick-open record through the locator, and parse the cached file
    /// headers. Falls back to a full scan when the archive has no usable
    /// quick-open record (multi-volume, header-encrypted, no QO written,
    /// or a corrupt record).
    pub(crate) fn open_read_quick_rar5(&mut self) -> RarResult<()> {
        if self.volume_paths.len() > 1 {
            return self.scan_all_volumes();
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
        if ah.flags & crate::format::rar5::ARCHIVE_FLAG_SOLID != 0 {
            self.rar4_solid_archive = true;
        }
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
                self.read_ctx_mut().quick_open_catalog = true;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Guarantee that `self.entries` came from a full block scan, so the
    /// service records the quick-open payload does not cache ("STM" NTFS
    /// streams) are discovered. No-op unless the catalog came from the
    /// quick-open record. The scan reads headers only: payload areas are
    /// skipped with seeks, never loaded.
    ///
    /// The scan can reorder members relative to the cached catalog, and the
    /// catalog token is deliberately *not* rotated here: [`crate::EntryId`]s
    /// carry the member's packed-payload offset and are re-resolved by
    /// identity (see `ArchiveReader::resolve_id`), so IDs issued from the
    /// cached listing survive the reorder while an ID whose member the scan
    /// no longer contains still fails as stale.
    pub(crate) fn ensure_full_catalog(&mut self) -> RarResult<()> {
        if !self.read_ctx().quick_open_catalog {
            return Ok(());
        }
        let stream = stream_mut(&mut self.stream)?;
        stream.seek(SeekFrom::Start(
            self.sfx_offset + RAR5_SIGNATURE.len() as u64,
        ))?;
        self.scan_blocks()?;
        Ok(())
    }

    fn scan_blocks(&mut self) -> RarResult<()> {
        self.entries.clear();
        self.read_ctx_mut().streams.clear();
        self.read_ctx_mut().quick_open_catalog = false;

        // None until the plaintext archive-level encryption header arrives
        // (header-encrypted archives: every block after it is `[IV][AES-256-
        // CBC header]`).
        let mut encr_key: Option<[u8; 32]> = None;
        let mut last_file_index: Option<usize> = None;
        // Declared data areas are bounded against the real file: a hostile
        // vint size can exceed the filesystem's maximum offset, where the
        // skip seek fails on Linux instead of hitting EOF.
        let file_len = crate::format::shared::stream_len(stream_mut(&mut self.stream)?)?;

        while let Some(meta) = crate::format::rar5::headers::read_block(
            stream_mut(&mut self.stream)?,
            encr_key.as_ref(),
        )? {
            self.check_cancel()?;
            let raw = &meta.raw;
            let stream_pos = stream_mut(&mut self.stream)?.stream_position()?;

            match raw.block_type {
                BLOCK_TYPE_ARCHIVE_HEADER => {
                    let ah = ArchiveHeader::from_raw(raw)?;
                    if ah.flags & crate::format::rar5::ARCHIVE_FLAG_SOLID != 0 {
                        self.rar4_solid_archive = true;
                    }
                }
                BLOCK_TYPE_FILE_HEADER => {
                    check_entry_cap(self.entries.len(), MAX_CATALOG_ENTRIES)?;
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
                        self.record_member_stream(&meta.raw, owner_index, 0)?;
                    }
                }
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_ENCRYPT_HEADER => {
                    encr_key = Some(crypto::derive_header_key(raw, self.password.as_deref())?);
                }
                _ => {}
            }

            if raw.data_size > 0
                && !crate::format::shared::seek_past_data_area(
                    stream_mut(&mut self.stream)?,
                    meta.data_end,
                    file_len,
                )?
            {
                break;
            }
        }

        Ok(())
    }

    /// Record one "STM" service block as an NTFS alternate data stream owned
    /// by `owner_index` and stored in `volume_index` (0 = the primary
    /// stream). `-p` streams carry their own ENCR record; the password is
    /// only needed at read time, so listing works on locked archives like it
    /// does for encrypted members.
    fn record_member_stream(
        &mut self,
        raw: &RawBlock,
        owner_index: usize,
        volume_index: usize,
    ) -> RarResult<()> {
        let extra = crate::format::rar5::headers::block_extra_area(&raw.header_data)?;
        if let Some(stream_name) = crate::format::rar5::headers::parse_service_subdata(&extra)
            && !stream_name.is_empty()
            && let Some((unpacked_size, method, dict_size_log, crc32)) =
                crate::format::rar5::headers::parse_stream_params(&raw.header_data)
        {
            let params = crate::crypto::parse_encryption_extra(&extra)?;
            self.read_ctx_mut().streams.push(StreamRecord {
                owner_index,
                volume_index,
                name: String::from_utf8_lossy(&stream_name).into_owned(),
                data_offset: raw.data_offset,
                data_size: raw.data_size,
                unpacked_size,
                method,
                dict_size_log,
                crc32,
                params,
            });
        }
        Ok(())
    }

    ///
    /// Header-encrypted volume sets repeat the plaintext archive-level
    /// encryption header at the start of EVERY volume (WinRAR convention);
    /// every block after it is `[16-byte IV][AES-256-CBC encrypted
    /// header]`. The archive key is derived once per volume and reused for
    /// all of its blocks.
    fn scan_all_volumes(&mut self) -> RarResult<()> {
        self.scan_all_volumes_capped(MAX_CATALOG_ENTRIES, MAX_MEMBER_CHUNKS)
    }

    /// [`scan_all_volumes`] with explicit entry/chunk ceilings. A crafted
    /// volume set can keep adding FILE_HEAD blocks while the catalog grows
    /// without bound (the headers on disk are tiny, the entry objects are
    /// not), so the same ceiling [`scan_blocks`] applies is enforced here
    /// too; a single continuing member is likewise bounded so its chunk
    /// vector cannot grow without limit.
    fn scan_all_volumes_capped(&mut self, max_entries: usize, max_chunks: usize) -> RarResult<()> {
        self.entries.clear();
        self.read_ctx_mut().streams.clear();
        let mut pending: Option<ArchiveEntry> = None;
        // Owner of a "STM" record that may appear in any volume after its
        // member's final chunk.
        let mut last_file_index: Option<usize> = None;
        // Cloned so the loop body can push stream records into `self`
        // (the field-borrow checker cannot split `self.volume_paths` from
        // the rest of `self` here).
        let volume_paths = self.volume_paths.clone();

        for (vol_idx, vol_path) in volume_paths.iter().enumerate() {
            let mut stream = File::open(vol_path)?;
            // Bound declared data areas against this volume's real size (see
            // `seek_past_data_area`: an out-of-range skip seek fails on Linux).
            let vol_len = stream.metadata().map_err(RarError::Io)?.len();

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
                let raw = &meta.raw;

                let stream_pos = stream.stream_position()?;

                match raw.block_type {
                    BLOCK_TYPE_ARCHIVE_HEADER => {
                        let ah = ArchiveHeader::from_raw(raw)?;
                        if ah.flags & crate::format::rar5::ARCHIVE_FLAG_SOLID != 0 {
                            self.rar4_solid_archive = true;
                        }
                    }
                    BLOCK_TYPE_FILE_HEADER => {
                        check_entry_cap(self.entries.len(), max_entries)?;
                        let fh = FileHeader::from_raw(raw, stream_pos)?;
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
                                check_chunk_cap(
                                    entry.chunks.len(),
                                    max_chunks,
                                    &entry.header.name,
                                )?;
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
                                    last_file_index = Some(self.entries.len() - 1);
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
                            last_file_index = Some(self.entries.len() - 1);
                        }
                    }
                    BLOCK_TYPE_SERVICE_HEADER
                        if raw.flags & crate::format::rar5::BLOCK_FLAG_DEPENDS_PREV != 0 =>
                    {
                        // NTFS stream record ("STM") owned by the preceding
                        // member; the record can sit in a later volume than
                        // the start of that member's data, so both the owner
                        // index and the volume index are recorded.
                        let name = self.service_block_name(&meta)?;
                        if name.as_deref() == Some("STM")
                            && let Some(owner_index) = last_file_index
                        {
                            self.record_member_stream(&meta.raw, owner_index, vol_idx)?;
                        }
                    }
                    BLOCK_TYPE_END_ARCHIVE => {
                        let eoa = EndOfArchiveHeader::from_raw(raw)?;
                        let _ = eoa;
                        break; // continue to next volume
                    }
                    BLOCK_TYPE_ENCRYPT_HEADER => {
                        encr_key = Some(crypto::derive_header_key(raw, self.password.as_deref())?);
                    }
                    _ => {}
                }

                if raw.data_size > 0
                    && !crate::format::shared::seek_past_data_area(
                        &mut stream,
                        meta.data_end,
                        vol_len,
                    )?
                {
                    break;
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
/// the data-area offset follows. Returns an error for any structural,
/// CRC or entry-count violation (the caller falls back to a full scan).
fn parse_quick_open_payload(payload: &[u8], qo_abs: u64) -> RarResult<Vec<ArchiveEntry>> {
    parse_quick_open_payload_capped(payload, qo_abs, MAX_CATALOG_ENTRIES)
}

/// [`parse_quick_open_payload`] with an explicit entry ceiling. The payload
/// is capped at [`MAX_METADATA_BYTES`], but its entries are tiny: without
/// the ceiling a crafted record expands into millions of entry objects.
fn parse_quick_open_payload_capped(
    payload: &[u8],
    qo_abs: u64,
    max_entries: usize,
) -> RarResult<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    while off < payload.len() {
        check_entry_cap(entries.len(), max_entries)?;
        if off + 4 > payload.len() {
            return Err(RarError::Format("quick-open: truncated entry CRC".into()));
        }
        let stored_crc = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        off += 4;
        let (body_size, n) = vint::decode_from_slice(payload, off)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        off += n;
        let body_len = qo_size_to_usize(body_size, "entry body size")?;
        let body_end = off
            .checked_add(body_len)
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
        let hdr_len = qo_size_to_usize(hdr_size, "file header size")?;
        let hdr_end = p
            .checked_add(hdr_len)
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
        let data_offset = header_abs + hdr_len as u64;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::discover_volumes;

    /// One quick-open entry: `[entry CRC32][body size][flags][relative
    /// offset][header size][complete file header]`, matching the writer.
    fn qo_entry(header: &[u8], rel: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(vint::encode(0));
        body.extend(vint::encode(rel));
        body.extend(vint::encode(header.len() as u64));
        body.extend_from_slice(header);
        let mut entry = Vec::new();
        entry.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        entry.extend(vint::encode(body.len() as u64));
        entry.extend(body);
        entry
    }

    #[test]
    fn quick_open_entry_cap_rejects_beyond_the_ceiling() {
        let header = FileHeader {
            name: "a.txt".into(),
            ..Default::default()
        }
        .to_bytes();
        let mut payload = qo_entry(&header, 10);
        payload.extend(qo_entry(&header, 11));
        assert!(parse_quick_open_payload_capped(&payload, 100, 2).is_ok());
        let err = parse_quick_open_payload_capped(&payload, 100, 1).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");
    }

    #[test]
    fn catalog_entry_cap_matches_the_bound() {
        assert!(check_entry_cap(MAX_CATALOG_ENTRIES - 1, MAX_CATALOG_ENTRIES).is_ok());
        assert!(check_entry_cap(MAX_CATALOG_ENTRIES, MAX_CATALOG_ENTRIES).is_err());
    }

    #[test]
    fn multivolume_scan_enforces_the_entry_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cap.rar");
        // Incompressible-ish payload so the store-level member cannot fit a
        // single 30 KB volume; mirrors the existing multi-volume tests.
        let mut data = vec![0u8; 102400];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i.wrapping_mul(7) ^ (i >> 3)) as u8;
        }
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    volume_size: Some(30_000),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("a.bin", &data, 0).unwrap();
            ar.add_bytes("b.bin", b"second member", 0).unwrap();
            ar.close().unwrap();
        }
        let vols = discover_volumes(&path);
        assert!(vols.len() > 1, "precondition: multiple volumes");

        let mut ar = RarArchive::open(&vols[0]).unwrap();
        assert_eq!(ar.entries.len(), 2, "precondition: two catalog entries");

        let err = ar
            .scan_all_volumes_capped(1, MAX_MEMBER_CHUNKS)
            .unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");

        ar.scan_all_volumes_capped(2, MAX_MEMBER_CHUNKS).unwrap();
        assert_eq!(ar.entries.len(), 2);
    }

    /// A one-volume archive holding `blocks` FILE_HEAD blocks of a single
    /// continuing member (the first opens the member, the last closes it),
    /// each without a data area.
    fn crafted_continuation_archive(blocks: usize) -> Vec<u8> {
        use crate::format::rar5::{BLOCK_FLAG_DATA_CONTINUE_TO, BLOCK_FLAG_DATA_CONTINUES};

        let mut out = RAR5_SIGNATURE.to_vec();
        out.extend_from_slice(
            &ArchiveHeader {
                flags: 0,
                extra_data: Vec::new(),
                volume_number: None,
            }
            .to_bytes(),
        );
        for i in 0..blocks {
            let mut fh = FileHeader {
                name: "member.bin".into(),
                ..Default::default()
            };
            fh.flags = match i {
                0 => BLOCK_FLAG_DATA_CONTINUE_TO,
                i if i + 1 == blocks => BLOCK_FLAG_DATA_CONTINUES,
                _ => BLOCK_FLAG_DATA_CONTINUES | BLOCK_FLAG_DATA_CONTINUE_TO,
            };
            out.extend_from_slice(&fh.to_bytes());
        }
        out.extend_from_slice(&EndOfArchiveHeader { flags: 0 }.to_bytes());
        out
    }

    #[test]
    fn multivolume_scan_enforces_the_chunk_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chunks.rar");
        std::fs::write(&path, crafted_continuation_archive(4)).unwrap();

        let mut ar = RarArchive::open(&path).unwrap();
        let err = ar
            .scan_all_volumes_capped(MAX_CATALOG_ENTRIES, 3)
            .unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");

        ar.scan_all_volumes_capped(MAX_CATALOG_ENTRIES, 4).unwrap();
        assert_eq!(
            ar.entries.len(),
            1,
            "the continuation blocks are one member"
        );
        assert_eq!(ar.entries[0].chunks.len(), 4);
    }

    #[test]
    fn chunk_cap_matches_the_bound() {
        assert!(check_chunk_cap(MAX_MEMBER_CHUNKS - 1, MAX_MEMBER_CHUNKS, "m").is_ok());
        assert!(check_chunk_cap(MAX_MEMBER_CHUNKS, MAX_MEMBER_CHUNKS, "m").is_err());
    }

    #[test]
    fn quick_open_declared_sizes_must_fit_the_host_address_space() {
        assert_eq!(qo_size_to_usize(4096, "entry body size").unwrap(), 4096);
        let over_32_bit = u64::from(u32::MAX) + 1;
        if cfg!(target_pointer_width = "64") {
            // 64-bit hosts can represent the value; only 32-bit targets can
            // execute the rejection arm below.
            assert_eq!(
                qo_size_to_usize(over_32_bit, "entry body size").unwrap(),
                usize::try_from(over_32_bit).unwrap()
            );
        } else {
            let err = qo_size_to_usize(over_32_bit, "entry body size").unwrap_err();
            assert!(matches!(err, RarError::LimitExceeded { .. }), "got {err}");
        }
    }

    #[test]
    fn quick_open_rejects_oversized_declared_sizes() {
        // Body size beyond u32: truncated to `N` by `as usize` on 32-bit
        // targets, out of range on every target.
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend(vint::encode(u64::from(u32::MAX) + 10));
        let err = parse_quick_open_payload_capped(&payload, 1000, 4).unwrap_err();
        assert!(
            matches!(err, RarError::Format(_) | RarError::LimitExceeded { .. }),
            "unexpected: {err:?}"
        );

        // Header size beyond u32 inside a CRC-valid body.
        let header = FileHeader {
            name: "a.txt".into(),
            ..Default::default()
        }
        .to_bytes();
        let mut body = Vec::new();
        body.extend(vint::encode(0));
        body.extend(vint::encode(10));
        body.extend(vint::encode(u64::from(u32::MAX) + 10));
        body.extend_from_slice(&header);
        let mut payload = Vec::new();
        payload.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        payload.extend(vint::encode(body.len() as u64));
        payload.extend_from_slice(&body);
        let err = parse_quick_open_payload_capped(&payload, 1000, 4).unwrap_err();
        assert!(
            matches!(err, RarError::Format(_) | RarError::LimitExceeded { .. }),
            "unexpected: {err:?}"
        );
    }
}
