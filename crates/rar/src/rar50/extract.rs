//! RAR5 extraction: opening, block scanning, listing and member decoding.
//!
//! Mirrors the reference layout's `rar50/extract.rs`: every read-side
//! operation on [RarArchive] lives here while the shared state definition
//! stays in the facade (`crate::archive`).

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::archive::{
    ArchiveEntry, DecryptedPayload, MAX_DICT_SIZE_LOG, RarArchive, StreamRecord, discover_volumes,
};
use crate::codec::DecoderState;
use crate::crypto;
use crate::detect::{SFX_SCAN_LIMIT, find_bytes};
use crate::error::{RarError, RarResult};
use crate::fs::atomic::{replace_file, temp_sibling_path};
use crate::fs::safe_path::sanitize_archive_path;
use crate::model::{DataChunk, FileHeader};
#[cfg(feature = "parallel")]
use crate::parallel::extraction_pool;
use crate::rar50::headers::*;
#[cfg(windows)]
use crate::rar50::write as archive_write;
use crate::rar50::*;

/// Memory budget (packed + unpacked) for the optional parallel extraction
/// path; larger archives stream sequentially to stay bounded.
#[cfg(feature = "parallel")]
const PARALLEL_BUFFER_LIMIT: u64 = 256 * 1024 * 1024;
/// Parallel extraction only engages for at least this many members...
#[cfg(feature = "parallel")]
const PARALLEL_MIN_MEMBERS: usize = 4;
/// ...and at least this much total unpacked data (Rayon overhead amortized).
#[cfg(feature = "parallel")]
const PARALLEL_MIN_UNPACKED: u64 = 64 * 1024 * 1024;

/// Write sink that computes CRC32 and optional BLAKE2sp over streamed
/// output.
struct IntegritySink<'a> {
    inner: &'a mut dyn Write,
    crc: crc32fast::Hasher,
    blake: Option<crate::rar50::blake2sp::Hasher>,
}

impl<'a> IntegritySink<'a> {
    fn new(inner: &'a mut dyn Write, want_blake: bool) -> Self {
        Self {
            inner,
            crc: crc32fast::Hasher::new(),
            blake: want_blake.then(crate::rar50::blake2sp::Hasher::new),
        }
    }

    fn finish(self) -> (u32, Option<[u8; 32]>) {
        (self.crc.finalize(), self.blake.map(|h| h.finalize()))
    }
}

impl Write for IntegritySink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        if let Some(h) = self.blake.as_mut() {
            h.update(&buf[..n]);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

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
            let stream = self.stream.as_mut().unwrap();
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
        let first = match crate::rar50::headers::read_block(self.stream.as_mut().unwrap(), None)? {
            Some(meta) => meta,
            None => return Ok(false),
        };
        if first.block_type != BLOCK_TYPE_ARCHIVE_HEADER {
            return Ok(false);
        }
        let ah = ArchiveHeader::from_raw(&first.raw)?;
        let Some(qo_rel) = crate::rar50::headers::locator_quick_open_offset(&ah.extra_data) else {
            return Ok(false);
        };
        let qo_abs = self
            .sfx_offset
            .checked_add(RAR5_SIGNATURE.len() as u64)
            .and_then(|base| base.checked_add(qo_rel))
            .unwrap_or(u64::MAX);
        let stream = self.stream.as_mut().unwrap();
        stream.seek(SeekFrom::Start(qo_abs))?;
        let Some(qo) = crate::rar50::headers::read_block(stream, None)? else {
            return Ok(false);
        };
        if qo.block_type != BLOCK_TYPE_SERVICE_HEADER {
            return Ok(false);
        }
        // The QO payload must fit entirely in memory; cap at 64 MiB like
        // the reader's other bounded buffers.
        const QO_PAYLOAD_CAP: u64 = 64 * 1024 * 1024;
        if qo.raw.data_size > QO_PAYLOAD_CAP {
            return Ok(false);
        }
        stream.seek(SeekFrom::Start(qo.data_offset))?;
        let mut payload = Vec::with_capacity(qo.raw.data_size as usize);
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

    // ── Signature ──────────────────────────────────────────────────────────

    fn verify_signature(&mut self) -> RarResult<()> {
        // The signature must appear at the start for plain archives and
        // after the embedded stub for SFX archives (scan up to 8 MiB,
        // like the reference readers).
        let stream = self.stream.as_mut().unwrap();
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

    // ── Block scanning ─────────────────────────────────────────────────────

    fn scan_blocks(&mut self) -> RarResult<()> {
        self.entries.clear();
        self.read_ctx_mut().streams.clear();

        // None until the plaintext archive-level encryption header arrives
        // (header-encrypted archives: every block after it is `[IV][AES-256-
        // CBC header]`).
        let mut encr_key: Option<[u8; 32]> = None;
        let mut last_file_index: Option<usize> = None;

        while let Some(meta) =
            crate::rar50::headers::read_block(self.stream.as_mut().unwrap(), encr_key.as_ref())?
        {
            self.check_cancel()?;
            let raw = &meta.raw;
            let stream_pos = self.stream.as_mut().unwrap().stream_position()?;

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
                    if raw.flags & crate::rar50::BLOCK_FLAG_DEPENDS_PREV != 0 =>
                {
                    // NTFS stream record ("STM"): the SUBDATA extra holds
                    // the stream name (":name"), the data area the content.
                    let name = self.service_block_name(&meta)?;
                    if name.as_deref() == Some("STM")
                        && let Some(owner_index) = last_file_index
                    {
                        let extra = crate::rar50::headers::block_extra_area(&raw.header_data)?;
                        if let Some(stream_name) =
                            crate::rar50::headers::parse_service_subdata(&extra)
                            && !stream_name.is_empty()
                            && let Some((unpacked_size, method, dict_size_log)) =
                                crate::rar50::headers::parse_stream_params(&raw.header_data)
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
        let mut scan = crate::rar40::Rar4VolumeScan::default();
        let mut out = Vec::new();

        // Volume 0 is the already-open primary stream, positioned right
        // after the signature (SFX-aware). Later volumes open fresh and each
        // starts with its own 7-byte signature.
        scan.scan_volume(
            self.stream.as_mut().unwrap(),
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

            // Verify signature
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
                crate::rar50::headers::read_block(&mut stream, encr_key.as_ref())?
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

    // ── Public API: listing ────────────────────────────────────────────────

    /// Return all entries in the archive.
    pub fn list(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// Find an entry by name.
    pub fn get_entry(&self, name: &str) -> Option<&ArchiveEntry> {
        self.entries.iter().find(|e| e.name() == name)
    }

    /// Return a list of all entry names.
    pub fn namelist(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name()).collect()
    }

    // ── Public API: reading ────────────────────────────────────────────────

    /// Read and return the uncompressed content of a member.
    pub fn read(&mut self, name: &str) -> RarResult<Vec<u8>> {
        self.read_with_options(name, crate::options::ExtractOptions::default())
    }

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

    /// Extract all archive contents to `dest_dir` (safe defaults).
    pub fn extract_all(&mut self, dest_dir: impl AsRef<Path>) -> RarResult<()> {
        self.extract_all_with_options(dest_dir, crate::options::ExtractOptions::default())
    }

    /// Extract all archive contents with explicit options.
    pub fn extract_all_with_options(
        &mut self,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<()> {
        let dest = dest_dir.as_ref();
        fs::create_dir_all(dest)?;
        self.read_ctx_mut().extract_options = opts;

        #[cfg(feature = "parallel")]
        {
            if self.extract_all_parallel(dest, opts)? {
                return Ok(());
            }
        }

        let mut total_unpacked = 0u64;
        let entries: Vec<_> = self.entries.clone();
        for (index, entry) in entries.iter().enumerate() {
            self.check_cancel()?;
            total_unpacked = total_unpacked
                .checked_add(entry.header.unpacked_size)
                .ok_or_else(|| RarError::LimitExceeded {
                    limit: opts.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                    context: "total unpacked size overflow".into(),
                })?;
            if let Some(limit) = opts.max_total_unpacked_bytes
                && total_unpacked > limit
            {
                return Err(RarError::LimitExceeded {
                    limit,
                    context: format!(
                        "total unpacked size {total_unpacked} exceeds limit while extracting {}",
                        entry.name()
                    ),
                });
            }
            self.extract_entry(index, entry, dest)?;
        }
        Ok(())
    }

    /// Parallel extraction for eligible archives (optional `parallel`
    /// feature).
    ///
    /// Eligible: at least [`PARALLEL_MIN_MEMBERS`] members, no solid chains,
    /// no split/multi-volume members, no progress callback, and total packed
    /// + unpacked sizes within a bounded memory budget. Packed payloads are
    ///   read sequentially, then decoded and integrity-checked with Rayon
    ///   workers (archive order preserved by replaying writes sequentially
    ///   afterwards). Ineligible archives fall back to the sequential path
    ///   unchanged. The codec's decode is memory-bandwidth-bound, so the
    ///   parallel path mainly helps on machines where decompression is
    ///   CPU-bound; it engages only for member counts and sizes where Rayon
    ///   overhead is amortized.
    #[cfg(feature = "parallel")]
    fn extract_all_parallel(
        &mut self,
        dest: &Path,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<bool> {
        use rayon::prelude::*;

        if self.progress.is_some() || self.entries.len() < PARALLEL_MIN_MEMBERS {
            return Ok(false);
        }
        for (i, e) in self.entries.iter().enumerate() {
            if self.is_solid_chain_member(i) || e.chunks.len() != 1 {
                return Ok(false);
            }
        }
        let mut total_packed = 0u64;
        let mut total_unpacked = 0u64;
        for e in &self.entries {
            total_packed = total_packed.saturating_add(e.header.packed_size);
            total_unpacked = total_unpacked.saturating_add(e.header.unpacked_size);
            if total_packed > PARALLEL_BUFFER_LIMIT || total_unpacked > PARALLEL_BUFFER_LIMIT {
                return Ok(false);
            }
        }
        if total_unpacked < PARALLEL_MIN_UNPACKED {
            return Ok(false);
        }
        if let Some(limit) = opts.max_total_unpacked_bytes
            && total_unpacked > limit
        {
            return Err(RarError::LimitExceeded {
                limit,
                context: "total unpacked size exceeds limit".into(),
            });
        }

        // Phase 1: read + decrypt all payloads sequentially.
        let mut payloads: Vec<(usize, DecryptedPayload)> = Vec::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            payloads.push((i, self.read_packed_data(i)?));
        }
        let headers: Vec<FileHeader> = self.entries.iter().map(|e| e.header.clone()).collect();

        struct DecodedMember {
            idx: usize,
            data: Vec<u8>,
        }

        // Phase 2: decode + integrity-check in parallel.
        let results: Vec<RarResult<DecodedMember>> = extraction_pool().install(|| {
            payloads
                .into_par_iter()
                .map(|(i, payload)| {
                    let hdr = &headers[i];
                    if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
                        return Err(RarError::LimitExceeded {
                            limit: MAX_DICT_SIZE_LOG as u64,
                            context: format!(
                                "{}: dictionary size log {} exceeds supported maximum {}",
                                hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                            ),
                        });
                    }
                    if let Some(limit) = opts.max_unpacked_bytes
                        && hdr.unpacked_size > limit
                    {
                        return Err(RarError::LimitExceeded {
                            limit,
                            context: format!(
                                "{}: unpacked size {} exceeds limit",
                                hdr.name, hdr.unpacked_size
                            ),
                        });
                    }

                    let data = if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
                        Vec::new()
                    } else if hdr.comp_method == COMP_METHOD_STORE {
                        payload.data
                    } else {
                        crate::codec::decode_raw(
                            &payload.data,
                            hdr.unpacked_size,
                            crate::codec::DecodeOptions {
                                dict_size_log: hdr.comp_dict_size,
                                dict_size_bytes: hdr.dict_size_bytes,
                                variant: crate::version::ArchiveVersion::from_v70(
                                    hdr.dict_size_bytes.is_some(),
                                ),
                                state: None,
                            },
                        )?
                    };

                    let crc = crc32fast::hash(&data);
                    let blake = if hdr.hash_value.is_some() {
                        Some(crate::rar50::blake2sp::hash(&data))
                    } else {
                        None
                    };
                    verify_integrity_for(
                        hdr,
                        crc,
                        blake,
                        payload.params.as_ref(),
                        payload.keys.as_ref(),
                    )?;
                    Ok(DecodedMember { idx: i, data })
                })
                .collect()
        });

        // Phase 3: replay writes sequentially in archive order.
        for result in results {
            let member = result?;
            let entry = &self.entries[member.idx];
            let dest_path = self.safe_dest_path(dest, &entry.header.name)?;
            if entry.is_dir() {
                fs::create_dir_all(&dest_path)?;
                continue;
            }
            if let Some(redir) = parse_redirect_record(&entry.header.extra_data) {
                self.extract_redirection(dest, &dest_path, &redir)?;
                continue;
            }
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let tmp_path = temp_sibling_path(&dest_path);
            let write_result = (|| -> RarResult<()> {
                let mut file = File::create(&tmp_path)?;
                file.write_all(&member.data)?;
                file.flush()?;
                Ok(())
            })();
            match write_result {
                Ok(()) => replace_file(&tmp_path, &dest_path)?,
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path);
                    return Err(e);
                }
            }
            if entry.header.mtime != 0 || entry.header.mtime_ns.is_some() {
                self.apply_member_times(&entry.header, &dest_path);
            }
            self.extract_member_streams(member.idx, &dest_path)?;
        }
        Ok(true)
    }

    /// Extract a single entry to `dest_dir` (safe defaults).
    pub fn extract(&mut self, name: &str, dest_dir: impl AsRef<Path>) -> RarResult<PathBuf> {
        self.extract_with_options(name, dest_dir, crate::options::ExtractOptions::default())
    }

    /// Extract a single entry with explicit options.
    pub fn extract_with_options(
        &mut self,
        name: &str,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<PathBuf> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.name() == name)
            .ok_or_else(|| RarError::MemberNotFound {
                name: name.to_string(),
            })?;
        self.extract_at_index_with_options(idx, dest_dir, opts)
    }

    /// Extract an entry selected by its archive-order catalog index.
    pub(crate) fn extract_at_index_with_options(
        &mut self,
        idx: usize,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<PathBuf> {
        if idx >= self.entries.len() {
            return Err(RarError::InvalidState(
                "entry index is outside the current catalog".into(),
            ));
        }
        let dest = dest_dir.as_ref();
        fs::create_dir_all(dest)?;
        self.read_ctx_mut().extract_options = opts;
        self.validate_entry_limits(idx)?;
        self.extract_entry(idx, &self.entries[idx].clone(), dest)
    }

    /// Validate per-entry header limits against the current extract options.
    fn validate_entry_limits(&self, idx: usize) -> RarResult<()> {
        let hdr = &self.entries[idx].header;
        if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
            return Err(RarError::LimitExceeded {
                limit: MAX_DICT_SIZE_LOG as u64,
                context: format!(
                    "{}: dictionary size log {} exceeds supported maximum {}",
                    hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                ),
            });
        }
        if let Some(limit) = self.read_ctx().extract_options.max_unpacked_bytes
            && hdr.unpacked_size > limit
        {
            return Err(RarError::LimitExceeded {
                limit,
                context: format!(
                    "{}: unpacked size {} exceeds limit",
                    hdr.name, hdr.unpacked_size
                ),
            });
        }
        Ok(())
    }

    /// Extract one entry. File contents are decoded to a temporary file and
    /// renamed over the destination only after integrity checks pass, so a
    /// failure never leaves partial or corrupt output behind.
    fn extract_entry(
        &mut self,
        idx: usize,
        entry: &ArchiveEntry,
        dest_dir: &Path,
    ) -> RarResult<PathBuf> {
        self.validate_entry_limits(idx)?;

        // Flat extraction (`rar e` / `unrar e`): members land in the
        // destination directory under their basename. The safe-path policy
        // always applies here — the full member name is sanitized (which
        // rejects `..`/absolute/drive names) before its basename is used,
        // so traversal-shaped names cannot escape the destination.
        let dest_path = if self.read_ctx().extract_options.flat_paths {
            if entry.is_dir() {
                return Ok(dest_dir.to_path_buf());
            }
            let safe_name = sanitize_archive_path(&entry.header.name)?;
            let base = safe_name.rsplit('/').next().unwrap_or(&safe_name);
            dest_dir.join(base)
        } else {
            self.safe_dest_path(dest_dir, &entry.header.name)?
        };

        // `-o-` (skip existing): members whose destination already exists
        // are left untouched.
        if self.read_ctx().extract_options.skip_existing && dest_path.exists() {
            return Ok(dest_path);
        }

        // `-or` (auto rename): when the destination exists, insert `(N)`
        // before the extension (like WinRAR: a.txt -> a(1).txt).
        let mut dest_path = dest_path;
        if self.read_ctx().extract_options.auto_rename && !entry.is_dir() {
            let mut n = 1;
            while dest_path.exists() {
                let file_name = dest_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (stem, ext) = match file_name.rfind('.') {
                    Some(dot) if dot > 0 => (&file_name[..dot], &file_name[dot..]),
                    _ => (file_name.as_str(), ""),
                };
                dest_path = dest_path.with_file_name(format!("{stem}({n}){ext}"));
                n += 1;
            }
        }

        if entry.is_dir() {
            fs::create_dir_all(&dest_path)?;
            return Ok(dest_path);
        }

        // RAR5 redirect records (symlinks, hardlinks, file copies): the
        // entry carries no data, only the target reference.
        if let Some(redir) = parse_redirect_record(&entry.header.extra_data) {
            return self.extract_redirection(dest_dir, &dest_path, &redir);
        }

        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = temp_sibling_path(&dest_path);
        let result = (|| -> RarResult<u64> {
            let mut file = File::create(&tmp_path)?;
            let written = if self.rar4 {
                self.decode_rar4_to(idx, &mut file)?
            } else if self.is_solid_chain_member(idx) {
                self.decode_solid_through_to(idx, &mut file)?
            } else {
                self.decode_file_to(idx, &mut file, None)?
            };
            file.flush()?;
            Ok(written)
        })();

        match result {
            Ok(_) => {
                replace_file(&tmp_path, &dest_path)?;
            }
            Err(e) => {
                if self.read_ctx().extract_options.keep_broken {
                    // `-kb`: keep the partially extracted file.
                    let _ = replace_file(&tmp_path, &dest_path);
                } else {
                    let _ = fs::remove_file(&tmp_path);
                }
                return Err(e);
            }
        }

        // Restore mtime (best-effort), including the nanosecond fraction
        // from the FILE_TIME extra record when present.
        if entry.header.mtime != 0 || entry.header.mtime_ns.is_some() {
            self.apply_member_times(&entry.header, &dest_path);
        }
        // Restore NTFS alternate data streams attached to this member
        // (no-op on non-Windows, like the reference extractor).
        self.extract_member_streams(idx, &dest_path)?;

        Ok(dest_path)
    }

    /// Write the "STM" stream records attached to member `idx` onto the
    /// extracted file (`file:name`); Windows only.
    fn extract_member_streams(&mut self, idx: usize, dest_path: &Path) -> RarResult<()> {
        #[cfg(windows)]
        {
            use std::io::Read;
            let owned: Vec<StreamRecord> = self
                .read_ctx()
                .streams
                .iter()
                .filter(|s| s.owner_index == idx)
                .cloned()
                .collect();
            for s in owned {
                // Read the stream payload (possibly RAR5-compressed).
                let mut packed = vec![0u8; s.data_size as usize];
                {
                    let stream = self.stream.as_mut().unwrap();
                    stream.seek(SeekFrom::Start(s.data_offset))?;
                    stream.read_exact(&mut packed)?;
                }
                let data = if s.method == crate::rar50::COMP_METHOD_STORE {
                    packed
                } else {
                    crate::codec::decode_standalone(
                        &packed,
                        s.unpacked_size,
                        s.dict_size_log,
                        None,
                        crate::version::ArchiveVersion::Rar50,
                    )
                    .map_err(|e| RarError::Format(format!("stream decode: {e}")))?
                };
                archive_write::write_windows_stream(dest_path, &s.name, &data)?;
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (idx, dest_path);
        }
        Ok(())
    }

    /// Restore a member's stored timestamps on the extracted file: the
    /// modification time always (when nonzero), plus access time when
    /// requested via [`ExtractOptions`]. The creation time is set through
    /// `SetFileTime` on Windows (std has no creation-time setter) and is a
    /// `SetFileTime` on Windows (std has no creation-time setter) and is a
    /// no-op on Unix, where the change time cannot be set (matching
    /// WinRAR's behavior).
    fn apply_member_times(&self, hdr: &crate::model::FileHeader, dest_path: &Path) {
        let mut times = std::fs::FileTimes::new();
        let mut any = false;
        if hdr.mtime != 0 || hdr.mtime_ns.is_some() {
            let mut mtime = UNIX_EPOCH + std::time::Duration::from_secs(hdr.mtime as u64);
            if let Some(ns) = hdr.mtime_ns {
                mtime += std::time::Duration::from_nanos(ns as u64);
            }
            times = times.set_modified(mtime);
            any = true;
        }
        if self.read_ctx().extract_options.set_access_time
            && let Some((secs, ns)) = hdr.atime
        {
            let t = UNIX_EPOCH
                + std::time::Duration::from_secs(secs)
                + std::time::Duration::from_nanos(ns as u64);
            times = times.set_accessed(t);
            any = true;
        }
        if any {
            let _ = std::fs::File::options()
                .write(true)
                .open(dest_path)
                .and_then(|f| f.set_times(times));
        }
        #[cfg(windows)]
        if self.read_ctx().extract_options.set_creation_time
            && let Some((secs, ns)) = hdr.ctime
        {
            let _ = archive_write::windows_set_creation_time(dest_path, secs, ns);
        }
    }

    /// Materialize a RAR5 file redirection (symlink, hardlink or file
    /// copy) at `dest_path`.
    fn extract_redirection(
        &self,
        dest_dir: &Path,
        dest_path: &Path,
        redir: &RedirectSpec,
    ) -> RarResult<PathBuf> {
        const REDIR_UNIX_SYMLINK: u64 = 0x01;
        const REDIR_WINDOWS_SYMLINK: u64 = 0x02;
        const REDIR_WINDOWS_JUNCTION: u64 = 0x03;
        const REDIR_HARDLINK: u64 = 0x04;
        const REDIR_FILE_COPY: u64 = 0x05;
        match redir.redir_type {
            REDIR_UNIX_SYMLINK | REDIR_WINDOWS_SYMLINK | REDIR_WINDOWS_JUNCTION => {
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&redir.target, dest_path)?;
                }
                #[cfg(not(unix))]
                {
                    return Err(RarError::Unsupported(
                        "symbolic links are not supported on this platform".into(),
                    ));
                }
            }
            REDIR_HARDLINK | REDIR_FILE_COPY => {
                let target_path = self.safe_dest_path(dest_dir, &redir.target)?;
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                if redir.redir_type == REDIR_HARDLINK {
                    fs::hard_link(&target_path, dest_path)?;
                } else {
                    fs::copy(&target_path, dest_path)?;
                }
            }
            _ => {
                // Unknown redirection type: fall back to an empty regular
                // file so the archive remains extractable.
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(dest_path, [])?;
            }
        }
        Ok(dest_path.to_path_buf())
    }

    /// Compute the destination path for an entry name, applying the safe
    /// path policy (sanitization + canonical containment check).
    fn safe_dest_path(&self, dest_dir: &Path, name: &str) -> RarResult<PathBuf> {
        let sanitized = if self.read_ctx().extract_options.safe_paths {
            sanitize_archive_path(name)?
        } else {
            name.replace('\\', "/")
        };
        let dest_path = dest_dir.join(&sanitized);
        if self.read_ctx().extract_options.safe_paths
            && let Some(parent) = dest_path.parent()
        {
            fs::create_dir_all(parent)?;
            let canon_dest = dest_dir.canonicalize()?;
            let canon_parent = parent.canonicalize()?;
            if !canon_parent.starts_with(&canon_dest) {
                return Err(RarError::Security(format!(
                    "entry {name:?} resolves outside the destination directory"
                )));
            }
        }
        Ok(dest_path)
    }

    /// Check if entry at `idx` is in a solid chain (is solid itself, or
    /// the next entry after it is solid).
    fn is_solid_chain_member(&self, idx: usize) -> bool {
        let hdr = &self.entries[idx].header;
        if hdr.comp_solid {
            return true;
        }
        // First file in a solid group isn't flagged solid but the next one is
        if idx + 1 < self.entries.len() && self.entries[idx + 1].header.comp_solid {
            return true;
        }
        false
    }

    /// Reset RAR5 solid state to immediately before the current chain. Keeping
    /// the state and marker in lockstep is essential after a decoder or writer
    /// error because the local decoder may already have been partially mutated.
    fn reset_solid_decoder(&mut self, chain_start: usize) {
        let ctx = self.read_ctx_mut();
        ctx.solid_state = None;
        ctx.solid_decoded_through = chain_start as isize - 1;
    }

    /// Decode all files in the solid chain up through `target_idx`,
    /// returning the data for `target_idx`.
    fn decode_solid_through(&mut self, target_idx: usize) -> RarResult<Vec<u8>> {
        let mut target_data = Vec::new();
        self.decode_solid_through_to(target_idx, &mut target_data)?;
        Ok(target_data)
    }

    /// Find the start index of the solid chain containing `target_idx`
    /// (the first non-directory file at or before it that is not solid,
    /// followed by solid files).
    fn find_solid_chain_start(&self, target_idx: usize) -> usize {
        let mut chain_start = target_idx;
        for i in (0..target_idx).rev() {
            if self.entries[i].is_dir() {
                continue;
            }
            if self.entries[i].header.comp_solid || self.is_solid_chain_member(i) {
                chain_start = i;
            } else {
                break;
            }
        }
        chain_start
    }

    /// Streaming variant of [`Self::decode_solid_through`]: decodes the
    /// chain up to `target_idx`, writing only the target member to
    /// `writer` (intermediate members are decoded to a discard sink so the
    /// shared window advances).
    fn decode_solid_through_to(
        &mut self,
        target_idx: usize,
        writer: &mut dyn Write,
    ) -> RarResult<u64> {
        let chain_start = self.find_solid_chain_start(target_idx);

        let can_continue = {
            let ctx = self.read_ctx();
            ctx.solid_state.is_some()
                && ctx.solid_decoded_through >= chain_start as isize
                && ctx.solid_decoded_through < target_idx as isize
        };
        if !can_continue {
            self.reset_solid_decoder(chain_start);
        }

        if self.read_ctx().solid_state.is_none() {
            let dict_size = self.member_dict_window(chain_start)?;
            self.read_ctx_mut().solid_state = Some(DecoderState::new(dict_size));
        }

        let start_from = (self.read_ctx_mut().solid_decoded_through + 1) as usize;
        let mut target_written = 0u64;
        let mut discard = io::sink();

        for i in start_from..=target_idx {
            let entry = self.entries[i].clone();
            if entry.is_dir() {
                continue;
            }
            let sink: &mut dyn Write = if i == target_idx {
                writer
            } else {
                &mut discard
            };
            let mut state = self.read_ctx_mut().solid_state.take().unwrap();
            let written = match self.decode_file_to(i, sink, Some(&mut state)) {
                Ok(written) => written,
                Err(err) => {
                    self.reset_solid_decoder(chain_start);
                    return Err(err);
                }
            };
            self.read_ctx_mut().solid_state = Some(state);
            self.read_ctx_mut().solid_decoded_through = i as isize;
            if i == target_idx {
                target_written = written;
            }
        }

        Ok(target_written)
    }

    /// Read packed data for an entry, potentially across multiple volumes.
    ///
    /// The returned payload is decrypted (when applicable) together with
    /// the derived keys needed for integrity verification.
    fn read_packed_data(&mut self, idx: usize) -> RarResult<DecryptedPayload> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        let max_packed = self.max_packed_bytes();
        let password = self.password.as_deref();
        let cancel = &self.cancel;
        let mut reader = crate::rar50::payload::StreamReader {
            stream: self.stream.as_mut().unwrap(),
            volume_paths: &self.volume_paths,
        };
        crate::rar50::payload::read_packed(
            &mut reader,
            hdr,
            &entry.chunks,
            &hdr.name,
            password,
            max_packed,
            || {
                if cancel
                    .as_ref()
                    .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                {
                    return Err(RarError::Cancelled);
                }
                Ok(())
            },
        )
    }

    /// Maximum packed bytes accepted when the payload must be aggregated in
    /// memory. Bounded by the configured unpacked limit plus a small overhead,
    /// or a hard 8 GiB allocation guard when output is otherwise unlimited.
    pub(crate) fn max_packed_bytes(&self) -> u64 {
        self.read_ctx()
            .extract_options
            .max_unpacked_bytes
            .map(|u| u.saturating_add(1 << 20))
            .unwrap_or(8 * 1024 * 1024 * 1024)
    }

    /// Maximum packed bytes accepted by a truly streaming STORE path. With no
    /// unpacked limit there is no allocation-driven packed-size ceiling.
    fn max_stream_packed_bytes(&self) -> u64 {
        self.read_ctx()
            .extract_options
            .max_unpacked_bytes
            .map(|u| u.saturating_add(1 << 20))
            .unwrap_or(u64::MAX)
    }

    /// Decode a single file into memory, optionally with a shared
    /// DecoderState (solid archives), verifying CRC32/BLAKE2sp.
    fn decode_file_at(
        &mut self,
        idx: usize,
        state: Option<&mut DecoderState>,
    ) -> RarResult<Vec<u8>> {
        self.validate_entry_limits(idx)?;
        let hdr = &self.entries[idx].header;

        // Empty files / directories
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }

        let payload = self.read_packed_data(idx)?;
        let mut raw_data = Vec::new();
        crate::rar50::payload::decode_member(
            &self.entries[idx].header,
            &payload,
            state,
            &mut raw_data,
        )?;

        let crc = crc32fast::hash(&raw_data);
        let blake = self.entries[idx]
            .header
            .hash_value
            .map(|_| crate::rar50::blake2sp::hash(&raw_data));
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(raw_data)
    }

    /// Decode a single file, streaming output to `writer` (bounded memory),
    /// verifying CRC32/BLAKE2sp over the written bytes.
    /// Actual dictionary size of a member in bytes: RAR5 uses
    /// `128 KiB << comp_dict_size`, RAR7 carries the byte count directly
    /// (possibly non-power-of-two). The sliding window rounds up to a
    /// power of two. Enforces the extraction dictionary cap
    /// (`ExtractOptions::max_dict_size`, WinRAR's `-mdx`).
    fn member_dict_window(&self, idx: usize) -> RarResult<usize> {
        let hdr = &self.entries[idx].header;
        let bytes = match hdr.dict_size_bytes {
            Some(b) => b,
            None => (128u64 * 1024) << hdr.comp_dict_size,
        };
        if let Some(cap) = self.read_ctx().extract_options.max_dict_size
            && bytes > cap
        {
            return Err(RarError::LimitExceeded {
                limit: cap,
                context: format!(
                    "{}: dictionary size {bytes} bytes exceeds the extraction cap (use -mdx to raise it)",
                    hdr.name
                ),
            });
        }
        let bytes = usize::try_from(bytes)
            .map_err(|_| RarError::Format("dictionary size overflows host address space".into()))?;
        bytes
            .checked_next_power_of_two()
            .ok_or_else(|| RarError::Format("dictionary size overflows host address space".into()))
    }

    fn decode_file_to(
        &mut self,
        idx: usize,
        writer: &mut dyn Write,
        state: Option<&mut DecoderState>,
    ) -> RarResult<u64> {
        self.validate_entry_limits(idx)?;
        let hdr = &self.entries[idx].header;
        let _ = self.member_dict_window(idx)?; // enforces the -mdx cap
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(0);
        }

        let payload = self.read_packed_data(idx)?;
        let mut sink = IntegritySink::new(writer, self.entries[idx].header.hash_value.is_some());

        let written = crate::rar50::payload::decode_member(
            &self.entries[idx].header,
            &payload,
            state,
            &mut sink,
        )?;

        let (crc, blake) = sink.finish();
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(written)
    }

    /// Decode a single RAR4 member in memory, verifying its CRC32. Solid
    /// chain members decode through their chain prefix (shared window).
    fn decode_rar4_at(&mut self, idx: usize) -> RarResult<Vec<u8>> {
        self.validate_entry_limits(idx)?;
        let hdr = self.entries[idx].header.clone();
        if self.is_rar4_solid_member(idx) {
            let chain_start = self.rar4_find_chain_start(idx);
            let result = self
                .rar4_decode_solid_through(idx)
                .and_then(|out| self.rar4_verify_crc(&hdr, &out).map(|()| out));
            if result.is_err() {
                self.reset_rar4_solid_decoder(chain_start);
            }
            return result;
        }
        let out = self.rar4_decode_member(idx)?;
        self.rar4_verify_crc(&hdr, &out)?;
        Ok(out)
    }

    /// Decode a single RAR4 member, streaming output to `writer`, verifying
    /// its CRC32 over the written bytes. Non-chain members stream through
    /// the bounded-memory path (STORE chunks copied straight out; compressed
    /// members decode incrementally); solid-chain members keep the shared
    /// window semantics and decode in one pass.
    fn decode_rar4_to(&mut self, idx: usize, writer: &mut dyn Write) -> RarResult<u64> {
        self.validate_entry_limits(idx)?;
        let hdr = self.entries[idx].header.clone();
        if self.is_rar4_solid_member(idx) {
            let chain_start = self.rar4_find_chain_start(idx);
            let result = self.rar4_decode_solid_through(idx).and_then(|out| {
                self.rar4_verify_crc(&hdr, &out)?;
                writer.write_all(&out).map_err(RarError::Io)?;
                Ok(out.len() as u64)
            });
            if result.is_err() {
                self.reset_rar4_solid_decoder(chain_start);
            }
            return result;
        }
        let entry = self.entries[idx].clone();
        let max_alloc_packed_bytes = self.max_packed_bytes();
        let max_stream_packed_bytes = self.max_stream_packed_bytes();
        let (written, crc) = crate::rar40::decode_member_bytes_to(
            self.stream.as_mut().unwrap(),
            &self.volume_paths,
            &entry.chunks,
            &entry.header,
            crate::rar40::MemberDecodeOptions {
                password: self.password.as_deref(),
                decoder: None,
                max_alloc_packed_bytes,
                max_stream_packed_bytes,
            },
            writer,
        )?;
        // The streamed CRC is authoritative; compare with the header.
        if let Some(expected) = hdr.crc32_val
            && crc != expected
        {
            return Err(RarError::Crc {
                expected,
                actual: crc,
                context: format!("{}: CRC32 mismatch", hdr.name),
            });
        }
        Ok(written)
    }

    /// Decode RAR4 member `idx`, routing solid-chain members through the
    /// persistent legacy decoder so their look-behind window covers the
    /// chain prefix.
    fn rar4_decode_member(&mut self, idx: usize) -> RarResult<Vec<u8>> {
        if self.is_rar4_solid_member(idx) {
            return self.rar4_decode_solid_through(idx);
        }
        let entry = self.entries[idx].clone();
        let max_packed_bytes = self.max_packed_bytes();
        crate::rar40::decode_member_bytes(
            self.stream.as_mut().unwrap(),
            &self.volume_paths,
            &entry.chunks,
            &entry.header,
            crate::rar40::MemberDecodeOptions {
                password: self.password.as_deref(),
                decoder: None,
                max_alloc_packed_bytes: max_packed_bytes,
                max_stream_packed_bytes: max_packed_bytes,
            },
        )
    }

    /// Whether `idx` sits in a legacy solid run. RAR3+ members (unp_ver >=
    /// 29) chain on the per-file FHD_SOLID bit (a head member is solid by
    /// being directly followed by a flagged member). Pre-RAR3 codecs never
    /// write that bit: when the main header carried MHD_SOLID, every
    /// compressed member of such a codec is part of one shared-window run.
    fn is_rar4_solid_member(&self, idx: usize) -> bool {
        let hdr = &self.entries[idx].header;
        if hdr.unp_ver < 29 {
            return self.rar4_solid_archive && !self.entries[idx].is_dir();
        }
        if hdr.comp_solid {
            return true;
        }
        idx + 1 < self.entries.len() && self.entries[idx + 1].header.comp_solid
    }

    /// Find the start index of the legacy solid chain containing `idx` (the
    /// first member at or before it that is not solid, followed by solid
    /// members; directory entries do not break the run).
    fn rar4_find_chain_start(&self, target_idx: usize) -> usize {
        // Pre-RAR3 codecs under MHD_SOLID do not write FHD_SOLID. STORE
        // members leave the shared window untouched, so the run reaches back
        // across them to the first compressed member.
        if self.entries[target_idx].header.unp_ver < 29 {
            let mut chain_start = target_idx;
            for i in (0..target_idx).rev() {
                if !self.entries[i].is_dir()
                    && !crate::rar40::is_stored(self.entries[i].header.comp_method)
                {
                    chain_start = i;
                }
            }
            return chain_start;
        }

        // For RAR3+, FHD_SOLID belongs to the current member and means it
        // continues the previous non-directory member. Stop as soon as the
        // current chain head is unflagged; inspecting the previous member's
        // flag would incorrectly cross into an independent earlier run.
        let mut chain_start = target_idx;
        while self.entries[chain_start].header.comp_solid {
            let Some(previous) = (0..chain_start).rev().find(|&i| !self.entries[i].is_dir()) else {
                break;
            };
            if self.entries[previous].header.unp_ver < 29 {
                break;
            }
            chain_start = previous;
        }
        chain_start
    }

    /// Reset the legacy solid decoder to immediately before the current run.
    fn reset_rar4_solid_decoder(&mut self, chain_start: usize) {
        let ctx = self.read_ctx_mut();
        ctx.rar4_decoder = None;
        ctx.rar4_decoded_through = chain_start as isize - 1;
    }

    /// Decode the legacy solid chain up through `target_idx` with one shared
    /// decoder, returning the target member's bytes. Intermediate members are
    /// decoded only to advance the shared window. STORE members in a RAR2.x
    /// or RAR1.5 chain do not advance the window but do not break the chain
    /// either (the decoder is simply not called).
    fn rar4_decode_solid_through(&mut self, target_idx: usize) -> RarResult<Vec<u8>> {
        let chain_start = self.rar4_find_chain_start(target_idx);

        let start_from = {
            let ctx = self.read_ctx_mut();
            if ctx.rar4_decoder.is_some()
                && ctx.rar4_decoded_through >= chain_start as isize
                && ctx.rar4_decoded_through < target_idx as isize
            {
                // Continue from where we left off.
            } else {
                // Backwards request or a fresh chain: restart from this run's
                // head, not from unrelated members in an earlier solid run.
                ctx.rar4_decoder = None;
                ctx.rar4_decoded_through = chain_start as isize - 1;
            }
            if ctx.rar4_decoder.is_none() {
                // Bootstrap with a Rar29 decoder; it will be replaced on the
                // first compressed member that reveals the actual unp_ver.
                ctx.rar4_decoder = Some(crate::rar40::LegacyDecoder::Rar29(
                    crate::codec::rar29::Rar29Decoder::new(),
                ));
            }
            (ctx.rar4_decoded_through + 1) as usize
        };

        let mut target = Vec::new();
        for i in start_from..=target_idx {
            self.validate_entry_limits(i)?;
            let entry = self.entries[i].clone();
            if entry.is_dir() {
                continue;
            }
            let hdr = entry.header;
            let chunks = entry.chunks;

            // Determine decoder type from the member's unp_ver. A STORE
            // member keeps the existing decoder unchanged (for RAR2.x the
            // window is not advanced; for RAR1.5 likewise).
            let is_compressed = !crate::rar40::is_stored(hdr.comp_method);
            if is_compressed {
                // Ensure the decoder matches this member's codec version.
                let needs_rebuild = {
                    let dec = self.read_ctx_mut().rar4_decoder.as_ref();
                    match (hdr.unp_ver, dec) {
                        (v, Some(crate::rar40::LegacyDecoder::Rar29(_))) if v >= 29 => false,
                        (20 | 26, Some(crate::rar40::LegacyDecoder::Rar20(_))) => false,
                        (15, Some(crate::rar40::LegacyDecoder::Rar15(_))) => false,
                        _ => true,
                    }
                };
                if needs_rebuild {
                    let new_decoder = if hdr.unp_ver >= 29 {
                        crate::rar40::LegacyDecoder::Rar29(crate::codec::rar29::Rar29Decoder::new())
                    } else if hdr.unp_ver == 20 || hdr.unp_ver == 26 {
                        crate::rar40::LegacyDecoder::Rar20(Box::default())
                    } else {
                        crate::rar40::LegacyDecoder::Rar15(Box::default())
                    };
                    self.read_ctx_mut().rar4_decoder = Some(new_decoder);
                }
            }

            let mut decoder = self.read_ctx_mut().rar4_decoder.take();
            let max_packed_bytes = self.max_packed_bytes();
            let data = match crate::rar40::decode_member_bytes(
                self.stream.as_mut().unwrap(),
                &self.volume_paths,
                &chunks,
                &hdr,
                crate::rar40::MemberDecodeOptions {
                    password: self.password.as_deref(),
                    decoder: decoder.as_mut(),
                    max_alloc_packed_bytes: max_packed_bytes,
                    max_stream_packed_bytes: max_packed_bytes,
                },
            ) {
                Ok(data) => data,
                Err(err) => {
                    self.reset_rar4_solid_decoder(chain_start);
                    return Err(err);
                }
            };
            self.read_ctx_mut().rar4_decoder = decoder;
            self.read_ctx_mut().rar4_decoded_through = i as isize;
            if i == target_idx {
                target = data;
            }
        }
        Ok(target)
    }

    fn rar4_verify_crc(&self, hdr: &FileHeader, data: &[u8]) -> RarResult<()> {
        if let Some(expected) = hdr.crc32_val {
            let actual = crate::rar40::member_crc(data);
            if actual != expected {
                return Err(RarError::Crc {
                    expected,
                    actual,
                    context: format!("{}: CRC32 mismatch", hdr.name),
                });
            }
        }
        Ok(())
    }

    /// Verify CRC32 and BLAKE2sp integrity of decoded data. Encrypted
    /// members use the hash-key MAC when the encryption record requests it.
    pub(crate) fn verify_integrity(
        &self,
        idx: usize,
        crc: u32,
        blake: Option<[u8; 32]>,
        params: Option<&crypto::EncryptionParams>,
        keys: Option<&crypto::DerivedKeys>,
    ) -> RarResult<()> {
        let hdr = &self.entries[idx].header;
        verify_integrity_for(hdr, crc, blake, params, keys)
    }

    /// [`crate::rar50::headers::read_block`].
    pub(crate) fn archive_block_key(&self) -> RarResult<Option<[u8; 32]>> {
        let encr = match self.archive_encr.as_ref() {
            Some(encr) => encr,
            None => return Ok(None),
        };
        let password = match self.password.as_ref() {
            Some(password) => password,
            None => return Ok(None),
        };
        encr.get_key(password).map(Some)
    }
}

/// Verify CRC32 and BLAKE2sp integrity against a file header. Encrypted
/// members use the hash-key MAC when the encryption record requests it.
fn verify_integrity_for(
    hdr: &FileHeader,
    crc: u32,
    blake: Option<[u8; 32]>,
    params: Option<&crypto::EncryptionParams>,
    keys: Option<&crypto::DerivedKeys>,
) -> RarResult<()> {
    let uses_mac = params.is_some_and(|p| p.uses_hash_mac());

    if let Some(expected) = hdr.crc32_val {
        let mut actual = crc;
        if uses_mac {
            let keys = keys.ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: missing derived keys for MAC verification",
                    hdr.name
                ))
            })?;
            actual = keys.mac_crc32(actual);
        }
        if actual != expected {
            return Err(RarError::Crc {
                expected,
                actual,
                context: hdr.name.clone(),
            });
        }
    }

    if let (Some(expected), Some(actual)) = (hdr.hash_value, blake) {
        let actual = if uses_mac {
            let keys = keys.ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: missing derived keys for hash MAC verification",
                    hdr.name
                ))
            })?;
            keys.mac_hash32(actual)
        } else {
            actual
        };
        if !crypto::constant_time_eq(&expected, &actual) {
            return Err(RarError::HashMismatch {
                expected,
                actual,
                context: hdr.name.clone(),
            });
        }
    }
    Ok(())
}

// ── Quick-open payload ─────────────────────────────────────────────────────

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
        let raw = crate::rar50::headers::parse_block_bytes(&payload[p..hdr_end])?;
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
