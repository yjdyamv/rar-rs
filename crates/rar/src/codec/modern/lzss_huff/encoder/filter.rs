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
    emitted_block_cap, find_block_end_adaptive, find_matches_optimal, find_matches_with_tail,
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
            return Err(RarError::invalid_option(format!(
                "unsupported RAR5 filter type {}",
                f.filter_type
            )));
        }
        if f.block_length == 0 {
            return Err(RarError::invalid_option(format!(
                "RAR5 filter region at {} has zero length",
                f.block_start
            )));
        }
        let start = u64::from(f.block_start);
        let end = start + u64::from(f.block_length);
        if end > data_len {
            return Err(RarError::invalid_option(format!(
                "RAR5 filter region {start}..{end} exceeds member size {data_len}"
            )));
        }
        ranges.push((start, end));
    }
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(RarError::invalid_option(format!(
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
    let mut state = EncoderState::default();
    encode_with_filters_into(data, method, dict_size_log, filters, variant, &mut state)
}

/// Like [`encode_with_filters`] but continues an existing solid chain.
///
/// `state` carries the shared LZ window, long-range history and repeat
/// distances across member boundaries, exactly like the unfiltered solid
/// path. Filter records stay member-relative; the decoder adds the member
/// start when it sees them, so a filtered member can sit anywhere in a
/// solid chain (that is how WinRAR writes solid archives).
pub fn encode_with_filters_into(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    filters: &[FilterSpec],
    variant: ArchiveVersion,
    state: &mut EncoderState,
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
            Some(&mut *state),
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
    let (_, lazy_thresh, max_match) = LEVEL_PARAMS[level];
    // Same tier decision as the unfiltered path: filtered members are the
    // ones with an automatic x86/delta filter, so a tier that only reached
    // `encode_chunked_raw_inner` would silently skip exactly the binaries the
    // tier was measured on.
    let (chain_len, miss_threshold) = super::parse_tier(method);
    let dict_size = 128 * 1024 * (1usize << dict_size_log as u32);
    let long_range = level >= 2;

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
                &mut *state,
                chunk,
                super::parse::SequentialSearch {
                    limits: super::parse::MatchLimits {
                        max_match,
                        window: dict_size,
                        variant,
                    },
                    chain_len,
                    miss_threshold,
                    long_range,
                    lr_shared: None,
                    lr_anchor: 0,
                    seed_tail: true,
                },
            )
        } else {
            find_matches_with_tail(
                &mut *state,
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

        let emit_started = std::time::Instant::now();
        let mut block_start = 0usize;
        while block_start < symbols.len() {
            let (block_end, _) =
                find_block_end_adaptive(&symbols, block_start, emitted_block_cap());
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
        super::profile::add(super::profile::EMIT, emit_started);
        if output.len() > data.len() {
            break;
        }
        chunk_start = chunk_end;
    }
    super::profile::dump();
    Ok(output)
}

/// Multi-threaded `encode_with_filters_into`: the transform runs first,
/// then the transformed member is encoded across the pool while `state`
/// carries the solid chain.
#[cfg(not(feature = "parallel"))]
#[allow(clippy::too_many_arguments)]
pub fn encode_with_filters_continue(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    filters: &[FilterSpec],
    variant: ArchiveVersion,
    _threads: usize,
    state: &mut EncoderState,
    _cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<Vec<u8>> {
    encode_with_filters_into(data, method, dict_size_log, filters, variant, state)
}

/// Multi-threaded `encode_with_filters_into`: the transform runs first,
/// then the transformed member is encoded across the pool while `state`
/// carries the solid chain.
#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)]
pub fn encode_with_filters_continue(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    filters: &[FilterSpec],
    variant: ArchiveVersion,
    threads: usize,
    state: &mut EncoderState,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<Vec<u8>> {
    if threads <= 1 || filters.is_empty() {
        return encode_with_filters_into(data, method, dict_size_log, filters, variant, state);
    }
    validate_filter_specs(data.len(), filters)?;
    if data.is_empty() {
        return Ok(encode_empty_block(variant));
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
    encode_chunked_mt_with_progress(
        &transformed,
        EncodeSpec {
            method,
            dict_size_log,
            chunk_size: DEFAULT_CHUNK_SIZE,
            is_final: true,
            variant,
            lead: Some(&lead),
        },
        state,
        threads,
        None,
        cancel,
    )
}

/// Filter specs for a member that will continue a solid chain.
///
/// The whole-member probes decide before the shared encoder state is
/// touched, because a losing candidate cannot be "un-encoded" from a solid
/// chain. Delta is probed first (correlated audio/raw data), then x86 — the
/// same order as the standalone path — and a winner is emitted as one
/// member-wide record (split into legal 256 KiB pieces by the encoder). The
/// x86 transform is applied to the whole member rather than to detected code
/// ranges; the probes' 1% margin keeps that from losing to plain LZSS.
pub(crate) fn solid_filter_specs(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    variant: ArchiveVersion,
    policy: &crate::options::FilterOptions,
) -> RarResult<Option<Vec<FilterSpec>>> {
    use crate::options::FilterMode;
    if data.len() <= 5 || data.len() > u32::MAX as usize {
        return Ok(None);
    }
    if policy.delta == FilterMode::Disabled && policy.x86 == FilterMode::Disabled {
        return Ok(None);
    }
    let probes = member_filter_probes(data);
    if probes.is_empty() {
        return Ok(None);
    }
    // A forced delta filter applies to the whole member without a probe.
    if policy.delta == FilterMode::Forced {
        let channels = match policy.delta_channels {
            Some(channels) => channels,
            None => pick_delta_channel(data, method, dict_size_log, variant)?.unwrap_or(4),
        };
        return Ok(Some(vec![FilterSpec::new(
            FILTER_DELTA,
            channels,
            0,
            data.len() as u32,
        )]));
    }
    if policy.delta == FilterMode::Auto
        && let Some(channels) = pick_delta_channel(data, method, dict_size_log, variant)?
        && filter_transform_wins(&probes, method, dict_size_log, variant, |bytes, offset| {
            delta_stream_window(bytes, offset, channels).0
        })?
        .is_some()
    {
        return Ok(Some(vec![FilterSpec::new(
            FILTER_DELTA,
            channels,
            0,
            data.len() as u32,
        )]));
    }
    if policy.x86 == FilterMode::Disabled {
        return Ok(None);
    }
    // A forced x86 filter applies to the whole member without a probe.
    if policy.x86 == FilterMode::Forced {
        return Ok(Some(vec![FilterSpec::new(
            FILTER_E8E9,
            0,
            0,
            data.len() as u32,
        )]));
    }
    let mut ranges = crate::codec::common::filters::auto_x86_filter_ranges(data, true);
    if ranges.is_empty() {
        return Ok(None);
    }
    merge_ranges(&mut ranges);
    let wins = filter_transform_wins(&probes, method, dict_size_log, variant, |bytes, offset| {
        let mut buffer = bytes.to_vec();
        let piece = apply_filter_encode(FILTER_E8E9, &mut buffer, 0, offset);
        buffer.copy_from_slice(&piece);
        buffer
    })?;
    if wins.is_none() {
        return Ok(None);
    }
    Ok(Some(
        ranges
            .iter()
            .map(|range| {
                FilterSpec::new(
                    FILTER_E8E9,
                    0,
                    range.start.min(u32::MAX as usize) as u32,
                    range.len().min(u32::MAX as usize) as u32,
                )
            })
            .collect(),
    ))
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

/// [`encode_with_filters`] using up to `threads` workers, honouring `cancel`
/// between chunks; the output is byte-identical to the single-threaded call.
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
    // The MT tier pays for slices: with a 2 MiB slice floor a member under
    // `MT_MIN_LEN` (four slices' worth) yields too few of them, so it is both
    // slower per byte
    // than the sequential parse and a little worse (its candidate depth is
    // bounded, the sequential path descends the tree). Measured at `-mt8`:
    // kernel32 845 KiB 40.23 % -> 41.03 %, ntdll 2.5 MiB 39.34 % -> 40.36 %,
    // win32kfull 4.35 MiB 42.57 % -> 43.41 %; ntoskrnl 13.9 MiB goes 41.53 %
    // -> 42.28 % for 3x the speed. Keep small members sequential.
    let mt_min_len = std::env::var("RAR_RS_MT_MIN_LEN")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8 * 1024 * 1024);
    if data.len() < mt_min_len {
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
        EncodeSpec {
            method,
            dict_size_log,
            chunk_size: DEFAULT_CHUNK_SIZE,
            is_final: true,
            variant,
            lead: Some(&lead),
        },
        &mut state,
        threads,
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

/// Delta frame sizes tried per block by [`best_combined_specs`]: the auto
/// list plus the wider frames WinRAR emits in x86 resource sections (5 and
/// 32 appear in its ntoskrnl records).
const BLOCK_DELTA_CHANNELS: &[u8] = &[1, 2, 3, 4, 5, 6, 8, 9, 12, 16, 32];

/// Cheap per-block delta channel pick: count deltas of magnitude at most 8
/// (the "is this correlated?" signal `auto_delta_filter_channels` uses) for
/// each candidate frame and keep the best. No encode is involved; the block
/// trial below is the arbiter, so a bad pick only costs one trial encode.
fn block_delta_channel(block: &[u8]) -> Option<u8> {
    block_stats(block, 0, false, &[], None).delta_ch
}

/// Per-block statistics for the trial-free filter pick.
///
/// `delta_*` is the best delta frame and how many of its bytes differ by at
/// most 8 (the correlation signal `auto_delta_filter_channels` uses). The
/// `x86_*` counters are the `E8`/`E9` opcodes and how many of them carry a
/// displacement that the x86 transform would actually rewrite — exactly the
/// `< 0x0100_0000` window test in `e8_encode`, so "plausible" here means "the
/// filter will touch this byte". A byte the filter leaves alone cannot help,
/// which is what makes the count a usable gain proxy.
/// Bytes covered by matches in a greedy 4-byte-hash LZ pass.
///
/// This is the cheapest honest answer to "does the encoder find structure
/// here?": entropy counts say a stream is skewed, not that it repeats, and
/// the m3 trial is 10x this cost. Greedy 4-byte hashing understates what the
/// optimal parse finds, but the comparison across the (at most four) candidate
/// streams of one block only needs the ordering to hold.
fn match_cover(bytes: &[u8]) -> u64 {
    const HLOG: u32 = 16;
    if bytes.len() < 8 {
        return 0;
    }
    let mask = (1usize << HLOG) - 1;
    let mut table = vec![u32::MAX; 1 << HLOG];
    let mut covered = 0u64;
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        let slot = u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let hash = (slot.wrapping_mul(0x9E37_79B1) >> (32 - HLOG)) as usize & mask;
        let prev = table[hash];
        table[hash] = i as u32;
        if prev != u32::MAX {
            let p = prev as usize;
            if bytes[p..p + 4] == bytes[i..i + 4] {
                let mut len = 4usize;
                while i + len < bytes.len() && bytes[p + len] == bytes[i + len] {
                    len += 1;
                }
                covered += len as u64;
                i += len;
                continue;
            }
        }
        i += 1;
    }
    covered
}

#[derive(Debug, Clone, Copy, Default)]
struct BlockStats {
    delta_ch: Option<u8>,
    delta_near_zero: u64,
    e8_total: u64,
    e8_plausible: u64,
    e9_total: u64,
    e9_plausible: u64,
    /// Order-1 conditional entropy (bits/byte) of the raw, delta-transformed
    /// and x86-transformed block. `near_zero` counts cannot tell "the delta
    /// stream is more predictable" from "the raw block is already
    /// compressible" — these can, and they cost one pass each, not an encode.
    h1_raw: f32,
    h1_delta: f32,
    h1_x86: f32,
    /// Greedy LZ match coverage of the same streams (bytes covered), the
    /// x86 one kept per filter type (`cover_x86[FILTER_E8]`,
    /// `cover_x86[FILTER_E8E9]`) because the opcode set is picked per member.
    cover_raw: u64,
    cover_delta: u64,
    cover_x86: [u64; 3],
}

/// Order-1 conditional entropy `H(next | prev)` in bits per byte: the cheapest
/// proxy for what the order-0/order-1 Huffman stages can squeeze out of a
/// stream, and the quantity a filter is supposed to lower.
fn order1_entropy_bits(bytes: &[u8]) -> f32 {
    if bytes.len() < 2 {
        return 0.0;
    }
    let mut counts = vec![0u32; 1 << 16];
    for pair in bytes.windows(2) {
        counts[((pair[0] as usize) << 8) | pair[1] as usize] += 1;
    }
    let mut total = 0f64;
    let mut weighted = 0f64;
    for row in 0..256usize {
        let base = row << 8;
        let cells = &counts[base..base + 256];
        let rsum: u32 = cells.iter().sum();
        if rsum == 0 {
            continue;
        }
        let mut entropy = 0f64;
        for &cell in cells {
            if cell > 0 {
                let p = f64::from(cell) / f64::from(rsum);
                entropy -= p * p.log2();
            }
        }
        weighted += entropy * f64::from(rsum);
        total += f64::from(rsum);
    }
    if total == 0.0 {
        return 0.0;
    }
    (weighted / total) as f32
}

/// Number of leading block bytes the coverage measure looks at
/// (`RAR_RS_HEUR_SAMPLE`, clamped to the block).
fn heur_sample_len(len: usize) -> usize {
    static SAMPLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let sample = *SAMPLE.get_or_init(|| {
        std::env::var("RAR_RS_HEUR_SAMPLE")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1024)
            .unwrap_or(usize::MAX)
    });
    sample.min(len)
}

/// Per-block measurements for the trial-free picker.
///
/// Every stream is measured in one pass over the block: the delta channel
/// counts come from a single sweep that tracks all `BLOCK_DELTA_CHANNELS`
/// lanes at once (eleven separate sweeps re-read the block eleven times, which
/// on a 13.9 MB member is 150 MB of extra traffic for the same counts), and the
/// x86 coverage is kept per filter type so the member-level choice needs no
/// second sweep over the member.
fn block_stats(
    block: &[u8],
    start: u32,
    want_h1: bool,
    x86_types: &[u8],
    delta_override: Option<u8>,
) -> BlockStats {
    let mut stats = BlockStats::default();
    if block.len() < 256 {
        return stats;
    }
    // Pick the channel whose delta stream is most skewed. The sweep is per
    // channel (eleven passes over the block) rather than per byte (one pass,
    // eleven comparisons per byte): the block is 64 KiB and already in L2, so
    // the repeated passes are free, while the arithmetic is the same and this
    // version keeps the picks the corpus numbers were measured with.
    let mut best: Option<(u8, u64)> = None;
    for &ch in BLOCK_DELTA_CHANNELS {
        let ch_usize = ch as usize;
        if block.len() < ch_usize * 4 {
            continue;
        }
        let mut prev = [0u8; 32];
        let mut near_zero = 0u64;
        for (i, &byte) in block.iter().enumerate() {
            let lane = i % ch_usize;
            let delta = prev[lane].wrapping_sub(byte) as i32;
            if delta.unsigned_abs() <= 8 {
                near_zero += 1;
            }
            prev[lane] = byte;
        }
        if best.is_none_or(|(_, b)| near_zero > b) {
            best = Some((ch, near_zero));
        }
    }
    if let Some(channels) = delta_override.or(best.map(|(ch, _)| ch)) {
        stats.delta_ch = Some(channels);
        stats.delta_near_zero = best
            .filter(|(ch, _)| *ch == channels)
            .map(|(_, near_zero)| near_zero)
            .unwrap_or(0);
        let delta_stream = apply_filter_encode(
            FILTER_DELTA,
            &mut block.to_vec(),
            channels,
            u64::from(start),
        );
        if want_h1 {
            stats.h1_raw = order1_entropy_bits(block);
            stats.h1_delta = order1_entropy_bits(&delta_stream);
        }
        // Coverage is measured on a prefix of the block by default: the
        // measure only ranks candidates, and the head of a 64 KiB block
        // already carries its structure (the full-block sweep costs about
        // four times the scan and moved no decision on the corpora
        // measured). `RAR_RS_HEUR_SAMPLE` sets the prefix size.
        let sample = heur_sample_len(block.len());
        let head = &block[..sample];
        stats.cover_raw = match_cover(head);
        stats.cover_delta = match_cover(&delta_stream[..sample.min(delta_stream.len())]);
        for &ty in x86_types {
            let stream = apply_filter_encode(ty, &mut block.to_vec(), 0, u64::from(start));
            stats.cover_x86[ty as usize] = match_cover(&stream[..sample.min(stream.len())]);
            if want_h1 {
                stats.h1_x86 = order1_entropy_bits(&stream);
            }
        }
    }
    // Mirror `e8_encode`'s scan: an `E8`/`E9` byte is rewritten when
    // `addr + (start + i + 1) mod 2^24` lands below 0x0100_0000.
    let mut i = 0usize;
    while i + 4 < block.len() {
        let opcode = block[i];
        if opcode == 0xE8 || opcode == 0xE9 {
            // Same 16 MiB displacement window `e8_encode` uses.
            let offset = start.wrapping_add(i as u32 + 1) % 0x0100_0000;
            let addr = u32::from_le_bytes([block[i + 1], block[i + 2], block[i + 3], block[i + 4]]);
            let plausible = addr.wrapping_add(offset) < 0x0100_0000;
            if opcode == 0xE8 {
                stats.e8_total += 1;
                stats.e8_plausible += u64::from(plausible);
            } else {
                stats.e9_total += 1;
                stats.e9_plausible += u64::from(plausible);
            }
            i += 5;
        } else {
            i += 1;
        }
    }
    stats
}

/// Trial-free per-block choice from [`BlockStats`] (the `RAR_RS_FILTER_HEUR`
/// experiment): the x86 filter needs rewritten displacements, delta needs
/// near-zero deltas, and anything else stays plain.
/// Measures every block of the member once, for the trial-free path.
///
/// The x86 opcode set is a member-level decision (see [`member_x86_type`]), so
/// the per-block x86 coverage is kept for every candidate type and the choice
/// is made by summing it — no second sweep over the member.
fn block_stats_all(
    data: &[u8],
    x86_types: &[u8],
    delta_override: Option<u8>,
    want_h1: bool,
) -> Vec<BlockStats> {
    // Blocks are independent, and this sweep is the one serial piece left in
    // front of the encoder: on ntoskrnl at `-mt8` it was 354 ms of a 1714 ms
    // run (measured as mt8 with filters minus mt8 `-mc-`). Hand the blocks to
    // the pool; the measurement per block is unchanged, so the decision is
    // byte-identical.
    let measure = |index: usize, chunk: &[u8]| {
        let start = (index * FORCED_FILTER_BLOCK) as u32;
        block_stats(chunk, start, want_h1, x86_types, delta_override)
    };
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        let pool = crate::parallel::compression_pool();
        if pool.current_num_threads() > 1 {
            let blocks: Vec<&[u8]> = data.chunks(FORCED_FILTER_BLOCK).collect();
            return pool.install(|| {
                blocks
                    .par_iter()
                    .enumerate()
                    .map(|(index, chunk)| measure(index, chunk))
                    .collect()
            });
        }
    }
    data.chunks(FORCED_FILTER_BLOCK)
        .enumerate()
        .map(|(index, chunk)| measure(index, chunk))
        .collect()
}

/// Member-level x86 choice for the trial-free path: whether to transform at
/// all, and with which opcode set, is a property of the whole stream (matches
/// cross the 64 KiB block boundaries), not of a block.
///
/// On kernel32.dll the per-block trials picked `E8` for three blocks on a
/// 7-byte margin and that was worth 11 KiB in the stream; deciding at member
/// level with the same coverage measure gets the stream-level effect instead.
fn pick_member_x86(stats: &[BlockStats], x86_types: &[u8]) -> Option<u8> {
    if let Some(forced) = std::env::var("RAR_RS_HEUR_X86")
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
    {
        return match forced {
            0 => None,
            ty if x86_types.contains(&ty) => Some(ty),
            _ => None,
        };
    }
    let mut best_cover: u64 = stats.iter().map(|s| s.cover_raw).sum();
    let mut best = None;
    for &ty in x86_types {
        let cover: u64 = stats.iter().map(|s| s.cover_x86[ty as usize]).sum();
        if cover > best_cover {
            best_cover = cover;
            best = Some(ty);
        }
    }
    best
}

/// Trial-free per-block choice from [`BlockStats`] (`RAR_RS_FILTER_HEUR`).
///
/// The arbiter is greedy match coverage, not the opcode/near-zero counts: the
/// counts say a stream *could* be transformed, coverage says the encoder will
/// actually find structure in the result. On the ntoskrnl corpus this rule
/// lands within 0.06 pp of the three-trial oracle, i.e. it keeps the filter's
/// gain and stops paying for the trials.
fn heuristic_spec(
    stats: &BlockStats,
    len: usize,
    start: u32,
    x86_type: Option<u8>,
    delta_override: Option<u8>,
) -> Option<FilterSpec> {
    let env_u64 = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(default)
    };
    // Delta needs a *big* win, not a tie: a delta block stops matching the rest
    // of the stream, and a 0.3 % block-local loss cost 11 KiB in the stream on
    // kernel32.dll while every real win on ntoskrnl was 10 %+.
    let delta_margin = env_u64("RAR_RS_HEUR_DELTA_MARGIN", 80);
    let x86_margin = env_u64("RAR_RS_HEUR_MARGIN", 0);
    let no_delta = std::env::var_os("RAR_RS_HEUR_NO_DELTA").is_some();
    let raw = stats.cover_raw;
    let mut best_cover = raw;
    let mut best: Option<FilterSpec> = None;
    if let Some(ch) = delta_override.or(stats.delta_ch)
        && !no_delta
        && stats.cover_delta * 1000 > raw.saturating_mul(1000 + delta_margin)
    {
        best_cover = stats.cover_delta;
        best = Some(FilterSpec::new(FILTER_DELTA, ch, start, len as u32));
    }
    if let Some(ty) = x86_type
        && stats.cover_x86[ty as usize] * 1000 > raw.saturating_mul(1000 + x86_margin)
        && stats.cover_x86[ty as usize] > best_cover
    {
        best = Some(FilterSpec::new(ty, 0, start, len as u32));
    }
    best
}

/// Per-block filter choice for the default `-mc` policy (WinRAR's forced
/// `-mcd+ -mce+` geometry): split `data` at [`FORCED_FILTER_BLOCK`] and give
/// each chunk a delta record, an x86 record, or nothing at all. Chunks that
/// stay plain emit no record and are left untransformed.
///
/// The default picker is trial-free (`block_stats` + `match_cover`): per block
/// it compares the greedy LZ match coverage of the plain, delta-transformed
/// and x86-transformed bytes and keeps a filter only when it clears a margin;
/// the x86 opcode set (`E8` vs `E8E9`) is decided once per member, because
/// those bytes change how the block matches its neighbours more than they
/// change the block itself. On the corpora measured so far this beats the
/// trial picker on *both* axes (ntoskrnl `-m3`: 5,780,209 B in 4.1 s vs
/// 5,789,172 B in 6.2 s).
///
/// `RAR_RS_FILTER_TRIAL=1` restores the older picker, which trial-encodes
/// plain, delta (`channels` or a per-block pick) and `x86_type` per chunk at
/// `trial_method` and keeps the smallest; `trial_method` is only a ranking
/// signal and a cheap one ranks better here (m1 beat m3 on ntoskrnl).
///
/// Tuning switches (all optional, `RAR_RS_*`): `RAR_RS_FILTER_TRIAL=1` (use
/// the trial picker instead), `RAR_RS_HEUR_MARGIN` / `RAR_RS_HEUR_DELTA_MARGIN`
/// (per-mille coverage margin the delta/x86 pick must beat plain by),
/// `RAR_RS_HEUR_X86=0|1|2` (force the member-level x86 type),
/// `RAR_RS_TRIAL_X86`, `RAR_RS_TRIAL_NO_PLAIN`, `RAR_RS_TRIAL_DELTA_CH` (same
/// for the trial picker), `RAR_RS_FILTER_STATS=2` (dump every block).
pub fn best_combined_specs(
    data: &[u8],
    method: u8,
    dsl: u8,
    variant: ArchiveVersion,
    channels: Option<u8>,
    x86_type: u8,
    trial_method: u8,
) -> Vec<FilterSpec> {
    let env_flag = |name: &str| std::env::var_os(name).is_some();
    // A zero trial method means "same as the real encode".
    let trial_method = if trial_method == 0 {
        method
    } else {
        trial_method
    };
    // The trial window is one 64 KiB block, so a dictionary larger than that
    // cannot change its decision — but it does size the encoder's tables, and
    // allocating them per trial dominated the cost of the first version.
    let trial_dsl = dsl.min(16);
    // Optional head sample per block: the trials then pack only the first
    // `sample` bytes instead of the whole 64 KiB block.
    let sample = std::env::var("RAR_RS_TRIAL_SAMPLE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1024)
        .unwrap_or(FORCED_FILTER_BLOCK);
    let trial = |bytes: &[u8], spec: Option<FilterSpec>| {
        let specs: &[FilterSpec] = spec.as_slice();
        encode_with_filters(bytes, trial_method, trial_dsl, specs, variant)
            .map(|packed| packed.len())
            .unwrap_or(usize::MAX)
    };
    // Both x86 variants are candidates by default: on ntoskrnl the mixed
    // choice packs ~20 KB smaller than E8E9 alone.
    let mut x86_types = vec![x86_type];
    if x86_type != FILTER_E8 {
        x86_types.push(FILTER_E8);
    }
    if let Ok(extra) = std::env::var("RAR_RS_TRIAL_X86") {
        x86_types.clear();
        for part in extra.split(",") {
            if let Ok(ty) = part.trim().parse::<u8>()
                && matches!(ty, FILTER_E8 | FILTER_E8E9)
                && !x86_types.contains(&ty)
            {
                x86_types.push(ty);
            }
        }
        if x86_types.is_empty() {
            x86_types.push(x86_type);
        }
    }
    let allow_plain = !env_flag("RAR_RS_TRIAL_NO_PLAIN");
    // The trial-free pick: the same geometry, but the decision comes from
    // `block_stats` instead of three trial encodes.
    // Default: decide by `block_stats`/`match_cover` (no trial encodes). The
    // trial path stays available for experiments (`RAR_RS_FILTER_TRIAL=1`).
    let heur = std::env::var("RAR_RS_FILTER_TRIAL").map_or_else(
        |_| std::env::var("RAR_RS_FILTER_HEUR").map_or(true, |v| v != "0"),
        |_| false,
    );
    let forced_delta = std::env::var("RAR_RS_TRIAL_DELTA_CH")
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .or(channels);
    let csv = env_flag("RAR_RS_TRIAL_DUMP");
    // One measuring sweep over the member decides both the opcode set and
    // every block's filter; the old version swept the member a second time
    // (`member_x86_type`) just to pick the opcode set.
    let all_stats = heur.then(|| block_stats_all(data, &x86_types, forced_delta, csv));
    let member_x86 = all_stats
        .as_ref()
        .and_then(|stats| pick_member_x86(stats, &x86_types));
    let verbose = std::env::var("RAR_RS_FILTER_STATS").ok();
    let dump = verbose.as_deref() == Some("2");
    let mut specs = Vec::new();
    let (mut plain_blocks, mut delta_blocks, mut x86_blocks) = (0u64, 0u64, 0u64);
    let (mut plain_bytes, mut delta_bytes, mut x86_bytes) = (0u64, 0u64, 0u64);
    for (index, chunk) in data.chunks(FORCED_FILTER_BLOCK).enumerate() {
        let start = (index * FORCED_FILTER_BLOCK) as u32;
        let len = chunk.len() as u32;
        let probe = &chunk[..chunk.len().min(sample)];
        let probe_len = probe.len() as u32;
        let block_x86 = if heur { member_x86 } else { Some(FILTER_E8E9) };
        let stats = if let Some(all) = &all_stats {
            Some(all[index])
        } else {
            csv.then(|| {
                block_stats(
                    chunk,
                    start,
                    true,
                    &[block_x86.unwrap_or(FILTER_E8E9)],
                    forced_delta,
                )
            })
        };
        if heur {
            let stats = stats.expect("heur implies stats");
            let spec = heuristic_spec(&stats, chunk.len(), start, member_x86, forced_delta);
            if csv {
                eprintln!(
                    "HEURBLOCK {index} start={start} len={len} ch={:?} nz={} e8={}/{}\\te9={}/{}\\t-> {:?}",
                    stats.delta_ch,
                    stats.delta_near_zero,
                    stats.e8_plausible,
                    stats.e8_total,
                    stats.e9_plausible,
                    stats.e9_total,
                    spec.map(|s| (s.filter_type, s.channels))
                );
            }
            match spec {
                None => {
                    plain_blocks += 1;
                    plain_bytes += u64::from(len);
                }
                Some(spec) if spec.filter_type == FILTER_DELTA => {
                    delta_blocks += 1;
                    delta_bytes += u64::from(len);
                    specs.push(spec);
                }
                Some(spec) => {
                    x86_blocks += 1;
                    x86_bytes += u64::from(len);
                    specs.push(spec);
                }
            }
            continue;
        }
        let plain_len = trial(probe, None);
        let mut best_len = if allow_plain { plain_len } else { usize::MAX };
        let mut best_spec: Option<FilterSpec> = None;
        let mut delta_len = usize::MAX;
        if let Some(ch) = forced_delta.or_else(|| block_delta_channel(chunk)) {
            delta_len = trial(probe, Some(FilterSpec::new(FILTER_DELTA, ch, 0, probe_len)));
            if delta_len < best_len {
                best_len = delta_len;
                best_spec = Some(FilterSpec::new(FILTER_DELTA, ch, start, len));
            }
        }
        let mut x86_len = usize::MAX;
        let mut x86_e8e9 = usize::MAX;
        let mut x86_e8 = usize::MAX;
        for &ty in &x86_types {
            let packed = trial(probe, Some(FilterSpec::new(ty, 0, 0, probe_len)));
            if packed < best_len {
                best_len = packed;
                best_spec = Some(FilterSpec::new(ty, 0, start, len));
            }
            x86_len = x86_len.min(packed);
            if ty == FILTER_E8E9 {
                x86_e8e9 = packed;
            } else if ty == FILTER_E8 {
                x86_e8 = packed;
            }
        }
        if csv {
            let s = stats.expect("csv implies stats");
            eprintln!(
                "CSVBLOCK,{index},{start},{len},{},{},{},{},{},{},{},{},{},{},{:.4},{:.4},{:.4},{},{},{}",
                s.delta_ch.unwrap_or(0),
                s.delta_near_zero,
                s.e8_plausible,
                s.e8_total,
                s.e9_plausible,
                s.e9_total,
                plain_len,
                delta_len,
                x86_e8e9,
                x86_e8,
                s.h1_raw,
                s.h1_delta,
                s.h1_x86,
                s.cover_raw,
                s.cover_delta,
                s.cover_x86
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join("/")
            );
        }
        if dump {
            eprintln!(
                "FILTERBLOCK {index} start={start} len={len} plain={plain_len} delta={delta_len} x86={x86_len} -> {:?}",
                best_spec.map(|s| (s.filter_type, s.channels))
            );
        }
        match best_spec {
            None => {
                plain_blocks += 1;
                plain_bytes += u64::from(len);
            }
            Some(spec) if spec.filter_type == FILTER_DELTA => {
                delta_blocks += 1;
                delta_bytes += u64::from(len);
                specs.push(spec);
            }
            Some(spec) => {
                x86_blocks += 1;
                x86_bytes += u64::from(len);
                specs.push(spec);
            }
        }
    }
    if verbose.is_some() {
        eprintln!(
            "FILTERSTATS blocks: plain={plain_blocks} ({} B) delta={delta_blocks} ({} B) x86={x86_blocks} ({} B) records={}",
            plain_bytes,
            delta_bytes,
            x86_bytes,
            specs.len()
        );
    }
    specs
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

/// Encode `data` with the automatic filter selection: x86 `E8`/`E8E9` for code
/// and the delta filter for correlated multi-channel data, per emitted block.
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
            assert!(matches!(
                spec.filter_type,
                FILTER_DELTA | FILTER_E8 | FILTER_E8E9
            ));
            cursor += spec.block_length;
        }
        assert_eq!(cursor as usize, data.len());
        // The validator the encoder runs must accept the sequence.
        validate_filter_specs(data.len(), &specs).unwrap();
    }

    /// The trial-free picker: plain unless a candidate clears its margin, and
    /// the bigger coverage wins between delta and x86.
    #[test]
    fn heuristic_spec_needs_a_margin_and_prefers_the_bigger_cover() {
        let raw = 10_000u64;
        let mut stats = BlockStats {
            delta_ch: Some(4),
            cover_raw: raw,
            cover_delta: raw,
            cover_x86: [raw, raw, raw],
            ..BlockStats::default()
        };
        // Neither candidate beats plain: no record at all.
        assert!(heuristic_spec(&stats, 65_536, 0, Some(FILTER_E8E9), None).is_none());
        // Delta needs 8 % by default; 5 % is not enough, 50 % is.
        stats.cover_delta = raw * 105 / 100;
        assert!(heuristic_spec(&stats, 65_536, 0, None, None).is_none());
        stats.cover_delta = raw * 150 / 100;
        let spec = heuristic_spec(&stats, 65_536, 0, None, None).expect("delta clears the margin");
        assert_eq!((spec.filter_type, spec.channels), (FILTER_DELTA, 4));
        // x86 with the bigger cover wins over the delta candidate.
        stats.cover_x86[FILTER_E8E9 as usize] = raw * 160 / 100;
        let spec =
            heuristic_spec(&stats, 65_536, 0, Some(FILTER_E8E9), None).expect("x86 beats plain");
        assert_eq!((spec.filter_type, spec.channels), (FILTER_E8E9, 0));
        // A forced channel override wins over the measured one.
        let mut forced = BlockStats {
            delta_ch: Some(4),
            cover_raw: raw,
            cover_delta: raw * 200 / 100,
            ..BlockStats::default()
        };
        forced.cover_x86 = [0; 3];
        let spec = heuristic_spec(&forced, 65_536, 0, None, Some(12)).expect("delta wins");
        assert_eq!(spec.channels, 12);
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
