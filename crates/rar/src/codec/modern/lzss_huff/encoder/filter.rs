//! Pre-compression VM filters and their entry points.
//!
//! `encode_with_filters` (and the auto variants) apply delta/E8/E8E9/ARM
//! transforms to regions of the input before the LZ parse, splitting members
//! into filter blocks no larger than [`MAX_FILTER_BLOCK_LENGTH`]. The
//! platform-independent window helpers (`delta_stream_window`,
//! `x86_stream_window`, `merge_ranges`) are shared with the decoder side.

use super::*;

use super::super::{FILTER_ARM, FILTER_DELTA, FILTER_E8, FILTER_E8E9};
use super::emit::{encode_block, encode_empty_block};
use super::parse::{
    EMITTED_BLOCK_SIZE, OPTIMAL_PARSE_PASSES, find_block_end_adaptive, find_matches_optimal,
    find_matches_with_tail,
};
use crate::codec::common::filters::apply_filter_encode;
use crate::error::{RarError, RarResult};
use crate::version::ArchiveVersion;

/// Maximum length of one RAR5 filter block.
///
/// RARLAB readers (unrar/WinRAR) refuse filter regions larger than 256 KiB
/// (`0x40000`); the reference writer splits members into filter blocks of at
/// most `0x3FFFF` bytes. Same value as the `rars` project's
/// `MAX_FILTER_BLOCK_LENGTH` (MIT OR Apache-2.0).
pub const MAX_FILTER_BLOCK_LENGTH: u32 = 0x3FFFF;

/// Filter types the RAR5 writer intentionally produces: Delta, E8, E8E9 and
/// ARM. Types 4-7 (ARMT/IA64/PPC/SPARC) are defined in the format notes but
/// never produced by any implementation; the decoder refuses them explicitly
/// (`apply_filter_decode`) while the encoder's forward transform would no-op
/// them, so without this check a caller could write a filter record the
/// reader rejects over raw, untransformed bytes.
fn is_supported_filter_type(filter_type: u8) -> bool {
    matches!(
        filter_type,
        FILTER_DELTA | FILTER_E8 | FILTER_E8E9 | FILTER_ARM
    )
}

/// Single validation point for caller-supplied [`FilterSpec`]s, shared by the
/// sequential ([`encode_with_filters`]) and parallel ([`encode_with_filters_mt`])
/// writer entry points:
///
/// - the filter type must be one the writer produces,
/// - regions must be non-empty and lie fully within `data`,
/// - regions must not overlap. The decoder applies records in stream order,
///   and mixed transforms (e.g. Delta then E8) do not commute, so overlapping
///   records have no correct inverse; callers with overlapping ranges merge
///   them first (see [`merge_ranges`]).
///
/// Rejecting up front keeps the writer from emitting a member its own
/// streaming decoder refuses (a filter region that outlives the member).
fn validate_filter_specs(data_len: usize, filters: &[FilterSpec]) -> RarResult<()> {
    let data_len = data_len as u64;
    let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(filters.len());
    for f in filters {
        if !is_supported_filter_type(f.filter_type) {
            return Err(RarError::InvalidOption(format!(
                "unsupported RAR5 filter type {}",
                f.filter_type
            )));
        }
        if f.block_length == 0 {
            return Err(RarError::InvalidOption(format!(
                "RAR5 filter region at {} has zero length",
                f.block_start
            )));
        }
        let start = u64::from(f.block_start);
        let end = start + u64::from(f.block_length);
        if end > data_len {
            return Err(RarError::InvalidOption(format!(
                "RAR5 filter region {start}..{end} exceeds member size {data_len}"
            )));
        }
        ranges.push((start, end));
    }
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(RarError::InvalidOption(format!(
                "overlapping RAR5 filter regions {}..{} and {}..{}",
                pair[0].0, pair[0].1, pair[1].0, pair[1].1
            )));
        }
    }
    Ok(())
}

/// Encode `data` as a single RAR5 member with output filters applied.
///
/// The filters are recorded at the start of the symbol stream (the decoder
/// applies each filter to its region once the region is fully produced, so
/// emitting the records early is equivalent to inline emission). `data` is
/// forward-transformed per filter spec before match finding. The caller is
/// responsible for comparing the packed size against unfiltered output and
/// falling back to STORE.
///
/// The filter region positions are member-relative and the E8/ARM transform
/// offsets are member-relative too (WinRAR's `WrittenFileSize` is per-file);
/// a member written through this path must be marked non-solid so the
/// decoder's filter positions stay member-relative.
///
/// Specs are validated (see `validate_filter_specs`): unsupported filter
/// types, empty or out-of-bounds regions and overlaps are rejected instead of
/// producing a member the streaming decoder cannot decode.
pub fn encode_with_filters(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    filters: &[FilterSpec],
    variant: ArchiveVersion,
) -> RarResult<Vec<u8>> {
    validate_filter_specs(data.len(), filters)?;
    if data.is_empty() {
        return Ok(encode_empty_block(variant));
    }
    if filters.is_empty() {
        return encode_chunked_raw(
            data,
            method,
            dict_size_log,
            DEFAULT_CHUNK_SIZE,
            None,
            true,
            None,
            variant,
        );
    }

    // Split over-long regions: RARLAB readers reject filter blocks above
    // MAX_FILTER_BLOCK_LENGTH. Each piece is an independent filter record
    // with its own transform state, so splitting is byte-exact.
    let mut specs: Vec<FilterSpec> = Vec::new();
    for f in filters {
        let mut start = f.block_start;
        let mut remaining = f.block_length;
        while remaining > 0 {
            let len = remaining.min(MAX_FILTER_BLOCK_LENGTH);
            specs.push(FilterSpec::new(f.filter_type, f.channels, start, len));
            start = start.saturating_add(len);
            remaining = remaining.saturating_sub(len);
        }
    }

    // 1. Forward-transform each region (shared with the MT path).
    let transformed = forward_transform(data, &specs);

    // 2. Match-find on the transformed data in bounded chunks with a
    //    persistent window, mirroring the unfiltered chunked path. The
    //    filter records lead the first chunk's symbol stream.
    let level = (method as usize).clamp(1, 5);
    let (chain_len, lazy_thresh, max_match) = LEVEL_PARAMS[level];
    let dict_size = 128 * 1024 * (1usize << dict_size_log as u32);
    let long_range = level >= 2;

    let mut state = EncoderState::default();
    let mut filter_symbols: Vec<Symbol> = specs
        .iter()
        .map(|f| Symbol::Filter {
            block_start: f.block_start,
            block_length: f.block_length,
            filter_type: f.filter_type,
            channels: f.channels,
        })
        .collect();
    let mut output = Vec::new();
    let mut chunk_start = 0usize;
    let mut first_chunk = true;
    while chunk_start < transformed.len() {
        let chunk_end = (chunk_start + DEFAULT_CHUNK_SIZE).min(transformed.len());
        let chunk = &transformed[chunk_start..chunk_end];
        let is_final = chunk_end >= transformed.len();
        // Levels 2-5 use the optimal parse, like the unfiltered path.
        let mut symbols = if level >= 2 {
            find_matches_optimal(
                &mut state,
                chunk,
                chain_len,
                lazy_thresh,
                max_match,
                dict_size,
                long_range,
                None,
                0,
                variant,
                OPTIMAL_PARSE_PASSES[level],
                true,
            )
        } else {
            find_matches_with_tail(
                &mut state,
                chunk,
                chain_len,
                lazy_thresh,
                max_match,
                dict_size,
                long_range,
            )
        };
        // The filter records lead the first chunk's symbol stream so they
        // are read before any output is produced (write_pos = the member
        // start), keeping the recorded positions member-relative.
        if first_chunk {
            let mut filters = std::mem::take(&mut filter_symbols);
            filters.append(&mut symbols);
            symbols = filters;
            first_chunk = false;
        }

        let mut block_start = 0usize;
        while block_start < symbols.len() {
            let (block_end, _) = find_block_end_adaptive(&symbols, block_start, EMITTED_BLOCK_SIZE);
            let is_last = is_final && block_end >= symbols.len();
            let block_data = encode_block(&symbols[block_start..block_end], is_last, variant);
            output.extend(block_data);
            // Early bail-out: a filtered stream already larger than the
            // input cannot beat STORE (callers fall back to STORE).
            if !is_last && output.len() > data.len() {
                break;
            }
            block_start = block_end;
        }
        if output.len() > data.len() {
            break;
        }
        chunk_start = chunk_end;
    }
    Ok(output)
}

/// Forward-transform `data` per filter spec, in place on a copy. Regions
/// must be disjoint; the transform reads only its own slice, and E8/ARM
/// file offsets are member-relative positions. Shared by the sequential
/// and multi-threaded filtered encoders.
fn forward_transform(data: &[u8], specs: &[FilterSpec]) -> Vec<u8> {
    let mut transformed = data.to_vec();
    for f in specs {
        let start = f.block_start as usize;
        let end = (start + f.block_length as usize).min(transformed.len());
        if start >= end {
            continue;
        }
        let t = apply_filter_encode(
            f.filter_type,
            &mut transformed[start..end],
            f.channels,
            f.block_start as u64,
        );
        transformed[start..end].copy_from_slice(&t);
    }
    transformed
}

/// Multi-threaded variant of [`encode_with_filters`]: the forward transform
/// is identical, then the transformed member is encoded across the
/// compression pool (the filter records lead the first slice's symbol
/// stream). `threads == 1` keeps the sequential path (byte-identical to
/// [`encode_with_filters`]); the MT slices reset the repeat-distance cache
/// per slice, the documented MT divergence.
#[cfg(not(feature = "parallel"))]
pub fn encode_with_filters_mt(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    filters: &[FilterSpec],
    variant: ArchiveVersion,
    _threads: usize,
    _cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<Vec<u8>> {
    // Without the pool, fall back to the sequential encode; the caller's
    // member-level logic (threads == 1 or no pool) makes this equivalent.
    encode_with_filters(data, method, dict_size_log, filters, variant)
}

#[cfg(feature = "parallel")]
pub fn encode_with_filters_mt(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    filters: &[FilterSpec],
    variant: ArchiveVersion,
    threads: usize,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<Vec<u8>> {
    validate_filter_specs(data.len(), filters)?;
    if data.is_empty() {
        return Ok(encode_empty_block(variant));
    }
    if threads <= 1 || filters.is_empty() {
        return encode_with_filters(data, method, dict_size_log, filters, variant);
    }
    let mut specs: Vec<FilterSpec> = Vec::new();
    for f in filters {
        let mut start = f.block_start;
        let mut remaining = f.block_length;
        while remaining > 0 {
            let len = remaining.min(MAX_FILTER_BLOCK_LENGTH);
            specs.push(FilterSpec::new(f.filter_type, f.channels, start, len));
            start = start.saturating_add(len);
            remaining = remaining.saturating_sub(len);
        }
    }
    let transformed = forward_transform(data, &specs);
    let lead: Vec<Symbol> = specs
        .iter()
        .map(|f| Symbol::Filter {
            block_start: f.block_start,
            block_length: f.block_length,
            filter_type: f.filter_type,
            channels: f.channels,
        })
        .collect();
    let mut state = EncoderState::default();
    encode_chunked_mt_with_progress(
        &transformed,
        method,
        dict_size_log,
        DEFAULT_CHUNK_SIZE,
        &mut state,
        threads,
        true,
        variant,
        Some(&lead),
        None,
        cancel,
    )
}

/// Piecewise delta transform for one window of a streaming member.
///
/// The member is cut into independent delta regions at
/// [`MAX_FILTER_BLOCK_LENGTH`] on absolute member coordinates — the same
/// split [`encode_with_filters`] applies, where each region's lanes start
/// fresh — and each region is forward-transformed, reproducing the layout
/// of a whole-member front-transform exactly. Returns the transformed
/// window bytes (the compressed blocks cover this stream in order) and the
/// per-region specs whose records must lead the window's first symbol
/// stream (the decoder holds them pending and applies each inverse once
/// its region is produced).
pub(crate) fn delta_stream_window(
    window: &[u8],
    base_offset: u64,
    channels: u8,
) -> (Vec<u8>, Vec<FilterSpec>) {
    let mut specs = Vec::with_capacity(window.len() / MAX_FILTER_BLOCK_LENGTH as usize + 1);
    let mut transformed = Vec::with_capacity(window.len());
    let mut off = 0usize;
    while off < window.len() {
        let take = MAX_FILTER_BLOCK_LENGTH.min((window.len() - off) as u32) as usize;
        let piece = &window[off..off + take];
        let t = {
            let mut buf = piece.to_vec();
            apply_filter_encode(FILTER_DELTA, &mut buf, channels, base_offset + off as u64)
        };
        // delta_encode returns the lane-blocked reorder of `piece`;
        // concatenating regions reproduces the member's transformed stream.
        transformed.extend_from_slice(&t);
        specs.push(FilterSpec::new(
            FILTER_DELTA,
            channels,
            (base_offset + off as u64) as u32,
            take as u32,
        ));
        off += take;
    }
    (transformed, specs)
}

/// Piecewise x86 (E8/E8E9) transform for one window of a streaming member.
///
/// The window's portion of each auto-detected region is itself split at
/// [`MAX_FILTER_BLOCK_LENGTH`] boundaries (RARLAB readers reject longer
/// filter records), reproducing the layout `encode_with_filters` applies —
/// each piece is an independent record whose E8/E8E9 inverse reads its own
/// file-relative sign, so splitting is byte-exact. The `file_offset` passed
/// to the encoder is each piece's absolute member offset, matching the
/// decoder's expectation. Returns the transformed window bytes and the
/// per-piece specs whose records lead the window's first symbol stream.
pub(crate) fn x86_stream_window(
    window: &[u8],
    base_offset: u64,
    filter_type: u8,
    regions: &[std::ops::Range<usize>],
) -> (Vec<u8>, Vec<FilterSpec>) {
    let mut specs = Vec::new();
    let mut transformed = window.to_vec();
    for region in regions {
        let region_start = region.start as u64;
        let region_end = region.end as u64;
        let win_start = base_offset;
        let win_end = base_offset + window.len() as u64;
        if region_end <= win_start || region_start >= win_end {
            continue;
        }
        let clip_start = region_start.max(win_start);
        let clip_end = region_end.min(win_end);
        let mut piece_start = clip_start;
        while piece_start < clip_end {
            let piece_end = (piece_start + MAX_FILTER_BLOCK_LENGTH as u64).min(clip_end);
            let local_start = (piece_start - win_start) as usize;
            let local_end = (piece_end - win_start) as usize;
            let t = apply_filter_encode(
                filter_type,
                &mut transformed[local_start..local_end],
                0,
                piece_start,
            );
            transformed[local_start..local_end].copy_from_slice(&t);
            specs.push(FilterSpec::new(
                filter_type,
                0,
                piece_start as u32,
                (piece_end - piece_start) as u32,
            ));
            piece_start = piece_end;
        }
    }
    (transformed, specs)
}

/// Block size for the per-block forced-filter choice in
/// [`forced_combined_specs`] (the 64 KiB granularity WinRAR uses for
/// `-mcd+ -mce+`).
pub const FORCED_FILTER_BLOCK: usize = 64 * 1024;

/// Choose one forced filter per [`FORCED_FILTER_BLOCK`] chunk of `data`.
///
/// `-mcd+ -mce+` forces delta *and* x86 over the same bytes, but RARLAB
/// readers reject overlapping filter records. WinRAR splits the member into
/// 64 KiB blocks and emits one non-overlapping filter per block; do the same,
/// picking the candidate whose trial encode of the chunk is smaller (ties keep
/// delta). Feed the result to [`encode_with_filters`]/
/// [`encode_with_filters_mt`].
pub fn forced_combined_specs(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    variant: ArchiveVersion,
    delta_channels: u8,
    x86_type: u8,
) -> Vec<FilterSpec> {
    data.chunks(FORCED_FILTER_BLOCK)
        .enumerate()
        .map(|(index, chunk)| {
            let start = (index * FORCED_FILTER_BLOCK) as u32;
            let length = chunk.len() as u32;
            let delta = FilterSpec::new(FILTER_DELTA, delta_channels, start, length);
            let x86 = FilterSpec::new(x86_type, 0, start, length);
            // The trial encode sees the chunk on its own, so its spec must be
            // block-local (member-absolute offsets would fail validation).
            let trial = |spec: FilterSpec| {
                encode_with_filters(chunk, method, dict_size_log, &[spec], variant)
                    .map(|packed| packed.len())
                    .unwrap_or(usize::MAX)
            };
            let delta_len = trial(FilterSpec::new(FILTER_DELTA, delta_channels, 0, length));
            let x86_len = trial(FilterSpec::new(x86_type, 0, 0, length));
            if delta_len <= x86_len { delta } else { x86 }
        })
        .collect()
}

/// Streaming counterpart of [`forced_combined_specs`]: transform `window` one
/// 64 KiB block at a time (choosing delta or x86 per block) and return the
/// transformed bytes plus the non-overlapping records, whose offsets are
/// member-absolute (`base_offset` is the window's member offset).
pub fn forced_combined_stream_window(
    window: &[u8],
    base_offset: u64,
    method: u8,
    dict_size_log: u8,
    variant: ArchiveVersion,
    delta_channels: u8,
    x86_type: u8,
) -> (Vec<u8>, Vec<FilterSpec>) {
    let mut transformed = Vec::with_capacity(window.len());
    let mut specs = Vec::new();
    for (index, chunk) in window.chunks(FORCED_FILTER_BLOCK).enumerate() {
        let block_offset = (index * FORCED_FILTER_BLOCK) as u64;
        let absolute = base_offset + block_offset;
        let length = chunk.len() as u32;
        let trial = |spec: FilterSpec| {
            encode_with_filters(chunk, method, dict_size_log, &[spec], variant)
                .map(|packed| packed.len())
                .unwrap_or(usize::MAX)
        };
        let delta_len = trial(FilterSpec::new(FILTER_DELTA, delta_channels, 0, length));
        let x86_len = trial(FilterSpec::new(x86_type, 0, 0, length));
        if delta_len <= x86_len {
            let (bytes, mut block_specs) = delta_stream_window(chunk, absolute, delta_channels);
            transformed.extend_from_slice(&bytes);
            specs.append(&mut block_specs);
        } else {
            let region = 0..chunk.len();
            let (bytes, mut block_specs) =
                x86_stream_window(chunk, absolute, x86_type, std::slice::from_ref(&region));
            transformed.extend_from_slice(&bytes);
            specs.append(&mut block_specs);
        }
    }
    (transformed, specs)
}

/// Merge overlapping or adjacent ranges (the x86 scan can return a broad
/// span plus tighter clusters inside it; overlapping filter records would
/// double-transform the overlap).
pub(crate) fn merge_ranges(ranges: &mut Vec<std::ops::Range<usize>>) {
    if ranges.len() < 2 {
        return;
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => {
                if range.end > last.end {
                    last.end = range.end;
                }
            }
            _ => merged.push(range),
        }
    }
    *ranges = merged;
}

/// Encode `data` with automatic x86 output filtering.
///
/// Scans `data` for x86 code regions and encodes with the E8/E8E9 filter
/// variant that packed smallest. Returns `None` when the scan found no
/// regions worth filtering (the caller then uses the unfiltered path). The
/// caller is responsible for comparing the packed size against unfiltered
/// output and falling back to STORE, and for writing the member as
/// non-solid.
pub fn encode_with_auto_x86_filter(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    variant: ArchiveVersion,
    threads: usize,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<Option<Vec<u8>>> {
    if data.len() <= 5 {
        return Ok(None);
    }
    let mut ranges_e9 = crate::codec::common::filters::auto_x86_filter_ranges(data, true);
    if ranges_e9.is_empty() {
        return Ok(None);
    }
    merge_ranges(&mut ranges_e9);
    let specs_e9: Vec<FilterSpec> = ranges_e9
        .iter()
        .map(|r| {
            FilterSpec::new(
                FILTER_E8E9,
                0,
                r.start.min(u32::MAX as usize) as u32,
                (r.len()).min(u32::MAX as usize) as u32,
            )
        })
        .collect();

    let mut ranges_e8 = crate::codec::common::filters::auto_x86_filter_ranges(data, false);
    if ranges_e8.is_empty() || ranges_e8 == ranges_e9 {
        // Only one variant exists: encode it once.
        return Ok(Some(encode_with_filters_mt(
            data,
            method,
            dict_size_log,
            &specs_e9,
            variant,
            threads,
            cancel,
        )?));
    }
    merge_ranges(&mut ranges_e8);
    let specs_e8: Vec<FilterSpec> = ranges_e8
        .iter()
        .map(|r| {
            FilterSpec::new(
                FILTER_E8,
                0,
                r.start.min(u32::MAX as usize) as u32,
                (r.len()).min(u32::MAX as usize) as u32,
            )
        })
        .collect();

    // The E8 vs E8E9 choice costs a full member encode each. Decide it on a
    // leading 64 KiB sample instead (the delta filter picks its channel the
    // same way): the two variants differ by a fraction of a percent on real
    // binaries, and the sample winner is the full winner almost always. Only
    // when the sample is inconclusive (no ranges in it, or a tie) does the
    // full two-encode comparison run, preserving today's exact choice there.
    let sample_len = data.len().min(1 << 16);
    let sample = &data[..sample_len];
    let clip_specs = |specs: &[FilterSpec]| -> Vec<FilterSpec> {
        specs
            .iter()
            .filter_map(|s| {
                if s.block_start >= sample_len as u32 {
                    return None;
                }
                let end = (s.block_start as usize + s.block_length as usize).min(sample_len);
                Some(FilterSpec::new(
                    s.filter_type,
                    0,
                    s.block_start,
                    (end - s.block_start as usize) as u32,
                ))
            })
            .collect()
    };
    let sample_specs_e9 = clip_specs(&specs_e9);
    let sample_specs_e8 = clip_specs(&specs_e8);
    let sample_e9 = encode_with_filters(sample, method, dict_size_log, &sample_specs_e9, variant)?;
    let sample_e8 = encode_with_filters(sample, method, dict_size_log, &sample_specs_e8, variant)?;
    if sample_e8.len() != sample_e9.len()
        && !sample_specs_e8.is_empty()
        && !sample_specs_e9.is_empty()
    {
        let packed = encode_with_filters_mt(
            data,
            method,
            dict_size_log,
            if sample_e8.len() < sample_e9.len() {
                &specs_e8
            } else {
                &specs_e9
            },
            variant,
            threads,
            cancel,
        )?;
        return Ok(Some(packed));
    }

    // Inconclusive sample: keep the exact full comparison.
    let packed_e9 = encode_with_filters_mt(
        data,
        method,
        dict_size_log,
        &specs_e9,
        variant,
        threads,
        cancel,
    )?;
    let packed_e8 = encode_with_filters_mt(
        data,
        method,
        dict_size_log,
        &specs_e8,
        variant,
        threads,
        cancel,
    )?;
    Ok(Some(if packed_e8.len() < packed_e9.len() {
        packed_e8
    } else {
        packed_e9
    }))
}

/// Pick the delta channel whose filtered leading sample packs smallest,
/// requiring it to beat plain LZSS on that sample (`None` otherwise).
/// WinRAR-style size-based selection is robust to byte-wrapping at sample
/// boundaries (a raw magnitude heuristic is fooled into picking a wider
/// channel by the large deltas that wrapping introduces).
pub fn pick_delta_channel(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    variant: ArchiveVersion,
) -> RarResult<Option<u8>> {
    // No data to transform; a zero-length spec is rejected by validation.
    if data.is_empty() {
        return Ok(None);
    }
    let sample_len = data.len().min(1 << 16);
    let sample = &data[..sample_len];
    let plain = encode_with_filters(sample, method, dict_size_log, &[], variant)?;
    let mut best: Option<(u8, usize)> = None;
    for &ch in crate::codec::common::filters::AUTO_DELTA_CHANNELS {
        let spec = FilterSpec::new(FILTER_DELTA, ch, 0, sample_len as u32);
        let packed = encode_with_filters(sample, method, dict_size_log, &[spec], variant)?;
        if packed.len() < plain.len() && best.is_none_or(|(_, b)| packed.len() < b) {
            best = Some((ch, packed.len()));
        }
    }
    Ok(best.map(|(ch, _)| ch))
}

/// Like [`encode_with_auto_x86_filter`] but for the delta (multimedia)
/// filter. When the data looks correlated (the cheap
/// `auto_delta_filter_channels` gate passes), the best channel count is
/// chosen by compressed size on a leading sample and the whole member
/// is forward-transformed and packed as a standalone (non-solid) filter
/// member — but only when it strictly beats plain LZSS. Size-based channel
/// selection is what WinRAR does and is robust to byte-wrapping at sample
/// boundaries (a raw magnitude heuristic is fooled into picking a wider channel
/// by the large deltas that wrapping introduces), and the plain-LZSS
/// comparison guarantees structured-but-not-multi-channel data (text, prose)
/// is never made worse than the unfiltered pack.
/// Bytes measured at each probe point by [`filter_transform_wins`].
///
/// A filter candidate used to be chosen from a 64 KiB head sample, which real
/// DLLs fool (PE headers look delta-friendly) — and on this path the decision
/// then costs a whole-member encode. The streaming writer had the same defect
/// in a worse form (it transformed 68 MiB on that evidence); both paths now
/// share this measured gate.
pub const FILTER_PROBE_LEN: usize = 512 * 1024;

/// How far a candidate must beat plain LZSS at every probe point, in percent.
///
/// A strict `<` is not enough: incompressible data can pack one byte smaller
/// with the transform applied (measured: 524423 vs 524424 on 512 KiB), and a
/// one-byte "win" is noise. It also has to cover the filter *records* the
/// competition does not count — a marginal sample win once shipped an archive
/// that was 53 bytes larger than the unfiltered one on 4.88 MiB of text.
pub const FILTER_PROBE_MARGIN_PERCENT: usize = 1;

/// One probe window of a member: `bytes` starting at member offset `offset`.
#[derive(Debug, Clone, Copy)]
pub struct FilterProbe<'a> {
    pub offset: u64,
    pub bytes: &'a [u8],
}

/// Measure `transform` against plain LZSS over the probe windows, packing both
/// sides with the member's own method.
///
/// Returns `None` when the transform fails to beat plain by
/// [`FILTER_PROBE_MARGIN_PERCENT`] at any probe point (the candidate is
/// rejected), otherwise the summed `(filtered, plain)` packed sizes so callers
/// can rank the candidates that did win. `transform` is the production
/// transform (the same helper the emit path applies); only the records are
/// skipped, and they cost the same on both sides of a comparison.
pub fn filter_transform_wins<F>(
    probes: &[FilterProbe<'_>],
    method: u8,
    dict_size_log: u8,
    variant: ArchiveVersion,
    transform: F,
) -> RarResult<Option<(usize, usize)>>
where
    F: Fn(&[u8], u64) -> Vec<u8>,
{
    let mut filtered_total = 0usize;
    let mut plain_total = 0usize;
    let mut probed = 0usize;
    for probe in probes {
        let plain = encode_with_filters(probe.bytes, method, dict_size_log, &[], variant)?.len();
        let transformed = transform(probe.bytes, probe.offset);
        let filtered =
            encode_with_filters(&transformed, method, dict_size_log, &[], variant)?.len();
        if filtered * 100 > plain * (100 - FILTER_PROBE_MARGIN_PERCENT) {
            return Ok(None);
        }
        filtered_total += filtered;
        plain_total += plain;
        probed += 1;
    }
    if probed == 0 {
        return Ok(None);
    }
    Ok(Some((filtered_total, plain_total)))
}

/// The probe windows of an in-memory member: its head and its middle.
pub fn member_filter_probes(data: &[u8]) -> Vec<FilterProbe<'_>> {
    [0usize, data.len() / 2]
        .into_iter()
        .filter_map(|offset| {
            let len = FILTER_PROBE_LEN.min(data.len().saturating_sub(offset));
            (len >= 64 * 1024).then(|| FilterProbe {
                offset: offset as u64,
                bytes: &data[offset..offset + len],
            })
        })
        .collect()
}

pub fn encode_with_auto_delta_filter(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    variant: ArchiveVersion,
    threads: usize,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<Option<Vec<u8>>> {
    // Cheap pre-gate: skip obviously-uncorrelated (random) data so we never pay
    // for a sample encode on it.
    if crate::codec::common::filters::auto_delta_filter_channels(data).is_none() {
        return Ok(None);
    }
    let Some(channels) = pick_delta_channel(data, method, dict_size_log, variant)? else {
        return Ok(None);
    };
    let block_length = (data.len() as u64).min(u32::MAX as u64) as u32;
    let spec = FilterSpec::new(FILTER_DELTA, channels, 0, block_length);
    // The full member encode runs on the pool like the unfiltered path;
    // the sample selection above stays sequential (64 KiB, negligible).
    let delta_packed = encode_with_filters_mt(
        data,
        method,
        dict_size_log,
        &[spec],
        variant,
        threads,
        cancel,
    )?;
    // No point transforming if it does not even beat STORE.
    if delta_packed.len() >= data.len() {
        return Ok(None);
    }
    // Keep the filter only when it is strictly smaller than plain LZSS; the
    // caller's chunked (possibly solid) path is the better choice otherwise.
    let plain_packed =
        encode_with_filters_mt(data, method, dict_size_log, &[], variant, threads, cancel)?;
    if delta_packed.len() < plain_packed.len() {
        Ok(Some(delta_packed))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| ((i / 64) % 251) as u8).collect()
    }

    /// `-mcd+ -mce+` must produce one non-overlapping record per 64 KiB block
    /// that tiles the whole member (RARLAB readers reject overlaps and records
    /// longer than 256 KiB).
    #[test]
    fn forced_combined_specs_tile_the_member_without_overlap() {
        let data = sample(300_000);
        let specs = forced_combined_specs(&data, 3, 5, ArchiveVersion::V50, 1, FILTER_E8E9);
        assert!(!specs.is_empty());
        let mut cursor = 0u32;
        for spec in &specs {
            assert_eq!(spec.block_start, cursor, "records must tile the member");
            assert!(spec.block_length > 0);
            assert!(spec.block_length <= FORCED_FILTER_BLOCK as u32);
            assert!(matches!(spec.filter_type, FILTER_DELTA | FILTER_E8E9));
            cursor += spec.block_length;
        }
        assert_eq!(cursor as usize, data.len());
        // The validator the encoder runs must accept the sequence.
        validate_filter_specs(data.len(), &specs).unwrap();
    }

    /// The streaming helper returns both the transformed window and the specs;
    /// re-applying the specs to the raw window must reproduce those bytes, so
    /// the encoder and decoder agree.
    #[test]
    fn forced_combined_stream_window_matches_its_records() {
        let data = sample(200_000);
        let (transformed, specs) =
            forced_combined_stream_window(&data, 0, 3, 5, ArchiveVersion::V50, 2, FILTER_E8E9);
        assert_eq!(transformed.len(), data.len());
        validate_filter_specs(data.len(), &specs).unwrap();
        let mut reference = data.clone();
        for spec in &specs {
            let start = spec.block_start as usize;
            let end = start + spec.block_length as usize;
            let piece = apply_filter_encode(
                spec.filter_type,
                &mut reference[start..end],
                spec.channels,
                spec.block_start as u64,
            );
            reference[start..end].copy_from_slice(&piece);
        }
        assert_eq!(reference, transformed);
    }
}
