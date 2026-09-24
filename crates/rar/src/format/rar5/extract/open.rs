//! RAR5 block scanning, quick-open resolution and catalog building.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::engine::{ArchiveEntry, Engine, StreamRecord};
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{
    ArchiveHeader, BlockMeta, RawBlock, parse_service_block_name, quick_open,
};
use crate::format::rar5::{
    BLOCK_FLAG_DATA_CONTINUE_TO, BLOCK_FLAG_DATA_CONTINUES, BLOCK_TYPE_ARCHIVE_HEADER,
    BLOCK_TYPE_ENCRYPT_HEADER, BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_FILE_HEADER,
    BLOCK_TYPE_SERVICE_HEADER, MAX_METADATA_BYTES, RAR5_SIGNATURE,
};
use crate::format::shared::extract::{
    MAX_CATALOG_ENTRIES, MAX_MEMBER_CHUNKS, check_chunk_cap, check_entry_cap,
};
use crate::model::{DataChunk, FileHeader};

/// Builds the member catalog from one or more volume sources.
///
/// One block walk serves single-volume archives and volume sets: a
/// single-volume archive is a one-element source list, so continuation
/// merging, the entry/chunk ceilings and "STM" ownership behave identically
/// by volume count.
struct CatalogBuilder {
    entries: Vec<ArchiveEntry>,
    streams: Vec<StreamRecord>,
    pending: Option<ArchiveEntry>,
    /// Owner of a "STM" record that may appear after its member's final
    /// chunk (in any volume).
    last_file_index: Option<usize>,
    archive_solid: bool,
    max_entries: usize,
    max_chunks: usize,
    /// Set when a salvage scan had to resync past a corrupt block.
    damaged: bool,
}

impl CatalogBuilder {
    fn new(max_entries: usize, max_chunks: usize) -> Self {
        Self {
            entries: Vec::new(),
            streams: Vec::new(),
            pending: None,
            last_file_index: None,
            archive_solid: false,
            max_entries,
            max_chunks,
            damaged: false,
        }
    }

    /// Walk one source (a volume file, or the single-volume archive stream)
    /// positioned right after its signature. `volume_index` is recorded on
    /// every chunk so member data is read back from the right volume;
    /// `volume_len` bounds declared data areas against the real file.
    ///
    /// Returns when the source's end-of-archive block is reached, when a
    /// declared data area runs past the source, or at EOF.
    ///
    /// With `salvage`, a corrupt plaintext block header does not abort the
    /// walk: the scanner resyncs to the next structurally valid block and
    /// keeps whatever still parses, so a damaged archive yields the members
    /// around the damage (WinRAR's `rar r` behavior). Header-encrypted streams
    /// (where the block key is known) and real I/O errors still abort.
    fn scan_source<R: Read + Seek>(
        &mut self,
        stream: &mut R,
        volume_index: usize,
        volume_len: u64,
        password: Option<&str>,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        salvage: bool,
    ) -> RarResult<()> {
        // None until this source's plaintext archive-level encryption header
        // arrives (header-encrypted archives: every block after it is
        // `[IV][AES-256-CBC header]`; volume sets repeat the header on every
        // volume, so the key is re-derived per source).
        let mut encr_key: Option<[u8; 32]> = None;

        loop {
            let block_start = stream.stream_position()?;
            let meta = match crate::format::rar5::headers::read_block(stream, encr_key.as_ref()) {
                Ok(Some(meta)) => meta,
                Ok(None) => return Ok(()),
                Err(error) => {
                    let salvageable =
                        salvage && encr_key.is_none() && !matches!(error, RarError::Io(_));
                    if !salvageable {
                        return Err(error);
                    }
                    // A corrupt block invalidates any pending continuation.
                    self.pending = None;
                    self.damaged = true;
                    match crate::format::rar5::headers::resync_plain_block(
                        stream,
                        block_start + 1,
                        volume_len,
                    )? {
                        Some(meta) => meta,
                        None => return Ok(()),
                    }
                }
            };
            if crate::engine::cancel_requested(cancel) {
                return Err(RarError::Cancelled);
            }
            let raw = &meta.raw;
            let stream_pos = stream.stream_position()?;

            match raw.block_type {
                BLOCK_TYPE_ARCHIVE_HEADER => {
                    let ah = ArchiveHeader::from_raw(raw)?;
                    if ah.flags & crate::format::rar5::ARCHIVE_FLAG_SOLID != 0 {
                        self.archive_solid = true;
                    }
                }
                BLOCK_TYPE_FILE_HEADER => {
                    check_entry_cap(self.entries.len(), self.max_entries)?;
                    let fh = FileHeader::from_raw(raw, stream_pos)?;
                    let continues_from = raw.flags & BLOCK_FLAG_DATA_CONTINUES != 0;
                    let continues_to = raw.flags & BLOCK_FLAG_DATA_CONTINUE_TO != 0;

                    let chunk = DataChunk {
                        volume_index,
                        data_offset: fh.data_offset,
                        packed_size: fh.packed_size,
                        crc32_val: fh.crc32_val,
                        is_final: !continues_to,
                        extra_data: fh.extra_data.clone(),
                    };

                    if continues_from {
                        if let Some(ref mut entry) = self.pending {
                            check_chunk_cap(
                                entry.chunks.len(),
                                self.max_chunks,
                                &entry.header.name,
                            )?;
                            entry.chunks.push(chunk);
                            if !continues_to {
                                // Final chunk: total packed size and the
                                // final chunk's CRC (MAC'd when encrypted).
                                // For encrypted members the final chunk also
                                // carries the full extra records (encryption
                                // with the hash-key MAC bit, BLAKE2sp hash,
                                // time); the reader must verify with those,
                                // so merge them in when present.
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
                                self.entries.push(self.pending.take().unwrap());
                                self.last_file_index = Some(self.entries.len() - 1);
                            }
                        }
                    } else if continues_to {
                        self.pending = Some(ArchiveEntry {
                            header: fh,
                            chunks: vec![chunk],
                        });
                    } else {
                        self.entries.push(ArchiveEntry {
                            header: fh,
                            chunks: vec![chunk],
                        });
                        self.last_file_index = Some(self.entries.len() - 1);
                    }
                }
                BLOCK_TYPE_SERVICE_HEADER
                    if raw.flags & crate::format::rar5::BLOCK_FLAG_DEPENDS_PREV != 0 =>
                {
                    // NTFS stream record ("STM") owned by the preceding
                    // member; the record can sit in a later volume than the
                    // start of that member's data, so both the owner index
                    // and the volume index are recorded.
                    let name = parse_service_block_name(&meta.raw.header_data)?;
                    if name.as_deref() == Some("STM")
                        && let Some(owner_index) = self.last_file_index
                    {
                        self.record_member_stream(&meta.raw, owner_index, volume_index)?;
                    }
                }
                BLOCK_TYPE_END_ARCHIVE => {
                    // Parsed for validation, like the volume-set walker
                    // always did; the walker stops at the end block.
                    let _ = crate::format::rar5::headers::EndOfArchiveHeader::from_raw(raw)?;
                    return Ok(());
                }
                BLOCK_TYPE_ENCRYPT_HEADER => {
                    encr_key = Some(crate::format::rar5::headers::derive_header_key(
                        raw, password,
                    )?);
                }
                _ => {}
            }

            if raw.data_size > 0
                && !crate::format::shared::seek_past_data_area(stream, meta.data_end, volume_len)?
            {
                return Ok(());
            }
        }
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
            self.streams.push(StreamRecord {
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
}

/// The archive's leading blocks: the parsed main archive header plus the
/// verbatim bytes of a leading plaintext encryption header (header-encrypted
/// archives), which rewrites re-emit.
pub(crate) struct MainHeader {
    /// The raw block envelope of the main archive header.
    pub(crate) meta: BlockMeta,
    /// The parsed archive-level fields.
    pub(crate) parsed: ArchiveHeader,
    /// Verbatim bytes of the leading plaintext encryption header, when the
    /// archive has one.
    pub(crate) encrypt_header: Option<Vec<u8>>,
}

/// Read the archive start: the optional plaintext encryption header
/// (verifying the password and keeping its key for the following blocks),
/// then the main archive header. `reader` may be positioned anywhere;
/// this seeks to the archive start (after any SFX stub) and leaves it
/// right after the main header.
///
/// Append, lock, rewrite planning and the locked check all read the start
/// through this opener, so the encryption branch and the missing-header
/// error exist once. A caller that already scanned an archive resets the
/// encryption state (`clear_archive_encryption`) before calling it.
pub(crate) fn read_main_header<R: Read + Seek>(
    cx: &mut dyn Engine,
    reader: &mut R,
) -> RarResult<MainHeader> {
    reader.seek(SeekFrom::Start(
        cx.sfx_offset() + RAR5_SIGNATURE.len() as u64,
    ))?;
    let missing = || RarError::format("archive is missing the main header");
    let first = crate::format::rar5::headers::read_block(
        reader,
        crate::format::rar5::extract::verify::archive_block_key(cx)?.as_ref(),
    )?
    .ok_or_else(missing)?;
    match first.block_type {
        BLOCK_TYPE_ENCRYPT_HEADER => {
            let params = crate::format::rar5::headers::parse_archive_encrypt_header(&first.raw)?;
            cx.handle_archive_encrypt_header(params)?;
            let meta = crate::format::rar5::headers::read_block(
                reader,
                crate::format::rar5::extract::verify::archive_block_key(cx)?.as_ref(),
            )?
            .ok_or_else(missing)?;
            if meta.block_type != BLOCK_TYPE_ARCHIVE_HEADER {
                return Err(missing());
            }
            let parsed = ArchiveHeader::from_raw(&meta.raw)?;
            Ok(MainHeader {
                meta,
                parsed,
                encrypt_header: Some(first.header_bytes),
            })
        }
        BLOCK_TYPE_ARCHIVE_HEADER => {
            let parsed = ArchiveHeader::from_raw(&first.raw)?;
            Ok(MainHeader {
                meta: first,
                parsed,
                encrypt_header: None,
            })
        }
        _ => Err(missing()),
    }
}

/// Full RAR5 scan: one catalog walk serves single-volume archives and
/// volume sets.
pub(crate) fn open_read_rar5(cx: &mut dyn Engine) -> RarResult<()> {
    rebuild_catalog(cx)
}

/// RAR5 quick-open: read only the main archive header, resolve the
/// quick-open record through the locator, and parse the cached file
/// headers. Falls back to a full scan when the archive has no usable
/// quick-open record (multi-volume, header-encrypted, no QO written,
/// or a corrupt record).
pub(crate) fn open_read_quick_rar5(cx: &mut dyn Engine) -> RarResult<()> {
    if cx.volume_paths().len() > 1 {
        return rebuild_catalog(cx);
    }
    if !try_quick_open_entries(cx)? {
        // The full scan starts at the archive start again, so the
        // leading plaintext blocks the quick-open probe consumed (e.g.
        // a -hp encryption header) are seen.
        rebuild_catalog(cx)?;
    }
    Ok(())
}

/// Try to populate the catalog from the quick-open record.
/// Returns `Ok(false)` when the archive has no usable record (the
/// caller falls back to the full scan). QO-specific corruption falls
/// back too; only genuine I/O errors propagate.
fn try_quick_open_entries(cx: &mut dyn Engine) -> RarResult<bool> {
    // Header-encrypted archives never carry a QO record, and reading
    // their main header would need the derived key — bail out early.
    let first = match crate::format::rar5::headers::read_block(cx.stream_mut()?, None)? {
        Some(meta) => meta,
        None => return Ok(false),
    };
    if first.block_type != BLOCK_TYPE_ARCHIVE_HEADER {
        return Ok(false);
    }
    let ah = ArchiveHeader::from_raw(&first.raw)?;
    if ah.flags & crate::format::rar5::ARCHIVE_FLAG_SOLID != 0 {
        cx.set_archive_solid(true);
    }
    let Some(qo_rel) = crate::format::rar5::headers::locator_quick_open_offset(&ah.extra_data)
    else {
        return Ok(false);
    };
    let qo_abs = cx
        .sfx_offset()
        .checked_add(RAR5_SIGNATURE.len() as u64)
        .and_then(|base| base.checked_add(qo_rel))
        .unwrap_or(u64::MAX);
    let stream = cx.stream_mut()?;
    stream.seek(SeekFrom::Start(qo_abs))?;
    // A corrupt QO block (bad CRC, malformed header) must fall back to
    // the full scan like a corrupt payload does; only I/O errors
    // propagate.
    let qo = match crate::format::rar5::headers::read_block(stream, None) {
        Ok(Some(qo)) => qo,
        Ok(None) => return Ok(false),
        Err(RarError::Io(error)) => return Err(RarError::Io(error)),
        Err(_) => return Ok(false),
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
            cx.replace_catalog(entries);
            cx.read_ctx_mut().quick_open_catalog = true;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Guarantee that the catalog came from a full block scan, so the
/// service records the quick-open payload does not cache ("STM" NTFS
/// streams) are discovered. No-op unless the catalog came from the
/// quick-open record. The scan reads headers only: payload areas are
/// skipped with seeks, never loaded.
///
/// The scan can reorder members relative to the cached catalog, and the
/// catalog token is deliberately *not* rotated here: [`crate::EntryId`]s
/// carry the member's packed-payload offset and are re-resolved by
/// identity (see `EntryId::resolve`), so IDs issued from the
/// cached listing survive the reorder while an ID whose member the scan
/// no longer contains still fails as stale.
pub(crate) fn ensure_full_catalog(cx: &mut dyn Engine) -> RarResult<()> {
    if !cx.read_ctx().quick_open_catalog {
        return Ok(());
    }
    rebuild_catalog(cx)
}

/// Rebuild the catalog from every volume (a single-volume archive is one
/// source): the single-volume stream is rewound to the archive start,
/// while volume sets are reopened from the volume paths.
fn rebuild_catalog(cx: &mut dyn Engine) -> RarResult<()> {
    rebuild_catalog_capped(cx, MAX_CATALOG_ENTRIES, MAX_MEMBER_CHUNKS, false)
}

/// [`rebuild_catalog`] tolerating corrupt block headers: the scanner resyncs
/// past them and the catalog keeps the members that still parse. Used by
/// `rar r`'s reconstruct fallback on a damaged archive.
pub(crate) fn rebuild_catalog_salvage(cx: &mut dyn Engine) -> RarResult<()> {
    rebuild_catalog_capped(cx, MAX_CATALOG_ENTRIES, MAX_MEMBER_CHUNKS, true)
}

/// [`rebuild_catalog`] with explicit entry/chunk ceilings. A crafted
/// volume set can keep adding FILE_HEAD blocks while the catalog grows
/// without bound (the headers on disk are tiny, the entry objects are
/// not), and a single continuing member is likewise bounded so its chunk
/// vector cannot grow without limit.
fn rebuild_catalog_capped(
    cx: &mut dyn Engine,
    max_entries: usize,
    max_chunks: usize,
    salvage: bool,
) -> RarResult<()> {
    let mut builder = CatalogBuilder::new(max_entries, max_chunks);

    if cx.volume_paths().len() > 1 {
        let volume_paths = cx.volume_paths().to_vec();
        for (vol_idx, vol_path) in volume_paths.iter().enumerate() {
            let mut stream = File::open(vol_path)?;
            // Bound declared data areas against this volume's real size
            // (see `seek_past_data_area`: an out-of-range skip seek fails
            // on Linux).
            let volume_len = stream.metadata().map_err(RarError::Io)?.len();

            // Verify signature. The first volume may be an SFX stub, so
            // the archive begins at `sfx_offset` there; later volumes
            // start at 0.
            if vol_idx == 0 && cx.sfx_offset() > 0 {
                stream.seek(SeekFrom::Start(cx.sfx_offset()))?;
            }
            let mut sig = [0u8; 8];
            stream.read_exact(&mut sig)?;
            if sig != *RAR5_SIGNATURE {
                return Err(RarError::format(format!(
                    "volume {} has bad signature",
                    vol_path.display()
                )));
            }
            builder.scan_source(
                &mut stream,
                vol_idx,
                volume_len,
                cx.password(),
                cx.cancel_flag(),
                salvage,
            )?;
        }
        // Keep the first volume open as the default stream.
        let primary = cx.volume_paths()[0].clone();
        cx.set_stream(Box::new(File::open(&primary)?));
    } else {
        // A rebuild always starts at the archive start, wherever the
        // stream was left (the quick-open probe and earlier scans move
        // it). Declared data areas are bounded against the real file: a
        // hostile vint size can exceed the filesystem's maximum offset,
        // where the skip seek fails on Linux instead of hitting EOF.
        let volume_len = crate::format::shared::stream_len(cx.stream_mut()?)?;
        let password = cx.password().map(str::to_owned);
        let cancel = cx.cancel_token();
        let start = cx.sfx_offset() + RAR5_SIGNATURE.len() as u64;
        cx.stream_mut()?.seek(SeekFrom::Start(start))?;
        builder.scan_source(
            cx.stream_mut()?,
            0,
            volume_len,
            password.as_deref(),
            cancel.as_deref(),
            salvage,
        )?;
    }

    cx.replace_catalog(builder.entries);
    let streams = builder.streams;
    cx.read_ctx_mut().streams = streams;
    cx.read_ctx_mut().quick_open_catalog = false;
    cx.read_ctx_mut().salvage_damaged = builder.damaged;
    let solid = cx.archive_solid() || builder.archive_solid;
    cx.set_archive_solid(solid);
    Ok(())
}

/// Parse a quick-open record payload into archive entries.
///
/// The payload layout lives in [`crate::format::rar5::headers::quick_open`],
/// which verifies the entry CRCs and returns `(relative offset, header
/// bytes)` pairs. Here the cached headers are parsed into [`ArchiveEntry`]s:
/// the `qo_abs` → data-area offset arithmetic, the entry/chunk ceilings and
/// the header-block checks stay on this side.
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
/// The ceiling is enforced inside the codec, before each entry is decoded.
fn parse_quick_open_payload_capped(
    payload: &[u8],
    qo_abs: u64,
    max_entries: usize,
) -> RarResult<Vec<ArchiveEntry>> {
    let catalog = quick_open::decode_payload(payload, max_entries)?;
    let mut entries = Vec::with_capacity(catalog.len());
    for (rel, header_bytes) in catalog {
        let raw = crate::format::rar5::headers::parse_block_bytes(&header_bytes)?;
        if raw.block_type != BLOCK_TYPE_FILE_HEADER {
            return Err(RarError::format("quick-open: unexpected block type"));
        }
        // The original file header sat `rel` bytes before the QO record;
        // its data area starts right after the header envelope.
        let header_abs = qo_abs.checked_sub(rel).ok_or_else(|| {
            RarError::format("quick-open: relative offset points past the archive start")
        })?;
        let data_offset = header_abs + header_bytes.len() as u64;
        // `stream_pos` carries the data-area offset, matching the full scan.
        let fh = FileHeader::from_raw(&raw, data_offset)?;
        let chunk = DataChunk {
            volume_index: 0,
            data_offset,
            packed_size: fh.packed_size,
            crc32_val: fh.crc32_val,
            is_final: true,
            extra_data: fh.extra_data.clone(),
        };
        entries.push(ArchiveEntry {
            header: fh,
            chunks: vec![chunk],
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::RarArchive;
    use crate::engine::discover_volumes;
    use crate::format::rar5::headers::EndOfArchiveHeader;
    use crate::vint;

    /// One quick-open entry, through the shared codec.
    fn qo_entry(header: &[u8], rel: u64) -> Vec<u8> {
        quick_open::encode_entry(rel, header)
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

        let err = crate::format::rar5::extract::open::rebuild_catalog_capped(
            &mut ar,
            1,
            MAX_MEMBER_CHUNKS,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");

        crate::format::rar5::extract::open::rebuild_catalog_capped(
            &mut ar,
            2,
            MAX_MEMBER_CHUNKS,
            false,
        )
        .unwrap();
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
        let err = crate::format::rar5::extract::open::rebuild_catalog_capped(
            &mut ar,
            MAX_CATALOG_ENTRIES,
            3,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");

        crate::format::rar5::extract::open::rebuild_catalog_capped(
            &mut ar,
            MAX_CATALOG_ENTRIES,
            4,
            false,
        )
        .unwrap();
        assert_eq!(
            ar.entries.len(),
            1,
            "the continuation blocks are one member"
        );
        assert_eq!(ar.entries[0].chunks.len(), 4);
    }

    /// The unified catalog walker merges `DATA_CONTINUES` headers for a
    /// single-volume archive exactly as for a volume set; the volume-count
    /// split used to yield one entry per header block here.
    #[test]
    fn single_volume_continuation_headers_merge_into_one_member() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one-volume-chunks.rar");
        std::fs::write(&path, crafted_continuation_archive(4)).unwrap();

        let ar = RarArchive::open(&path).unwrap();
        assert_eq!(ar.entries.len(), 1, "continuation blocks are one member");
        assert_eq!(ar.entries[0].chunks.len(), 4);
    }

    /// The one opener reads both plain and header-encrypted archives and
    /// reports the verbatim encryption header for rewrites.
    #[test]
    fn read_main_header_parses_plain_and_header_encrypted_archives() {
        let dir = tempfile::tempdir().unwrap();

        let plain = dir.path().join("plain.rar");
        {
            let mut ar =
                RarArchive::create_with_options(&plain, crate::options::CreateOptions::default())
                    .unwrap();
            ar.add_bytes("a.bin", b"a", 0).unwrap();
            ar.close().unwrap();
        }
        let mut ar = RarArchive::open(&plain).unwrap();
        let mut reader = File::open(&plain).unwrap();
        let main =
            crate::format::rar5::extract::open::read_main_header(&mut ar, &mut reader).unwrap();
        assert_eq!(main.meta.block_type, BLOCK_TYPE_ARCHIVE_HEADER);
        assert!(main.encrypt_header.is_none());
        assert!(!ar.header_encryption);

        let encrypted = dir.path().join("encrypted.rar");
        {
            let mut ar = RarArchive::create_with_options(
                &encrypted,
                crate::options::CreateOptions {
                    encrypt_headers: true,
                    password: Some("secret".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("a.bin", b"a", 0).unwrap();
            ar.close().unwrap();
        }
        let mut ar = RarArchive::open_with_password(&encrypted, "secret").unwrap();
        let bytes = std::fs::read(&encrypted).unwrap();
        let mut reader = File::open(&encrypted).unwrap();
        let main =
            crate::format::rar5::extract::open::read_main_header(&mut ar, &mut reader).unwrap();
        assert_eq!(main.meta.block_type, BLOCK_TYPE_ARCHIVE_HEADER);
        let encrypt = main.encrypt_header.expect("header-encrypted archive");
        assert_eq!(
            encrypt.as_slice(),
            &bytes[RAR5_SIGNATURE.len()..RAR5_SIGNATURE.len() + encrypt.len()],
            "the encryption header must round-trip verbatim"
        );
        assert!(ar.header_encryption);
    }

    /// An encrypted stream whose ENCR block is followed by a non-archive
    /// block must be rejected with the shared missing-main-header error.
    #[test]
    fn read_main_header_rejects_a_non_archive_second_block() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted = dir.path().join("encrypted.rar");
        {
            let mut ar = RarArchive::create_with_options(
                &encrypted,
                crate::options::CreateOptions {
                    encrypt_headers: true,
                    password: Some("secret".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("a.bin", b"a", 0).unwrap();
            ar.close().unwrap();
        }
        let mut ar = RarArchive::open_with_password(&encrypted, "secret").unwrap();
        let mut reader = File::open(&encrypted).unwrap();
        let enc = crate::format::rar5::extract::open::read_main_header(&mut ar, &mut reader)
            .unwrap()
            .encrypt_header
            .unwrap();

        // Encrypt a file header with the archive key so the block parses,
        // then place it where the main header should be.
        let file_header = FileHeader {
            name: "a.bin".into(),
            ..Default::default()
        }
        .to_bytes();
        let key = ar.archive_encr.as_ref().unwrap().get_key("secret").unwrap();
        let iv = [0x5Au8; 16];
        let mut bytes = RAR5_SIGNATURE.to_vec();
        bytes.extend_from_slice(&enc);
        bytes.extend_from_slice(&iv);
        bytes.extend_from_slice(&crate::crypto::encrypt_data(&file_header, &key, &iv));

        // Re-reading an already-keyed archive resets the encryption state
        // first (like the locked check): the leading ENCR block is plaintext
        // and must parse as such.
        ar.clear_archive_encryption();
        let mut reader = std::io::Cursor::new(bytes);
        let err = match crate::format::rar5::extract::open::read_main_header(&mut ar, &mut reader) {
            Err(error) => error,
            Ok(_) => panic!("a non-archive second block must be rejected"),
        };
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");
        assert!(err.to_string().contains("missing the main header"));
    }

    /// A stream whose first block is not the archive header is rejected by
    /// the opener (every caller shares this error path).
    #[test]
    fn read_main_header_requires_the_archive_header_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-main.rar");
        let mut bytes = RAR5_SIGNATURE.to_vec();
        bytes.extend_from_slice(
            &FileHeader {
                name: "a.bin".into(),
                ..Default::default()
            }
            .to_bytes(),
        );
        bytes.extend_from_slice(&EndOfArchiveHeader { flags: 0 }.to_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let mut ar = RarArchive::open(&path).unwrap();
        let mut reader = File::open(&path).unwrap();
        let err = match crate::format::rar5::extract::open::read_main_header(&mut ar, &mut reader) {
            Err(error) => error,
            Ok(_) => panic!("a stream without a main header must be rejected"),
        };
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");
        assert!(err.to_string().contains("missing the main header"));
    }

    /// A failed rebuild must leave the quick-open flag alone: clearing it up
    /// front would make a retry (`ensure_full_catalog`) a silent no-op over
    /// the stale quick-open catalog.
    #[test]
    fn failed_rebuild_keeps_the_quick_open_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("two.rar");
        {
            let mut ar =
                RarArchive::create_with_options(&path, crate::options::CreateOptions::default())
                    .unwrap();
            ar.add_bytes("a.bin", b"a", 0).unwrap();
            ar.add_bytes("b.bin", b"b", 0).unwrap();
            ar.close().unwrap();
        }
        let mut ar = RarArchive::open(&path).unwrap();
        ar.read_ctx_mut().quick_open_catalog = true;

        let err = crate::format::rar5::extract::open::rebuild_catalog_capped(
            &mut ar,
            1,
            MAX_MEMBER_CHUNKS,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");
        assert!(
            ar.read_ctx().quick_open_catalog,
            "the flag must survive a failed rebuild"
        );
    }

    #[test]
    fn chunk_cap_matches_the_bound() {
        assert!(check_chunk_cap(MAX_MEMBER_CHUNKS - 1, MAX_MEMBER_CHUNKS, "m").is_ok());
        assert!(check_chunk_cap(MAX_MEMBER_CHUNKS, MAX_MEMBER_CHUNKS, "m").is_err());
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
