//! Bounded-memory payload writers: STORE pass-through and the chunked
//! streaming compression path (spill file + on-the-fly encryption), plus
//! the Windows alternate-data-stream writer.
//!
//! Split out of `write/mod.rs`; the RAR5 pipeline reaches these through
//! the `pub(super)` entry points.

use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::Path;

use super::emit::SplitPhase;
use super::engine::payload_stream;
use crate::codec::lzss_huff;
use crate::crypto;
use crate::engine::ArchiveEntry;
use crate::engine::Engine;
use crate::engine::MemberPlan;
use crate::error::{RarError, RarResult};
use crate::format::rar5::{COMP_METHOD_STORE, FILE_FLAG_CRC32, FILE_FLAG_TIME_UNIX};
use crate::format::shared::engine::{
    CountingWriter, CrcSink, ProgressWriter, SpillGuard, spill_path_for,
};
use crate::model::{DataChunk, FileHeader};
use crate::options::FilterMode;

#[cfg(windows)]
use super::windows;
use crate::vint;

/// Defensive guard: the streaming writer emits its delta and x86 filters as
/// pre-built `Symbol::Filter` leads, bypassing `encode_with_filters`' overlap
/// validation, and a reader applies records in stream order, so overlapping
/// records would corrupt the member. `-mcd+ -mce+` no longer reaches this
/// point (the window loop picks one filter per 64 KiB block through
/// `forced_combined_stream_window`), and auto-delta under a forced x86 is
/// skipped (`auto_delta_probe_enabled`); the check stays as a cheap invariant
/// against a future regression.
fn ensure_compatible_stream_filters(delta_used: bool, x86_used: bool) -> RarResult<()> {
    if delta_used && x86_used {
        return Err(RarError::InvalidOption(
            "cannot force the delta and x86 filters on the same member; choose one filter mode"
                .into(),
        ));
    }
    Ok(())
}

/// Whether the streaming writer runs its auto-delta probe. A forced x86
/// filter claims the whole member and mirrors the buffered `-mc` policy
/// (`filter_policy.rs`): the auto-delta selection is skipped instead of
/// aborting the add. A forced delta is selected in its own branch and never
/// consults this, so forced delta + forced x86 still hits the compatibility
/// check above.
fn auto_delta_probe_enabled(delta: FilterMode, x86: FilterMode) -> bool {
    delta == FilterMode::Auto && x86 != FilterMode::Forced
}

/// Read the member's probe windows (head and middle) and measure each filter
/// candidate with the shared gate in [`lzss_huff::filter_transform_wins`].
/// Reading the windows here (rather than probing in memory) is what lets the
/// streaming writer use the same gate as the buffered path even though it
/// never holds the member.
fn probe_filter_candidate<F>(
    path: &Path,
    file_size: u64,
    method: u8,
    dsl: u8,
    variant: crate::version::ArchiveVersion,
    transform: F,
) -> RarResult<Option<(usize, usize)>>
where
    F: Fn(&[u8], u64) -> Vec<u8>,
{
    let mut windows: Vec<(u64, Vec<u8>)> = Vec::with_capacity(2);
    for offset in [0u64, file_size / 2] {
        let len = crate::codec::lzss_huff::FILTER_PROBE_LEN
            .min(file_size.saturating_sub(offset) as usize);
        if len < 64 * 1024 {
            continue;
        }
        let mut window = vec![0u8; len];
        let mut file = File::open(path)?;
        file.seek(std::io::SeekFrom::Start(offset))?;
        file.read_exact(&mut window)?;
        windows.push((offset, window));
    }
    let probes: Vec<lzss_huff::FilterProbe<'_>> = windows
        .iter()
        .map(|(offset, bytes)| lzss_huff::FilterProbe {
            offset: *offset,
            bytes,
        })
        .collect();
    lzss_huff::filter_transform_wins(&probes, method, dsl, variant, transform)
}

/// Collapse the merged sample-detected x86 regions into the single
/// member-relative span the streaming writer filters: from the first
/// detected region to the end of the member.
///
/// Extending *every* detected region to `file_size` made the emitted
/// records overlap. The buffered writer's `validate_filter_specs` rejects
/// exactly that shape, the streaming decoder applies records in order (so
/// the overlap was transformed twice), and official WinRAR writes disjoint
/// records (observed: adjacent 64 KiB records). Keeping one span from the
/// first detection preserves the intent — once the leading sample looks
/// like x86 code, filter from there through EOF — while leaving the records
/// disjoint; [`crate::codec::lzss_huff::x86_stream_window`] splits the span
/// at `MAX_FILTER_BLOCK_LENGTH` per window, like WinRAR's records.
fn x86_region_span(regions: &mut Vec<std::ops::Range<usize>>, file_size: usize) {
    let Some(start) = regions.iter().map(|region| region.start).min() else {
        return;
    };
    regions.clear();
    regions.push(start..file_size);
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
fn write_streamed_payload(
    cx: &mut dyn Engine,
    plan: &MemberPlan,
    packed_size: u64,
    reader: &mut File,
    plain_len: u64,
    encr: Option<&crypto::MemberEncryption>,
    progress: bool,
) -> RarResult<()> {
    let file_crc = plan.file_crc;
    let (mtime, file_flags) =
        super::add::rar5_time_fields(cx, plan.mtime, FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32);
    let fh_base = plan.file_header(packed_size, mtime, file_flags);

    // Two independent encryptors are seeded from the session's key/IV:
    // the probe pass (chunk CRC over the on-disk ciphertext) and the
    // write pass must produce identical bytes, so they run separate
    // chains from the same key and IV.
    let key_iv = encr.map(|session| {
        let (key, iv) = session.key_iv();
        (*key, *iv)
    });
    let mut write_src = payload_stream(&key_iv);
    let mut probe_src = payload_stream(&key_iv);
    // The progress destination is read before the stream is borrowed: the
    // writer holds `stream_mut()` and the tracker at the same time, which
    // two `&mut dyn Engine` calls cannot.
    let (progress_member, progress_shared) = match cx.progress_slot() {
        Some((tracker, member)) => (member, Some(tracker)),
        None => (0, None),
    };

    if cx.write_ctx().output.volume_size.is_none() {
        // ── Single-volume ──
        let hdr_bytes = fh_base.to_bytes();
        if cx.write_ctx().locator.quick_open {
            let pos = cx.stream_mut()?.stream_position()?;
            cx.write_ctx_mut()
                .locator
                .quick_open_entries
                .push((pos, hdr_bytes.clone()));
        }
        cx.write_block_header(&hdr_bytes)?;
        let written = {
            let stream = cx.stream_mut()?;
            if progress {
                let mut sink = ProgressWriter {
                    inner: stream,
                    total: plan.unpacked_size,
                    written: 0,
                    member: progress_member,
                    progress: progress_shared.clone(),
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
        let stream = cx.stream_mut()?;
        let data_offset = stream.stream_position()? - packed_size;
        cx.entries_mut().push(ArchiveEntry {
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
                extra_data: plan.extra_data.clone(),
            }],
        });
        return Ok(());
    }

    // ── Multi-volume splitting ──
    let volume_size = cx.write_ctx().output.volume_size.unwrap();
    // End-of-archive block: 8 plaintext bytes, or `[IV][padded]` when
    // header encryption wraps every block.
    let eoa_size: u64 = cx.on_disk_header_len(8);
    super::emit::write_split_member(
        cx,
        packed_size,
        plan,
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
                    probe_src.emit_to(reader, plain_len, offset, offset + chunk_size, &mut sink)?;
                    Ok(h.finalize() as u64)
                }
            }
            SplitPhase::Write => {
                {
                    let stream = this.stream_mut()?;
                    if progress {
                        let mut sink = ProgressWriter {
                            inner: stream,
                            total: plan.unpacked_size,
                            written: offset,
                            member: progress_member,
                            progress: progress_shared.clone(),
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
                let stream = this.stream_mut()?;
                let data_offset = stream.stream_position()? - chunk_size;
                Ok(data_offset)
            }
        },
    )
}

/// Stream a STORE member directly from disk (bounded memory),
/// encrypting on the fly when a session is set.
pub(super) fn write_store_member(
    cx: &mut dyn Engine,
    path: &Path,
    mut plan: MemberPlan,
    encr: Option<&crypto::MemberEncryption>,
) -> RarResult<()> {
    let mut reader = File::open(path)?;
    // Encrypted members store the zero-padded ciphertext length in the
    // header and on disk (the streaming encryptor pads the final partial
    // block); plain members store the packed length as-is. Progress
    // callbacks run on the plain path (the encrypted path reports
    // nothing).
    let (packed_size, progress) = match encr {
        Some(_) => (crypto::zero_padded_len(plan.unpacked_size), false),
        None => {
            // A plain STORE member declares no byte dictionary: the
            // payload is stored, not decoded through a window.
            plan.dict_size_bytes = None;
            (plan.unpacked_size, true)
        }
    };
    let unpacked_size = plan.unpacked_size;
    write_streamed_payload(
        cx,
        &plan,
        packed_size,
        &mut reader,
        unpacked_size,
        encr,
        progress,
    )
}

/// Compress a large file (≥ [`crate::engine::STREAM_COMPRESS_THRESHOLD`])
/// with bounded memory: the input is read and compressed in bounded chunks
/// (with the persistent encoder state), spilling the compressed stream to a
/// temporary file; once the packed size and plaintext checksums are
/// known, the member header is written and the spill is streamed into
/// the archive — encrypting on the fly when a password is set. Falls
/// back to streaming STORE when compression does not shrink the
/// payload.
#[allow(clippy::too_many_arguments)]
pub(super) fn add_file_streaming(
    cx: &mut dyn Engine,
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
    crate::format::shared::write_ops::maybe_reset_solid_for_extension(cx, name);

    // Automatic delta and x86 (E8/E8E9) filters for large members: the
    // scanners pick candidates from the head sample, and each candidate
    // must then win a *measured* probe before it may transform the member
    // (see [`probe_filter_candidate`]) — a scanner hit on a 64 KiB head is
    // not evidence for 68 MiB of body, and a transform applied to the
    // wrong data costs far more than it saves. Each window is
    // forward-transformed in place before compression. A filtered member
    // is written standalone — its window holds transformed bytes, so the
    // decoder window must never seed the next member — breaking the solid
    // chain. Member-relative region records keep positions correct and the
    // `packed_size` guard below still protects against STORE. Once x86 code
    // is detected in the sample, the filtered span runs from the first
    // detected region through end-of-member (see [`x86_region_span`]): the
    // E8/E8E9 encoder only touches actual opcodes, so non-opcode bytes
    // within the span pass through unchanged.
    let filter_policy = cx.write_ctx().compression.filters;
    let mut delta_channels: Option<u8> = None;
    let mut x86_filter_type: Option<u8> = None;
    let mut x86_regions: Vec<std::ops::Range<usize>> = Vec::new();
    // Set when `-mcd+ -mce+` forces both filters: the window loop then
    // picks one filter per 64 KiB block instead of overlapping records.
    let mut forced_combined: Option<u8> = None;
    if file_size < u32::MAX as u64 {
        let sample_len = ((64 * 1024) as u64).min(file_size) as usize;
        let mut sample = vec![0u8; sample_len];
        let got = {
            let mut sf = File::open(path)?;
            sf.read(&mut sample)?
        };
        sample.truncate(got);
        let variant = crate::version::ArchiveVersion::from_v70(dict_bytes.is_some());
        if got > 0 {
            // -mcd+ forces the delta filter; the switch's channel count
            // wins, then the sample pick, then a single lane.
            if filter_policy.delta == crate::options::FilterMode::Forced {
                delta_channels = Some(filter_policy.delta_channels.unwrap_or_else(|| {
                    lzss_huff::pick_delta_channel(&sample, method, dsl, variant)
                        .ok()
                        .flatten()
                        .unwrap_or(1)
                }));
                if filter_policy.x86 == crate::options::FilterMode::Forced {
                    forced_combined = Some(lzss_huff::FILTER_E8E9);
                }
            }
            // Try delta filter first (cheap pre-gate on sample); a
            // forced x86 owns the whole member, so the auto probe is
            // skipped rather than producing the overlapping-pair error.
            if auto_delta_probe_enabled(filter_policy.delta, filter_policy.x86)
                && crate::codec::common::filters::auto_delta_filter_channels(&sample).is_some()
            {
                delta_channels = lzss_huff::pick_delta_channel(&sample, method, dsl, variant)?;
            }
            // -mce+ forces the x86 filter over the whole member.
            if filter_policy.x86 == crate::options::FilterMode::Forced {
                if forced_combined.is_none() {
                    x86_filter_type = Some(lzss_huff::FILTER_E8E9);
                    x86_regions = std::iter::once(0..file_size as usize).collect();
                }
            } else if filter_policy.x86 == crate::options::FilterMode::Auto
                && delta_channels.is_none()
                && got > 5
            {
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
                    // Extend the merged sample regions into one span
                    // through end-of-member (disjoint records; see
                    // `x86_region_span`).
                    x86_region_span(&mut x86_regions, file_size as usize);
                }
            }
        }
    }
    let delta_used = delta_channels.is_some();
    // Measure the *delta* candidate before it may transform the member:
    // its effect is local (neighbour correlation), so probe windows judge
    // it faithfully, and the sample-based pick can win by a hair on a 64 KiB
    // head and then cost the whole member (measured: 1.7-2x larger output
    // on real DLLs at 68 MiB with decoding still correct, because the
    // transform is invertible) or a whole-member encode for nothing.
    //
    // The x86 candidate is deliberately *not* measured this way: its gain
    // comes from making code match across the whole member, so on an
    // isolated window it looks worse by construction (measured: every
    // window of a real DLL loses 15-45% with delta, and an x86 window probe
    // rejected a filter that wins ~6% on the member). It keeps its
    // detection-based decision, exactly as before.
    if delta_used && filter_policy.delta != FilterMode::Forced {
        let channels = delta_channels.expect("checked above");
        let variant = crate::version::ArchiveVersion::from_v70(dict_bytes.is_some());
        let probed =
            probe_filter_candidate(path, file_size, method, dsl, variant, |bytes, offset| {
                lzss_huff::delta_stream_window(bytes, offset, channels).0
            })?;
        if probed.is_none() {
            delta_channels = None;
        }
    }
    let delta_used = delta_channels.is_some();
    let x86_used = x86_filter_type.is_some();
    ensure_compatible_stream_filters(delta_used, x86_used)?;
    if delta_used || x86_used {
        crate::format::shared::write_ops::reset_solid_chain(cx);
    }

    let chain_solid = cx.write_ctx().solid.mode && cx.write_ctx().solid.encoder_state.is_some();
    cx.write_ctx_mut()
        .solid
        .encoder_state
        .get_or_insert_with(Default::default);
    // Each member starts its own frame; see `EncoderState::begin_member`.
    cx.write_ctx_mut()
        .solid
        .encoder_state
        .as_mut()
        .expect("encoder state seeded")
        .begin_member();

    let mut crc_hasher = crc32fast::Hasher::new();
    let mut blake_hasher = if cx.write_ctx().meta.blake2 {
        Some(crate::format::rar5::blake2sp::Hasher::new())
    } else {
        None
    };
    let mut bytes_read = 0u64;
    let mut packed_size = 0u64;
    let threads = cx.effective_threads();
    let cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = cx.cancel_token();
    let cancel_ref = cancel_flag.as_deref();
    let spill_path = spill_path_for(cx.path());
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
                            variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
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
        // Set once compression has already lost (`packed_size >=
        // file_size`): stop encoding, but keep reading to hash the rest
        // of the file, because the STORE fallback writes the whole file
        // and its header checksum must cover every byte.
        let mut gave_up = false;
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
                if !gave_up {
                    work.extend_from_slice(&buf[..n]);
                }
                cx.report_progress(bytes_read, file_size);
            }
            if !gave_up && (eof || work.len() >= mt_window) {
                let flushed = work.len() as u64;
                // Streaming filters: apply delta (full window, lane-
                // blocked) then x86 (clipped to detected regions) on
                // the already-delta-transformed data. The decoder
                // applies inverse transforms in symbol-stream order
                // (delta first, then x86), which reverses the encode
                // order — both are linear and region-independent so
                // composition is correct regardless of overlap.
                let mut window_specs: Option<Vec<lzss_huff::FilterSpec>> = None;
                if let Some(x86_type) = forced_combined {
                    let variant = crate::version::ArchiveVersion::from_v70(dict_bytes.is_some());
                    let (transformed, specs) = lzss_huff::forced_combined_stream_window(
                        &work,
                        member_offset,
                        method,
                        dsl,
                        variant,
                        delta_channels.unwrap_or(1),
                        x86_type,
                    );
                    work = transformed;
                    window_specs = Some(specs);
                } else {
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
                }
                flush_window(
                    &mut work,
                    eof,
                    threads,
                    method,
                    dsl,
                    dict_bytes,
                    &mut cx.write_ctx_mut().solid.encoder_state,
                    &mut spill,
                    &mut packed_size,
                    cancel_ref,
                    member_offset,
                    window_specs.as_deref(),
                )?;
                member_offset += flushed;
                if packed_size >= file_size {
                    gave_up = true;
                    work = Vec::new();
                }
            }
        }
    }

    if bytes_read != file_size {
        return Err(RarError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "file changed size while being archived: expected {file_size} bytes, read {bytes_read}"
            ),
        )));
    }

    let plain_crc = crc_hasher.finalize();
    let plain_blake = blake_hasher.map(|h| h.finalize());

    if packed_size >= file_size {
        // Compression is a net loss: fall back to streaming STORE.
        crate::format::shared::write_ops::reset_solid_chain(cx);
        let (header_crc, extra_data, stored_hash, encr) =
            super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
        let mut plan = MemberPlan {
            name: name.to_string(),
            unpacked_size: file_size,
            file_crc: header_crc,
            method: COMP_METHOD_STORE,
            dict_size_log: 0,
            dict_size_bytes: dict_bytes,
            extra_data,
            attrs,
            mtime,
            solid: false,
            stored_hash,
        };
        plan.push_extra(time_extra.as_deref(), owner_extra.as_deref());
        write_store_member(cx, path, plan, encr.as_ref())?;
        write_member_streams(cx, path)?;
        cx.report_progress(file_size, file_size);
        return Ok(());
    }

    let (header_crc, extra_data, stored_hash, encr) =
        super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
    let mut plan = MemberPlan {
        name: name.to_string(),
        unpacked_size: file_size,
        file_crc: header_crc,
        method,
        dict_size_log: dsl,
        dict_size_bytes: dict_bytes,
        extra_data,
        attrs,
        mtime,
        solid: chain_solid,
        stored_hash,
    };
    plan.push_extra(time_extra.as_deref(), owner_extra.as_deref());
    let mut spill = File::open(&spill_path)?;
    // Encrypted members store the zero-padded ciphertext length in
    // the header and on disk (the streaming encryptor pads the final
    // partial block); plain members store the packed length as-is.
    let (packed_size, plain_len) = match encr {
        Some(_) => (crypto::zero_padded_len(packed_size), packed_size),
        None => (packed_size, packed_size),
    };
    write_streamed_payload(
        cx,
        &plan,
        packed_size,
        &mut spill,
        plain_len,
        encr.as_ref(),
        false,
    )?;
    write_member_streams(cx, path)?;
    // Non-solid members use an independent LZ window: drop the
    // encoder state so the next member starts fresh. A delta/x86-
    // filtered member is also standalone (its window holds transformed
    // bytes, which must never seed the next solid member).
    if !cx.write_ctx().solid.mode || delta_used || x86_used {
        crate::format::shared::write_ops::reset_solid_chain(cx);
    }
    cx.report_progress(file_size, file_size);
    Ok(())
}

/// Write the NTFS alternate data streams of `path` as "STM" service
/// records right after the member's file block (WinRAR `-os`).
pub(super) fn write_member_streams(cx: &mut dyn Engine, path: &Path) -> RarResult<()> {
    if !cx.write_ctx().meta.streams {
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
            let password = cx.password().map(str::to_owned);
            write_stream_record(cx, &name, data, password.as_deref())?;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    Ok(())
}

/// Write one "STM" service record for `name` (the archive form, e.g.
/// `:Zone.Identifier`) carrying `data`. `password` encrypts the payload
/// with a fresh per-stream ENCR record; the stored CRC32 stays the
/// plaintext checksum (WinRAR does not MAC stream CRCs). Shared by the
/// add-time filesystem enumerator and the multi-volume rewrite's
/// re-emitted records.
pub(crate) fn write_stream_record(
    cx: &mut dyn Engine,
    name: &str,
    data: Vec<u8>,
    password: Option<&str>,
) -> RarResult<()> {
    let subdata = {
        let mut extra = Vec::new();
        extra.extend(vint::encode((1 + name.len()) as u64));
        extra.extend(vint::encode(crate::format::rar5::EXTRA_SERVICE_SUBDATA));
        extra.extend(name.as_bytes());
        extra
    };
    let data_len = data.len();
    let stream_crc = crc32fast::hash(&data);
    let (extra, packed) = match password {
        Some(password) => {
            // Stream CRCs stay plaintext, so the record must not
            // request hash-MAC'd checksums (flag 0x02).
            let session = crypto::MemberEncryption::generate_with_flags(
                password,
                crate::crypto::ENCR_PBKDF2_ITER_LOG,
                crate::crypto::ENCR_FLAG_CHECKSUM,
            );
            let mut extra = session.extra_bytes();
            extra.extend_from_slice(&subdata);
            (extra, session.encrypt(&data))
        }
        None => (subdata, data),
    };
    let hdr = crate::format::rar5::headers::build_stream_block(
        packed.len() as u64,
        data_len as u64,
        stream_crc,
        COMP_METHOD_STORE,
        0,
        &extra,
    );
    let hdr_disk = cx.on_disk_header_len(hdr.len() as u64);
    super::add::ensure_rar5_volume_space(cx, hdr_disk + packed.len() as u64)?;
    cx.write_block_header(&hdr)?;
    let stream = cx.stream_mut()?;
    stream.write_all(&packed)?;
    let ctx = cx.write_ctx_mut();
    ctx.output.bytes_written = ctx
        .output
        .bytes_written
        .saturating_add(hdr_disk)
        .saturating_add(packed.len() as u64);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        auto_delta_probe_enabled, ensure_compatible_stream_filters, probe_filter_candidate,
        x86_region_span,
    };
    use crate::error::RarError;
    use crate::options::FilterMode;
    use std::fs::File;
    use std::io::Write;

    /// A probe candidate must be rejected as soon as it loses on *one* probe
    /// point: the old gate trusted a single 64 KiB head, so a member whose head
    /// looked delta-friendly had delta applied to its whole (x86) body and
    /// packed 1.7-2x larger. Here the head is a random walk (delta wins) and
    /// the probe point at the middle is x86 code (plain wins), so the delta
    /// transform must be refused.
    #[test]
    fn probe_rejects_a_filter_that_loses_on_any_probe_point() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("member.bin");
        let mut data = Vec::new();
        // Random walk: correlated neighbours, no repeats for LZSS.
        let mut value = 0u8;
        let mut state = 0x1234_5678u64;
        while data.len() < crate::codec::lzss_huff::FILTER_PROBE_LEN {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            value = value.wrapping_add((state % 15) as u8).wrapping_sub(7);
            data.push(value);
        }
        // Incompressible body: plain LZSS cannot shrink it and delta only
        // adds boundary noise, so the candidate must lose here. This is the
        // shape the gate exists for: a head that looks delta-friendly (a PE
        // header, a media header) in front of 60 MiB of body that delta cannot
        // help.
        while data.len() < 2 * crate::codec::lzss_huff::FILTER_PROBE_LEN + 64 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            data.push((state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u8);
        }
        let size = data.len() as u64;
        File::create(&path).unwrap().write_all(&data).unwrap();

        let variant = crate::version::ArchiveVersion::V50;
        let channels = 2u8;
        let delta = probe_filter_candidate(&path, size, 1, 8, variant, |bytes, offset| {
            crate::codec::lzss_huff::delta_stream_window(bytes, offset, channels).0
        })
        .unwrap();
        assert!(
            delta.is_none(),
            "a delta candidate that loses on the incompressible probe point must be rejected"
        );

        // The same member with a random-walk body at both probe points lets
        // the candidate through: the gate rejects, it does not blanket-disable.
        let walk_path = dir.path().join("walk.bin");
        let mut walk = Vec::new();
        let mut value = 0u8;
        let mut state = 0x9E37_79B9u64;
        while walk.len() < 2 * crate::codec::lzss_huff::FILTER_PROBE_LEN + 64 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            value = value.wrapping_add((state % 15) as u8).wrapping_sub(7);
            walk.push(value);
        }
        let walk_size = walk.len() as u64;
        File::create(&walk_path).unwrap().write_all(&walk).unwrap();
        let accepted =
            probe_filter_candidate(&walk_path, walk_size, 1, 8, variant, |bytes, offset| {
                crate::codec::lzss_huff::delta_stream_window(bytes, offset, channels).0
            })
            .unwrap();
        assert!(
            accepted.is_some(),
            "a delta candidate that wins on both probe points must be accepted"
        );
    }

    #[test]
    fn forced_delta_and_x86_together_are_rejected() {
        assert!(ensure_compatible_stream_filters(false, false).is_ok());
        assert!(ensure_compatible_stream_filters(true, false).is_ok());
        assert!(ensure_compatible_stream_filters(false, true).is_ok());
        match ensure_compatible_stream_filters(true, true).unwrap_err() {
            RarError::InvalidOption(message) => assert_eq!(
                message,
                "cannot force the delta and x86 filters on the same member; choose one filter mode"
            ),
            other => panic!("expected InvalidOption, got {other}"),
        }
    }

    /// Forced x86 + an auto-detectable delta must not abort a large-member
    /// add: the probe is skipped (matching the buffered `-mc` policy), so
    /// the compatibility check never sees the pair.
    #[test]
    fn forced_x86_skips_the_auto_delta_probe() {
        assert!(auto_delta_probe_enabled(FilterMode::Auto, FilterMode::Auto));
        assert!(auto_delta_probe_enabled(
            FilterMode::Auto,
            FilterMode::Disabled
        ));
        assert!(!auto_delta_probe_enabled(
            FilterMode::Auto,
            FilterMode::Forced
        ));
        assert!(!auto_delta_probe_enabled(
            FilterMode::Disabled,
            FilterMode::Auto
        ));
        // Forced delta never runs through the probe: it still pairs with a
        // forced x86 into the rejection below.
        assert!(!auto_delta_probe_enabled(
            FilterMode::Forced,
            FilterMode::Disabled
        ));
        assert!(ensure_compatible_stream_filters(false, true).is_ok());
    }

    /// The streaming x86 records must be disjoint: collapse the merged
    /// sample regions into the span from the first detection to EOF instead
    /// of extending each region (which produced overlapping records).
    #[test]
    fn x86_stream_region_span_is_disjoint() {
        let mut regions = vec![16..100, 200..300, 640..1024];
        x86_region_span(&mut regions, 4096);
        assert_eq!(regions, vec![16..4096]);

        // Unordered input still starts at the earliest detection.
        let mut regions = vec![512..600, 32..64];
        x86_region_span(&mut regions, 4096);
        assert_eq!(regions, vec![32..4096]);

        let mut single: Vec<std::ops::Range<usize>> = Vec::new();
        single.push(0..64);
        x86_region_span(&mut single, 4096);
        assert_eq!(single, vec![0..4096]);

        let mut empty: Vec<std::ops::Range<usize>> = Vec::new();
        x86_region_span(&mut empty, 4096);
        assert!(empty.is_empty());
    }
}
