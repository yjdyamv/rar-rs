//! Bounded-memory payload writers: STORE pass-through and the chunked
//! streaming compression path (spill file + on-the-fly encryption), plus
//! the Windows alternate-data-stream writer.
//!
//! Split out of `write/mod.rs`; the RAR5 pipeline reaches these through
//! the `pub(super)` entry points.

use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::Path;

use super::emit::{SplitParams, SplitPhase};
use super::engine::payload_stream;
use crate::archive::{ArchiveEntry, RarArchive};
use crate::codec::lzss_huff;
use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::format::rar5::{COMP_METHOD_STORE, FILE_FLAG_CRC32, FILE_FLAG_TIME_UNIX, OS_UNIX};
use crate::format::shared::engine::{
    CountingWriter, CrcSink, ProgressWriter, SpillGuard, spill_path_for,
};
use crate::format::shared::stream_mut;
use crate::model::{DataChunk, FileHeader};

#[cfg(windows)]
use super::windows;
#[cfg(windows)]
use crate::format::rar5::vint;

impl RarArchive {
    /// Stream a STORE member directly from a reader (bounded memory).
    ///
    /// Handles single-volume and multi-volume splitting. The plaintext CRC
    /// must be supplied (it is part of the header, written before data).
    /// Progress is reported per chunk (`bytes_written, unpacked_size`),
    /// matching the historical streaming behavior.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn write_stored_file(
        &mut self,
        name: &str,
        unpacked_size: u64,
        file_crc: u32,
        attrs: u64,
        mtime: u32,
        reader: &mut File,
        extra_data: &[u8],
        hash_value: Option<[u8; 32]>,
    ) -> RarResult<()> {
        self.write_streamed_payload(
            name,
            unpacked_size,
            unpacked_size,
            file_crc,
            attrs,
            mtime,
            COMP_METHOD_STORE,
            0,
            None,
            extra_data,
            hash_value,
            false,
            reader,
            unpacked_size,
            None,
            None,
            true,
        )
    }

    /// Stream a member payload (compressed data or STORE bytes) from a
    /// seekable reader into the archive with bounded memory, splitting
    /// across volumes when needed.
    ///
    /// When `encr` is set the payload is AES-256-CBC encrypted on the fly
    /// (the IV chain carries across chunk boundaries), the header checksum
    /// is MAC'd, and non-final volume chunks carry the CRC32 of their
    /// on-disk ciphertext bytes — matching WinRAR's per-volume records.
    /// `plain_len` is the number of plaintext bytes available in `reader`;
    /// `packed_size` is the total on-disk data size (the zero-padded
    /// ciphertext length when encrypted). `progress` enables per-chunk
    /// progress callbacks (historical STORE-path behavior); the compressed
    /// path reports progress during its compression pass instead.
    #[allow(clippy::too_many_arguments)]
    fn write_streamed_payload(
        &mut self,
        name: &str,
        unpacked_size: u64,
        packed_size: u64,
        file_crc: u32,
        attrs: u64,
        mtime: u32,
        method: u8,
        dict_size_log: u8,
        dict_size_bytes: Option<u64>,
        extra_data: &[u8],
        hash_value: Option<[u8; 32]>,
        solid: bool,
        reader: &mut File,
        plain_len: u64,
        encr: Option<&crypto::EncryptionParams>,
        password: Option<&str>,
        progress: bool,
    ) -> RarResult<()> {
        let fh_base = FileHeader {
            name: name.to_string(),
            unpacked_size,
            packed_size,
            attributes: attrs,
            mtime,
            crc32_val: Some(file_crc),
            hash_type: if hash_value.is_some() { 0 } else { u8::MAX },
            hash_value,
            comp_method: method,
            comp_solid: solid,
            comp_dict_size: dict_size_log,
            dict_size_bytes,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
            extra_data: extra_data.to_vec(),
            ..Default::default()
        };

        // Derive the AES key once per member. Two independent encryptors
        // are seeded with it: the probe pass (chunk CRC over the on-disk
        // ciphertext) and the write pass must produce identical bytes, so
        // they run separate chains from the same key and IV.
        let key_iv = match (encr, password) {
            (Some(params), Some(password)) => Some((params.get_key(password)?, params.iv)),
            (None, None) => None,
            _ => {
                return Err(RarError::Format(
                    "internal error: encryption parameters mismatch".into(),
                ));
            }
        };
        let mut write_src = payload_stream(&key_iv);
        let mut probe_src = payload_stream(&key_iv);

        if self.write_ctx().volume_size.is_none() {
            // ── Single-volume ──
            let hdr_bytes = fh_base.to_bytes();
            if self.write_ctx().quick_open {
                let pos = stream_mut(&mut self.stream)?.stream_position()?;
                self.write_ctx_mut()
                    .quick_open_entries
                    .push((pos, hdr_bytes.clone()));
            }
            self.write_block_header(&hdr_bytes)?;
            let written = {
                let stream = stream_mut(&mut self.stream)?;
                if progress {
                    let mut sink = ProgressWriter {
                        inner: stream,
                        total: unpacked_size,
                        written: 0,
                        member: self.progress_member,
                        progress: self.progress.clone(),
                    };
                    write_src.emit_to(reader, plain_len, 0, packed_size, &mut sink)?;
                    sink.written
                } else {
                    let mut counting = CountingWriter::new(stream);
                    write_src.emit_to(reader, plain_len, 0, packed_size, &mut counting)?;
                    counting.written()
                }
            };
            if written != packed_size {
                return Err(RarError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "file changed size while being archived: expected {packed_size} bytes, wrote {written}"
                    ),
                )));
            }
            let stream = stream_mut(&mut self.stream)?;
            let data_offset = stream.stream_position()? - packed_size;
            self.entries.push(ArchiveEntry {
                header: FileHeader {
                    data_offset,
                    ..fh_base
                },
                chunks: vec![DataChunk {
                    volume_index: 0,
                    data_offset,
                    packed_size,
                    crc32_val: Some(file_crc),
                    is_final: true,
                    extra_data: extra_data.to_vec(),
                }],
            });
            return Ok(());
        }

        // ── Multi-volume splitting ──
        let volume_size = self.write_ctx().volume_size.unwrap();
        // End-of-archive block: 8 plaintext bytes, or `[IV][padded]` when
        // header encryption wraps every block.
        let eoa_size: u64 = self.on_disk_header_len(8);
        let params = SplitParams {
            name,
            unpacked_size,
            attrs,
            mtime,
            method,
            solid,
            dict_size_log,
            dict_size_bytes,
            extra_data,
        };
        self.write_split_member(
            packed_size,
            params,
            volume_size,
            eoa_size,
            fh_base,
            |this, phase, offset, chunk_size, is_last| match phase {
                SplitPhase::Crc => {
                    // For non-final chunks the header carries the CRC of this
                    // chunk's on-disk bytes (the ciphertext when encrypted),
                    // computed in a probe pass with an independent encryptor
                    // chain so the write pass below is not disturbed.
                    if is_last {
                        Ok(file_crc as u64)
                    } else {
                        let mut h = crc32fast::Hasher::new();
                        let mut sink = CrcSink(&mut h);
                        probe_src.emit_to(
                            reader,
                            plain_len,
                            offset,
                            offset + chunk_size,
                            &mut sink,
                        )?;
                        Ok(h.finalize() as u64)
                    }
                }
                SplitPhase::Write => {
                    {
                        let stream = stream_mut(&mut this.stream)?;
                        if progress {
                            let mut sink = ProgressWriter {
                                inner: stream,
                                total: unpacked_size,
                                written: offset,
                                member: this.progress_member,
                                progress: this.progress.clone(),
                            };
                            write_src.emit_to(
                                reader,
                                plain_len,
                                offset,
                                offset + chunk_size,
                                &mut sink,
                            )?;
                        } else {
                            write_src.emit_to(
                                reader,
                                plain_len,
                                offset,
                                offset + chunk_size,
                                &mut *stream,
                            )?;
                        }
                    }
                    let stream = stream_mut(&mut this.stream)?;
                    let data_offset = stream.stream_position()? - chunk_size;
                    Ok(data_offset)
                }
            },
        )
    }

    /// Stream a STORE member directly from disk (bounded memory),
    /// encrypting on the fly when a password is set.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn write_store_member(
        &mut self,
        path: &Path,
        name: &str,
        file_size: u64,
        header_crc: u32,
        extra_data: &[u8],
        stored_hash: Option<[u8; 32]>,
        encr_params: Option<&crypto::EncryptionParams>,
        attrs: u64,
        mtime: u32,
        dict_size_bytes: Option<u64>,
    ) -> RarResult<()> {
        let mut reader = File::open(path)?;
        let password = self.password.clone();
        match (password.as_deref(), encr_params) {
            (Some(password), Some(params)) => self.write_streamed_payload(
                name,
                file_size,
                crypto::zero_padded_len(file_size),
                header_crc,
                attrs,
                mtime,
                COMP_METHOD_STORE,
                0,
                dict_size_bytes,
                extra_data,
                stored_hash,
                false,
                &mut reader,
                file_size,
                Some(params),
                Some(password),
                false,
            ),
            (None, None) => self.write_stored_file(
                name,
                file_size,
                header_crc,
                attrs,
                mtime,
                &mut reader,
                extra_data,
                stored_hash,
            ),
            _ => Err(RarError::Format(
                "internal error: encryption parameters mismatch".into(),
            )),
        }
    }

    /// Compress a large file (≥ [`STREAM_COMPRESS_THRESHOLD`]) with bounded
    /// memory: the input is read and compressed in bounded chunks (with the
    /// persistent encoder state), spilling the compressed stream to a
    /// temporary file; once the packed size and plaintext checksums are
    /// known, the member header is written and the spill is streamed into
    /// the archive — encrypting on the fly when a password is set. Falls
    /// back to streaming STORE when compression does not shrink the
    /// payload.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_file_streaming(
        &mut self,
        path: &Path,
        name: &str,
        file_size: u64,
        attrs: u64,
        mtime: u32,
        time_extra: Option<Vec<u8>>,
        owner_extra: Option<Vec<u8>>,
        method: u8,
        dsl: u8,
        dict_bytes: Option<u64>,
    ) -> RarResult<()> {
        // A persistent encoder state carries the LZ window (tail and the
        // long-range match history) across chunks of one member — even in
        // non-solid archives, where the window is reset between members.
        // This is what makes >64 KiB match distances (WinRAR `-mcl`
        // long range search) work for large files.
        // WinRAR `-se`: reset the solid statistics when the extension changes.
        self.maybe_reset_solid_for_extension(name);

        // Automatic delta and x86 (E8/E8E9) filters for large members:
        // decide both on the leading 64 KiB sample (the same head sample
        // the in-memory path packs), then each window is forward-transformed
        // in place before compression. A filtered member is written
        // standalone — its window holds transformed bytes, so the decoder
        // window must never seed the next member — breaking the solid chain.
        // The sample gate compares filter vs plain LZSS at sample scale
        // (a whole-member comparison is impossible without a second
        // read+encode pass); member-relative region records keep positions
        // correct and the `packed_size` guard below still protects against
        // STORE. x86 regions detected in the sample are extended to the
        // full file size (the E8/E8E9 encoder only touches actual opcodes
        // within the region, so non-opcode bytes pass through unchanged).
        let mut delta_channels: Option<u8> = None;
        let mut x86_filter_type: Option<u8> = None;
        let mut x86_regions: Vec<std::ops::Range<usize>> = Vec::new();
        if file_size < u32::MAX as u64 {
            let sample_len = ((64 * 1024) as u64).min(file_size) as usize;
            let mut sample = vec![0u8; sample_len];
            let got = {
                let mut sf = File::open(path)?;
                sf.read(&mut sample)?
            };
            sample.truncate(got);
            if got > 0 {
                // Try delta filter first (cheap pre-gate on sample).
                if crate::codec::common::filters::auto_delta_filter_channels(&sample).is_some() {
                    delta_channels = lzss_huff::pick_delta_channel(
                        &sample,
                        method,
                        dsl,
                        crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                    )?;
                }
                // Try x86 filter when delta did not win (same ordering as
                // the in-memory path: real x86 code is not multi-channel-
                // correlated, so the cheap delta scan returns None and we
                // fall through; for correlated audio/raw the delta filter
                // wins outright).
                if delta_channels.is_none() && got > 5 {
                    let mut regions_e9 =
                        crate::codec::common::filters::auto_x86_filter_ranges(&sample, true);
                    if !regions_e9.is_empty() {
                        // Merge overlapping/adjacent ranges to avoid
                        // double-transforming the overlap.
                        lzss_huff::merge_ranges(&mut regions_e9);
                        let mut regions_e8 =
                            crate::codec::common::filters::auto_x86_filter_ranges(&sample, false);
                        if regions_e8.is_empty() {
                            // Only E8E9 variant exists.
                            x86_filter_type = Some(lzss_huff::FILTER_E8E9);
                            x86_regions = regions_e9;
                        } else {
                            lzss_huff::merge_ranges(&mut regions_e8);
                            if regions_e8 == regions_e9 {
                                // E8 and E8E9 detect the same regions.
                                x86_filter_type = Some(lzss_huff::FILTER_E8E9);
                                x86_regions = regions_e9;
                            } else {
                                // Decide E8 vs E8E9 by compressed size on
                                // the sample (same as
                                // encode_with_auto_x86_filter).
                                let variant =
                                    crate::version::ArchiveVersion::from_v70(dict_bytes.is_some());
                                let sample_specs_e9: Vec<lzss_huff::FilterSpec> = regions_e9
                                    .iter()
                                    .map(|r| {
                                        lzss_huff::FilterSpec::new(
                                            lzss_huff::FILTER_E8E9,
                                            0,
                                            r.start.min(u32::MAX as usize) as u32,
                                            r.len().min(u32::MAX as usize) as u32,
                                        )
                                    })
                                    .collect();
                                let sample_specs_e8: Vec<lzss_huff::FilterSpec> = regions_e8
                                    .iter()
                                    .map(|r| {
                                        lzss_huff::FilterSpec::new(
                                            lzss_huff::FILTER_E8,
                                            0,
                                            r.start.min(u32::MAX as usize) as u32,
                                            r.len().min(u32::MAX as usize) as u32,
                                        )
                                    })
                                    .collect();
                                let packed_e9 = lzss_huff::encode_with_filters(
                                    &sample,
                                    method,
                                    dsl,
                                    &sample_specs_e9,
                                    variant,
                                )?
                                .len();
                                let packed_e8 = lzss_huff::encode_with_filters(
                                    &sample,
                                    method,
                                    dsl,
                                    &sample_specs_e8,
                                    variant,
                                )?
                                .len();
                                if packed_e8 < packed_e9 {
                                    x86_filter_type = Some(lzss_huff::FILTER_E8);
                                    x86_regions = regions_e8;
                                } else {
                                    x86_filter_type = Some(lzss_huff::FILTER_E8E9);
                                    x86_regions = regions_e9;
                                }
                            }
                        }
                        // Extend sample regions to the full file size:
                        // the E8/E8E9 encoder only touches actual opcodes,
                        // so non-opcode bytes within the extended region
                        // pass through unchanged.
                        for r in &mut x86_regions {
                            r.end = r.end.max(file_size as usize);
                        }
                    }
                }
            }
        }
        let delta_used = delta_channels.is_some();
        let x86_used = x86_filter_type.is_some();
        if delta_used || x86_used {
            self.reset_solid_chain();
        }

        let chain_solid = self.write_ctx().solid_mode && self.write_ctx().encoder_state.is_some();
        self.write_ctx_mut()
            .encoder_state
            .get_or_insert_with(Default::default);
        // Each member starts its own frame; see `EncoderState::begin_member`.
        self.write_ctx_mut()
            .encoder_state
            .as_mut()
            .expect("encoder state seeded")
            .begin_member();

        let mut crc_hasher = crc32fast::Hasher::new();
        let mut blake_hasher = if self.write_ctx().blake2 {
            Some(crate::format::rar5::blake2sp::Hasher::new())
        } else {
            None
        };
        let mut bytes_read = 0u64;
        let mut packed_size = 0u64;
        let threads = self.effective_threads();
        let cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> =
            self.cancel.clone();
        let cancel_ref = cancel_flag.as_deref();
        let spill_path = spill_path_for(&self.path);
        let _spill_guard = SpillGuard(spill_path.clone());
        {
            let mut spill = File::create(&spill_path)?;

            /// Encode one buffered window to the spill file. With the
            /// `parallel` feature and enough data, the window is split
            /// across the compression pool's workers; otherwise it falls
            /// back to the byte-for-byte sequential chunk loop.
            /// `window_start` is this window's first byte as a running
            /// chain position (0 for the first window); `filter_specs`
            /// carries the streaming filter regions (delta and/or x86)
            /// of this window. Their records ("filter symbols") lead the
            /// window's first emitted block, and their positions are
            /// written relative to `window_start` because the decoder
            /// adds its current write position when it reads each record.
            #[allow(clippy::too_many_arguments)]
            fn flush_window(
                work: &mut Vec<u8>,
                is_final: bool,
                threads: usize,
                method: u8,
                dsl: u8,
                dict_bytes: Option<u64>,
                state: &mut Option<crate::codec::EncoderState>,
                spill: &mut File,
                packed_size: &mut u64,
                cancel: Option<&std::sync::atomic::AtomicBool>,
                window_start: u64,
                filter_specs: Option<&[lzss_huff::FilterSpec]>,
            ) -> RarResult<()> {
                if work.is_empty() {
                    return Ok(());
                }
                let lead: Option<Vec<lzss_huff::Symbol>> = filter_specs.map(|specs| {
                    specs
                        .iter()
                        .map(|f| lzss_huff::Symbol::Filter {
                            block_start: f.block_start - window_start as u32,
                            block_length: f.block_length,
                            filter_type: f.filter_type,
                            channels: f.channels,
                        })
                        .collect()
                });
                #[cfg(not(feature = "parallel"))]
                let _ = threads;
                #[cfg(feature = "parallel")]
                const MT_MIN: usize = 3 * crate::codec::DEFAULT_CHUNK_SIZE;
                // Solid windows take the MT path as well: the shared state
                // still carries the previous window's tail and long-range
                // table into the parallel slices, so the chain survives —
                // only the parse tier (documented MT divergence) differs.
                #[cfg(feature = "parallel")]
                if work.len() >= MT_MIN && threads > 1 {
                    let packed = crate::codec::lzss_huff::encode_chunked_mt_with_progress(
                        work,
                        method,
                        dsl,
                        crate::codec::DEFAULT_CHUNK_SIZE,
                        state.get_or_insert_with(Default::default),
                        threads,
                        is_final,
                        crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                        lead.as_deref(),
                        None,
                        cancel,
                    )?;
                    spill.write_all(&packed)?;
                    *packed_size += packed.len() as u64;
                    work.clear();
                    return Ok(());
                }
                let mut offset = 0usize;
                while offset < work.len() {
                    if cancel.is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed)) {
                        return Err(RarError::Cancelled);
                    }
                    let end = (offset + crate::codec::DEFAULT_CHUNK_SIZE).min(work.len());
                    // The filter records lead only the first chunk's symbol
                    // stream (read before any output; the decoder holds them
                    // pending until each region is produced).
                    let compressed = if offset == 0 && lead.is_some() {
                        lzss_huff::encode_chunked_raw_with_lead(
                            &work[offset..end],
                            method,
                            dsl,
                            crate::codec::DEFAULT_CHUNK_SIZE,
                            state.as_mut(),
                            is_final && end >= work.len(),
                            None,
                            crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                            lead.as_deref(),
                        )?
                    } else {
                        lzss_huff::encode_chunked(
                            &work[offset..end],
                            lzss_huff::EncodeOptions {
                                chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                                state: state.as_mut(),
                                is_final: is_final && end >= work.len(),
                                variant: crate::version::ArchiveVersion::from_v70(
                                    dict_bytes.is_some(),
                                ),
                                skip_incompressible_probe: true,
                                ..lzss_huff::EncodeOptions::new(method, dsl)
                            },
                        )?
                    };
                    spill.write_all(&compressed)?;
                    *packed_size += compressed.len() as u64;
                    offset = end;
                }
                work.clear();
                Ok(())
            }

            #[cfg(feature = "parallel")]
            let mt_window =
                (threads.max(2) * 8 * 1024 * 1024).clamp(24 * 1024 * 1024, 64 * 1024 * 1024);
            #[cfg(not(feature = "parallel"))]
            let mt_window = 0usize;

            let mut work: Vec<u8> = Vec::new();
            let mut eof = false;
            let mut file = io::BufReader::with_capacity(1 << 20, File::open(path)?);
            let mut buf = vec![0u8; crate::codec::DEFAULT_CHUNK_SIZE];
            let mut member_offset = 0u64;
            while !eof {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    eof = true;
                } else {
                    bytes_read += n as u64;
                    crc_hasher.update(&buf[..n]);
                    if let Some(h) = blake_hasher.as_mut() {
                        h.update(&buf[..n]);
                    }
                    work.extend_from_slice(&buf[..n]);
                    self.report_progress(bytes_read, file_size);
                }
                if eof || work.len() >= mt_window {
                    let flushed = work.len() as u64;
                    // Streaming filters: apply delta (full window, lane-
                    // blocked) then x86 (clipped to detected regions) on
                    // the already-delta-transformed data. The decoder
                    // applies inverse transforms in symbol-stream order
                    // (delta first, then x86), which reverses the encode
                    // order — both are linear and region-independent so
                    // composition is correct regardless of overlap.
                    let mut window_specs: Option<Vec<lzss_huff::FilterSpec>> = None;
                    if let Some(ch) = delta_channels {
                        let (transformed, specs) =
                            lzss_huff::delta_stream_window(&work, member_offset, ch);
                        work = transformed;
                        window_specs = Some(specs);
                    }
                    if let Some(ft) = x86_filter_type {
                        let (transformed, specs) =
                            lzss_huff::x86_stream_window(&work, member_offset, ft, &x86_regions);
                        work = transformed;
                        if let Some(ref mut existing) = window_specs {
                            existing.extend(specs);
                        } else {
                            window_specs = Some(specs);
                        }
                    }
                    flush_window(
                        &mut work,
                        eof,
                        threads,
                        method,
                        dsl,
                        dict_bytes,
                        &mut self.write_ctx_mut().encoder_state,
                        &mut spill,
                        &mut packed_size,
                        cancel_ref,
                        member_offset,
                        window_specs.as_deref(),
                    )?;
                    member_offset += flushed;
                    if packed_size >= file_size {
                        break;
                    }
                }
            }
        }

        let plain_crc = crc_hasher.finalize();
        let plain_blake = blake_hasher.map(|h| h.finalize());

        if packed_size >= file_size {
            // Compression is a net loss: fall back to streaming STORE.
            self.reset_solid_chain();
            let (header_crc, mut extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            if let Some(ref t) = time_extra {
                extra_data.extend_from_slice(t);
            }
            if let Some(ref t) = owner_extra {
                extra_data.extend_from_slice(t);
            }
            self.write_store_member(
                path,
                name,
                file_size,
                header_crc,
                &extra_data,
                stored_hash,
                encr_params.as_ref(),
                attrs,
                mtime,
                dict_bytes,
            )?;
            self.report_progress(file_size, file_size);
            return Ok(());
        }

        let (header_crc, mut extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        if let Some(ref t) = time_extra {
            extra_data.extend_from_slice(t);
        }
        if let Some(ref t) = owner_extra {
            extra_data.extend_from_slice(t);
        }
        let mut spill = File::open(&spill_path)?;
        let password = self.password.clone();
        // Encrypted members store the zero-padded ciphertext length in
        // the header and on disk (the streaming encryptor pads the final
        // partial block); plain members store the packed length as-is.
        let (packed_size, plain_len) = match encr_params {
            Some(_) => (crypto::zero_padded_len(packed_size), packed_size),
            None => (packed_size, packed_size),
        };
        self.write_streamed_payload(
            name,
            file_size,
            packed_size,
            header_crc,
            attrs,
            mtime,
            method,
            dsl,
            dict_bytes,
            &extra_data,
            stored_hash,
            chain_solid,
            &mut spill,
            plain_len,
            encr_params.as_ref(),
            password.as_deref(),
            false,
        )?;
        self.write_member_streams(path)?;
        // Non-solid members use an independent LZ window: drop the
        // encoder state so the next member starts fresh. A delta/x86-
        // filtered member is also standalone (its window holds transformed
        // bytes, which must never seed the next solid member).
        if !self.write_ctx().solid_mode || delta_used || x86_used {
            self.reset_solid_chain();
        }
        self.report_progress(file_size, file_size);
        Ok(())
    }

    /// Write the NTFS alternate data streams of `path` as "STM" service
    /// records right after the member's file block (WinRAR `-os`).
    pub(super) fn write_member_streams(&mut self, path: &Path) -> RarResult<()> {
        if !self.write_ctx().save_streams {
            return Ok(());
        }
        #[cfg(windows)]
        {
            for stream in windows::enumerate_windows_streams(path)? {
                // FindFirstStreamW returns names like "file:stream:$DATA"
                // (and "::$DATA" for the default stream); normalize to the
                // archive form ":stream".
                let raw_name = stream.0;
                if raw_name == "::$DATA" {
                    continue; // the default unnamed stream
                }
                let trimmed = raw_name.strip_suffix(":$DATA").unwrap_or(&raw_name);
                let stream_part = trimmed.rsplit_once(':').map(|(_, s)| s).unwrap_or(trimmed);
                let name = format!(":{stream_part}");
                // Read the stream payload through the `file:stream` path.
                let mut full = path.as_os_str().to_os_string();
                full.push(&name);
                let data = match std::fs::read(std::path::PathBuf::from(&full)) {
                    Ok(d) => d,
                    Err(_) => continue, // stream vanished mid-run
                };
                let subdata = {
                    let mut extra = Vec::new();
                    extra.extend(vint::encode((1 + name.len()) as u64));
                    extra.extend(vint::encode(crate::format::rar5::EXTRA_SERVICE_SUBDATA));
                    extra.extend(name.as_bytes());
                    extra
                };
                let hdr = crate::format::rar5::headers::build_service_block(
                    "STM",
                    &subdata,
                    data.len() as u64,
                    crate::format::rar5::BLOCK_FLAG_DEPENDS_PREV,
                );
                self.write_block_header(&hdr)?;
                let stream = stream_mut(&mut self.stream)?;
                stream.write_all(&data)?;
                self.write_ctx_mut().volume_bytes_written = self
                    .write_ctx()
                    .volume_bytes_written
                    .saturating_add(self.on_disk_header_len(hdr.len() as u64))
                    .saturating_add(data.len() as u64);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = path;
        }
        Ok(())
    }
}
