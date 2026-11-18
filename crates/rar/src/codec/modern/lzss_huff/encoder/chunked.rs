//! Chunked and multithreaded encode scheduling.
//!
//! The public raw entry points and the chunk pipeline: input is processed in
//! bounded chunks so the symbol table and match finder stay proportional to
//! `chunk_size`. Under the `parallel` feature a member is additionally split
//! into slices encoded by Rayon workers and stitched back into one symbol
//! stream. Parsing lives in [`super::parse`], serialisation in
//! [`super::emit`].

use super::*;

#[cfg(feature = "parallel")]
use std::sync::atomic::Ordering;

use super::FilterSpec;
use super::emit::{encode_block, encode_empty_block};
#[cfg(feature = "parallel")]
use super::parse::find_matches_in_range;
#[cfg(feature = "parallel")]
use super::parse::{EncoderMatchState, convert_tokens};
use super::parse::{
    emitted_block_cap, find_block_end_adaptive, find_matches_optimal, find_matches_with_tail,
};

#[cfg(feature = "parallel")]
use crate::codec::common::match_finder;
#[cfg(feature = "parallel")]
use crate::error::RarError;
use crate::error::RarResult;
use crate::version::ArchiveVersion;

/// Default input chunk size for the encoder. Processing input in bounded
/// slices keeps the symbol table (and match finder) memory proportional to
/// the chunk size instead of the whole file.
pub const DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Smallest member the parallel tier is used for, in bytes.
///
/// Below a few slices' worth the pool has nothing to overlap with: the fixed
/// per-member cost (filter pick, row-index build) is paid in full while the
/// parse itself shrinks. Measured on a kernel image at `-m3 -md32m -mt24`
/// (time / archive size, sequential vs parallel):
///
/// | member | sequential | parallel |
/// | --- | --- | --- |
/// | 256 KiB | 207 ms / 115,212 | 179 ms / 115,212 |
/// | 512 KiB | 255 ms / 187,388 | 256 ms / 187,388 |
/// | 1 MiB | 363 ms / 360,979 | 218 ms / 359,082 |
/// | 2 MiB | 535 ms / 804,743 | 264 ms / 799,645 |
/// | 4 MiB | 1,103 ms / 1,810,492 | 358 ms / 1,827,026 |
///
/// So the crossover is about a mebibyte; above it the parallel tier is 1.7-3x
/// faster and within noise on ratio (one case 0.9% worse, two cases better).
pub const MT_MIN_MEMBER_SIZE: usize = 1024 * 1024;

/// Longest slice the parallel parse cuts, in bytes.
///
/// Slices exist to spread work, but a slice that spans megabytes turns every
/// candidate probe into a cold dictionary read; capping them keeps the working
/// set near L2/L3 and also shortens the distance distribution a little. See the
/// measurement table at the use site in `encode_chunked_mt_with_progress`.
#[cfg(feature = "parallel")]
const MT_MAX_SLICE: usize = 512 * 1024;

/// The encode dimensions a chunked call runs under: the compression method and
/// dictionary, the chunking, the stream's finality and distance table, plus the
/// filter lead-in the streaming writer prepends to the first chunk's symbol
/// stream. These travel together from the public entry points down to
/// [`encode_chunked_raw_inner`] / [`encode_chunked_mt_with_progress`], so they
/// are one value rather than eight positional arguments each.
///
/// The progress callback is deliberately not a field: it is the only
/// non-`Copy` member, so it stays a separate parameter and this stays a
/// `Copy` value.
#[derive(Clone, Copy)]
pub(crate) struct EncodeSpec<'a> {
    /// Compression method: 0 = STORE, 1..=5 = fastest..best.
    pub method: u8,
    /// Dictionary size log (`128 KiB << log`).
    pub dict_size_log: u8,
    /// Input chunk size; the symbol table and finder stay proportional to it.
    pub chunk_size: usize,
    /// Last call of one member: only then does the stream carry its end block.
    pub is_final: bool,
    /// RAR5 (64-entry) or RAR7/v70 (80-entry) distance table.
    pub variant: ArchiveVersion,
    /// Filter records of a filtered member, in the coordinate system of the
    /// data buffer (member-absolute for whole-member calls, window-relative for
    /// the streaming writer).
    ///
    /// A record for a late region must not be front-loaded: RARLAB readers
    /// resolve `BlockStart` modulo the dictionary from the position produced so
    /// far (`Filter.BlockStart=(BlockStart+UnpPtr)%MaxWinSize`), so a record
    /// late region of a member larger than the dictionary describes a
    /// different area than intended — the archive decodes without a structural
    /// error and fails its CRC instead.
    pub filter_records: &'a [FilterSpec],
}

/// One multi-threaded wave's fixed parameters: everything every worker slice of
/// the wave needs that is not per-slice geometry. Built once per slice by
/// [`encode_chunked_mt_with_progress`] and read (never mutated) by the worker.
#[cfg(feature = "parallel")]
#[derive(Clone, Copy)]
struct MtJob<'a> {
    /// Hash-chain budget for the low-step tier.
    chain_len: usize,
    /// Lazy-matcher threshold for the low-step tier.
    lazy_thresh: usize,
    /// Candidates compared per position (the row-index tier's level dial).
    depth: usize,
    /// How far back the walk's candidates may reach (see [`WALK_REACH`]).
    reach: usize,
    /// Lazy lookahead for the walk (see [`walk_lazy_max`]).
    walk_lazy: u32,
    /// Shared read-only row index over the whole member (issue 15 step ③):
    /// every MT slice parses against it, which is what makes a slice
    /// self-sufficient. `None` selects the low-step chain tier (the fallback
    /// for callers that do not have one — the sequential-quality probe and
    /// tests).
    index: Option<&'a super::parse::RowIndex>,
    /// Position-indexed chain, the `RAR_RS_PREV4` alternative to `index` for
    /// the walk: same candidates, but a linear pass with sequential writes
    /// instead of the row index's counting sort. A plain slice, not the owning
    /// type, because the driver builds it wave by wave: wave `w` may read
    /// exactly what is built when it starts.
    prev4: Option<&'a [u32]>,
    /// This slice carries the member's last block.
    is_last_block_of_member: bool,
    /// Filter records of this member, as absolute offsets: every slice emits
    /// the records that start inside it (see [`slice_records`]).
    records: &'a [FilterSpec],
    /// Seed the lookbehind even when its sampled windows look random. The
    /// incompressibility probe only measures whether the tail repeats *inside
    /// itself*, which says nothing about how the slice relates to bytes from an
    /// earlier member of a solid chain (or an earlier window). Skipping the
    /// seed there leaves a hole: the near finder never sees those positions and
    /// the long-range table rejects the same distances, because its `min_dist`
    /// assumes the near finder covered them — cross-member duplicates then
    /// silently stop compressing.
    force_seed_tail: bool,
    /// Run the `-m1` walk instead of the priced DP. Same candidates, no
    /// per-position pricing: the fastest rung trades ratio for wall time.
    walk: bool,
}

/// One multi-threaded worker slice: the buffer, the half-open range inside it,
/// and the parse geometry every worker shares. Bundled because the three slice
/// helpers ([`mt_slice_symbols_row_index`], [`mt_slice_symbols_low_step`],
/// [`encode_mt_slice`]) carry the same ten values.
#[cfg(feature = "parallel")]
#[derive(Clone, Copy)]
struct MtSlice<'a> {
    /// The whole slice buffer (tail context + this slice's bytes).
    data: &'a [u8],
    /// Start of the slice inside `data`.
    s0: usize,
    /// End of the slice inside `data`.
    e0: usize,
    /// Absolute stream position of `s0` (the worker slices' long-range anchor).
    entry_len: usize,
    /// The shared, read-only long-range table.
    lr_shared: &'a match_finder::LongRange,
    /// Longest match the encoder may emit.
    max_match: usize,
    /// Dictionary size in bytes.
    dict_size: usize,
    /// Whether this level runs the long-range search at all.
    long_range: bool,
    /// RAR5 or RAR7 (v70) distance table.
    variant: ArchiveVersion,
}

/// Encode raw data into RAR5/RAR7 compressed format. `variant` selects
/// the RAR7 (v70) 80-entry distance code table (RAR5 uses 64).
pub fn encode_raw(data: &[u8], method: u8, dict_size_log: u8, variant: ArchiveVersion) -> Vec<u8> {
    encode_chunked_raw(
        data,
        method,
        dict_size_log,
        DEFAULT_CHUNK_SIZE,
        None,
        true,
        None,
        variant,
    )
    .unwrap_or_default()
}

/// Encode raw data into RAR5/RAR7 compressed format, reporting match-finder
/// progress as `(bytes_processed, total_bytes)`.
pub fn encode_with_progress_raw(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    progress: Option<&mut dyn FnMut(u64, u64)>,
    variant: ArchiveVersion,
) -> Vec<u8> {
    encode_chunked_raw(
        data,
        method,
        dict_size_log,
        DEFAULT_CHUNK_SIZE,
        None,
        true,
        progress,
        variant,
    )
    .unwrap_or_default()
}

/// Encode `data` in bounded chunks, optionally carrying encoder state
/// across calls (solid archives and multi-chunk files). `is_final` marks
/// the last call of one member so only its final block carries the
/// end-of-stream flag. Returns the compressed stream; callers fall back to
/// STORE when the result is not smaller than the input.
///
/// `variant` selects the RAR7 (v70) distance code table.
#[allow(clippy::too_many_arguments)] // public API: the historical positional form
pub fn encode_chunked_raw(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    state: Option<&mut EncoderState>,
    is_final: bool,
    progress: Option<&mut dyn FnMut(u64, u64)>,
    variant: ArchiveVersion,
) -> RarResult<Vec<u8>> {
    encode_chunked_raw_inner(
        data,
        EncodeSpec {
            method,
            dict_size_log,
            chunk_size,
            is_final,
            variant,
            filter_records: &[],
        },
        state,
        progress,
    )
}

/// Sequential variant of [`encode_chunked_raw`] that carries the filter
/// records of a streaming window. Used by the streaming writer, which hands
/// over window-relative records while the persistent encoder state carries on
/// across chunks and windows.
pub(crate) fn encode_chunked_raw_with_lead(
    data: &[u8],
    spec: EncodeSpec<'_>,
    state: Option<&mut EncoderState>,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<Vec<u8>> {
    encode_chunked_raw_inner(data, spec, state, progress)
}

// Private entry point of the encoder. Everything that describes the stream
// travels in `spec` (see [`EncodeSpec`]); only the progress callback and the
// carried encoder state stay separate (the callback is not `Copy`, and the
// state is borrowed mutably).
fn encode_chunked_raw_inner(
    data: &[u8],
    spec: EncodeSpec<'_>,
    state: Option<&mut EncoderState>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<Vec<u8>> {
    let EncodeSpec {
        method,
        dict_size_log,
        chunk_size,
        is_final,
        variant,
        filter_records: records,
    } = spec;
    if data.is_empty() {
        return Ok(encode_empty_block(variant));
    }
    if super::profile::enabled() {
        eprintln!("PROF entry seq {} B", data.len());
    }

    let level = (method as usize).clamp(1, 5);
    let (_, lazy_thresh, max_match) = LEVEL_PARAMS[level];
    // `-m1` is the "fastest" rung; a 4 KiB match cap makes the greedy matcher
    // extend every candidate up to 4 KiB, which dominated its run time (a
    // 6.6 MiB DLL took 1.43 s versus 0.37 s with a 128-byte cap, at +0.12%
    // size). Levels 2-5 keep the long cap: their priced parse uses it.
    let max_match = if level == 1 {
        max_match.min(128)
    } else {
        max_match
    };
    // Experimental (issue 15, lever B.3): a priced parse with a bounded
    // candidate budget, which is what fills the gap between `-m1` (greedy)
    // and `-m2` (priced): those two sit 10-40x apart in time, and almost all
    // of that jump is the collector, not the pricing. The decision itself
    // lives in `parse_tier` so the filtered path cannot drift from this one.
    let (chain_len, miss_threshold) = super::parse_tier(method);
    let dict_size = 128 * 1024 * (1usize << dict_size_log as u32);
    // WinRAR applies the long range search to -m2..-m5 and ignores it
    // for -m1 (fastest); it is automatic (no -mcl switch needed) and
    // mandatory for v70 dictionaries.
    let long_range = level >= 2;

    let mut local_state = EncoderState::default();
    let state = state.unwrap_or(&mut local_state);
    let chunk_size = chunk_size.max(1);

    let mut output = Vec::new();
    let mut chunk_start = 0usize;
    // Cursor into `records`: they are sorted by `block_start`, so one pass
    // over the chunks hands each record to the chunk that owns its region.
    let mut spec_idx = 0usize;
    let mut next_report = 0u64;

    while chunk_start < data.len() {
        let chunk_end = (chunk_start + chunk_size).min(data.len());
        let chunk = &data[chunk_start..chunk_end];
        // Levels 2-5 use the optimal (shortest-path) parse; level 1 keeps
        // the greedy+lazy matcher (it exists to be quick, like WinRAR's
        // own fastest rung).
        let mut symbols = if level >= 2 {
            find_matches_optimal(
                state,
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
                    // Sequential path: the state's own long-range table is
                    // used and extended inside the parse.
                    lr_shared: None,
                    lr_anchor: 0,
                    seed_tail: true,
                },
            )
        } else {
            let greedy_started = std::time::Instant::now();
            let greedy = find_matches_with_tail(
                state,
                chunk,
                chain_len,
                lazy_thresh,
                max_match,
                dict_size,
                long_range,
            );
            super::profile::add(super::profile::GREEDY, greedy_started);
            greedy
        };
        // Filter records are emitted by the chunk that owns each region's
        // start, as a delta from the chunk start (see
        // [`EncodeSpec::filter_records`]).
        let mut lead: Vec<Symbol> = Vec::new();
        while spec_idx < records.len() && (records[spec_idx].block_start as usize) < chunk_end {
            let f = &records[spec_idx];
            lead.push(Symbol::Filter {
                block_start: (f.block_start as usize).saturating_sub(chunk_start) as u32,
                block_length: f.block_length,
                filter_type: f.filter_type,
                channels: f.channels,
            });
            spec_idx += 1;
        }
        if !lead.is_empty() {
            lead.append(&mut symbols);
            symbols = lead;
        }

        let emit_started = std::time::Instant::now();
        let mut block_start = 0usize;
        while block_start < symbols.len() {
            let (block_end, _) =
                find_block_end_adaptive(&symbols, block_start, emitted_block_cap());
            let is_last = is_final && chunk_end >= data.len() && block_end >= symbols.len();
            let block_data = encode_block(&symbols[block_start..block_end], is_last, variant);
            output.extend(block_data);
            // Early bail-out: once the compressed stream already exceeds
            // the input size it can never beat STORE — stop before wasting
            // more time (callers fall back to STORE on oversized output).
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
        if let Some(cb) = progress.as_deref_mut()
            && chunk_end as u64 >= next_report
        {
            cb(chunk_end as u64, data.len() as u64);
            next_report = chunk_end as u64 + 0x10000;
        }
    }

    super::profile::dump();
    Ok(output)
}

/// Multi-threaded encoding of one contiguous window of a member (see
/// `encode_chunked_mt_with_progress`; this is the no-progress form).
#[cfg(not(feature = "parallel"))]
#[allow(clippy::too_many_arguments)] // public API: the historical positional form
pub fn encode_chunked_mt(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    seed: &mut EncoderState,
    _threads: usize,
    is_final: bool,
    variant: ArchiveVersion,
) -> Vec<u8> {
    // Without the pool this path is unreachable (callers gate on `use_mt`),
    // but it must compile: encode sequentially over the window.
    encode_chunked_raw(
        data,
        method,
        dict_size_log,
        chunk_size,
        Some(seed),
        is_final,
        None,
        variant,
    )
    .unwrap_or_default()
}

/// Multi-threaded chunked encode: each chunk is compressed on the pool and the
/// bitstream is written in chunk order, so the result is byte-identical to the
/// single-threaded path.
#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)] // public API: the historical positional form
pub fn encode_chunked_mt(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    seed: &mut EncoderState,
    threads: usize,
    is_final: bool,
    variant: ArchiveVersion,
) -> Vec<u8> {
    // No cancel flag: the no-progress form is used by tooling/examples that
    // never pass one, so this can only fail on an internal error.
    encode_chunked_mt_with_progress(
        data,
        EncodeSpec {
            method,
            dict_size_log,
            chunk_size,
            is_final,
            variant,
            filter_records: &[],
        },
        seed,
        threads,
        None,
        None,
    )
    .expect("MT encode cannot fail without a cancel flag")
}

/// Splits `data` into per-worker slices (chunk-size aligned) and encodes
/// them concurrently on the compression pool. Each worker matches against
/// the preceding plaintext — up to [`NEAR_WINDOW_MAX`] bytes ending at its
/// slice start, seeded with the entry tail for the first slice — plus a
/// shared long-range table built once over the entry history and this
/// window, so distant repeats across slices and across windows still
/// compress. Repeat-distance state starts fresh in every slice: valid
/// output, with slightly worse ratios when matches lean heavily on
/// repeat-distance symbols.
///
/// On success `seed` is updated to continue after this window: its tail
/// becomes the last `min(window, NEAR_WINDOW_MAX)` bytes of the window,
/// the long-range history absorbs the whole window, and the repeat-distance
/// cache resets. `progress` reports the input bytes covered once each wave
/// of slices completes (waves run in order, so the reports are monotonic).
/// `filter_records` carries the member's filter records in buffer coordinates;
/// each chunk or slice emits the records that start inside it (see
/// [`slice_records`] and [`EncodeSpec::filter_records`]).
#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)] // data, spec, seed, threads, progress, cancel
pub(crate) fn encode_chunked_mt_with_progress(
    data: &[u8],
    spec: EncodeSpec<'_>,
    seed: &mut EncoderState,
    threads: usize,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<Vec<u8>> {
    let EncodeSpec {
        method,
        dict_size_log,
        chunk_size,
        is_final,
        variant,
        filter_records: records,
    } = spec;
    let level = (method as usize).clamp(1, 5);
    let (chain_len, lazy_thresh, max_match) = LEVEL_PARAMS[level];
    let dict_size = 128 * 1024 * (1usize << dict_size_log as u32);
    let long_range = level >= 2;
    // Speed-first ladder: the walk parses every rung, and `RAR_RS_MT_DP=1`
    // puts the priced DP back under `-m4`/`-m5` for anyone who wants our best
    // ratio instead of our best time. The walk's speed dial is how far back it
    // may look — measured on the kernel image, 8 MB costs 492 bytes over a full
    // reach while 512 KB costs 1.27 pp — so the rungs widen with the level.
    // `RAR_RS_MT_WALK` forces the walk even over the DP.
    let force_walk = std::env::var("RAR_RS_MT_WALK").is_ok_and(|value| value != "0");
    let dp_rung = priced_dp_requested() && level >= 4;
    let walk_tier = force_walk || (level <= WALK_MAX_LEVEL && !dp_rung);
    // How far back the walk's candidates may reach. **Full window by default**:
    // capping it below `-md` silently drops matches the dictionary promises, and
    // that is catastrophic rather than merely lossy on long-range redundancy
    // (256 KiB reach: 3,070,349 B of the same markdown against 244,295 full).
    // `RAR_RS_WALK_REACH` exists to measure that trade, not to ship it.
    let walk_reach = std::env::var("RAR_RS_WALK_REACH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value >= 4096)
        .unwrap_or(dict_size)
        .min(dict_size);
    // MT parse is the low-step tier by design: workers price a bounded
    // candidate set from the shared row index instead of descending the BT4
    // tree at every position, which cuts the per-position step count well
    // below the tree's. It accepts MT output divergence from the sequential
    // path (already a documented, accepted divergence) and is the MT fastest
    // measured configuration (tsc x86 2.1x, repeated text 5.2x on the A/B
    // corpus); seq is never affected. The parse now carries each DP block's
    // price table into the next block (`windowed_priced_parse`), which
    // recovered a further 0.4-0.8pp on text without extra work.
    // Adaptive slice size: with a fixed 4 MiB slice a 13 MB member gives
    // only 4 slices and the pool sits mostly idle. Target ~2x the thread
    // count slices (floor 2 MiB) so medium members parallelize properly;
    // members whose caller chunk already yields plenty of slices (big
    // members, repetitive text) keep the caller's size unchanged.
    let cs = {
        let caller = chunk_size.max(1);
        // Slices per worker: every extra slice pays one price-table restart
        // (the slice's first DP block is priced from scratch) — ~1.9% at
        // 290 KiB against ~0.2% at 580 KiB on ntoskrnl. One slice per worker
        // measured equal in wall time to two and 0.22 pp smaller (13 KB) at 24
        // workers, so it is the default; workers are balanced by the rule that
        // re-splits the member into a whole number of waves, and
        // `RAR_RS_MT_SPW` raises the slice count when a machine wants slack
        // for stragglers.
        let spw = std::env::var("RAR_RS_MT_SPW")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|&value| value > 0)
            .unwrap_or(1);
        let by_count = data.len() / (threads.max(1) * spw);
        // Smallest slice the scheduler will cut: slices below this lose more to
        // the per-slice rep/price reset than the extra workers return. 128 KiB
        // is also the official's wave granularity (`threads x 128 KiB`).
        let floor = std::env::var("RAR_RS_MT_MIN_SLICE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(128 * 1024);
        let floored = by_count.max(caller.min(floor));
        // Cap: a slice that spans megabytes makes every candidate probe a cold
        // DRAM read inside the dictionary, so the parse slows per byte even
        // though each worker still has work. Measured on a 202 MB member (three
        // 67 MB chunks, 24 workers): 2.8 MB slices take 10.5 s of waves, while
        // capping at 1 MiB ...
        // Largest slice the parse wants: a slice that spans megabytes makes
        // every candidate probe a cold read inside the dictionary window, so
        // the cost per byte grows with the member even though each worker still
        // has work queued. Measured with `-m3 -md32m -mt24` on DLL corpora
        // (time / archive size), uncapped -> 512 KiB cap:
        //
        // | member | uncapped | capped |
        // | --- | --- | --- |
        // | 53 MB | 5,077 ms / 12,926,745 | 3,304 ms / 12,914,454 |
        // | 202 MB | 21,714 ms / 54,473,456 | 17,597 ms / 54,428,923 |
        // | 1,000 MB | 80.1 s / 286,406,261 | 62.0 s / 286,414,589 |
        //
        // 256 KiB was slower than 512 KiB on the 1 GB member (64.6 s) for the
        // same size, and it starts to bite on 13.9 MB members (slices are
        // ~580 KiB there), so 512 KiB is the cap. `-m0` floor for the 202 MB
        // member is 345 ms, i.e. this is all parse.
        let cap = std::env::var("RAR_RS_MT_MAX_SLICE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|&value| value > 0)
            .unwrap_or(MT_MAX_SLICE);
        // A filter record is emitted by the slice that owns its region start,
        // and a reader resolves it modulo the window from the position it has
        // produced — so under a small dictionary the slice has to fit inside
        // the window (see [`EncodeSpec::filter_records`]). Unfiltered members
        // keep the plain cap.
        let window_cap = if records.is_empty() {
            usize::MAX
        } else {
            (dict_size / 2).max(64 * 1024)
        };
        let cs = caller.min(floored).min(cap).min(window_cap).max(1);
        // Snap the slice count to a whole number of waves. `len / cs` usually
        // lands one past a multiple (13,917,568 / 289,949 = 48.0006 -> 49), and
        // that lone last slice costs a whole wave of wall time for 16 bytes of
        // work: 48 slices is 2 waves on 24 workers, 49 is 3. Redividing the
        // member by the snapped count keeps the slices near-equal.
        let wanted = data.len().div_ceil(cs);
        let workers = threads.max(1);
        let waves = ((wanted + workers / 2) / workers).max(1);
        data.len().div_ceil(waves * workers)
    };

    // Shared long-range table over entry history plus this window; built
    // once, read-only afterwards (workers query with absolute anchors).
    // Take over the existing history instead of copying it: each wave used
    // to duplicate up to 128 MiB of retained bytes and re-index them before
    // every member of a solid chain, which cost more than the parallel
    // encode saved. Only a different dictionary forces a rebuild (the
    // window bounds every candidate, so a stale one would be wrong).
    let mut lr_shared = match seed.long_range.take() {
        Some(lr) if lr.window() == dict_size => lr,
        Some(lr) => {
            let mut rebuilt = match_finder::LongRange::new(dict_size);
            rebuilt.push(lr.hist_bytes());
            rebuilt
        }
        None => match_finder::LongRange::new(dict_size),
    };
    // The previous member's bytes enter the table here, and this member's wait for
    // the next one: a standalone member never queries the table (`lr_q` is
    // `None`), so indexing one member ahead skips the whole push for the last
    // member of an archive — which is all of it when there is only one member.
    // The leading bytes past the history bound are accounted for by count only.
    let pending = std::mem::take(&mut seed.pending);
    let pending_skipped = std::mem::take(&mut seed.pending_skipped);
    // Absolute stream position of `data[0]`. It is the *pushed* total, not
    // the retained history length: once the sampled window has slid, `hist`
    // starts at `hist_base() > 0` and the two differ by exactly that much —
    // using `hist_len()` here anchors every worker query short and silently
    // drops long-range matches later in a chain. The pending bytes belong before
    // `data`, so they count here even though the push is still to come.
    let entry_len = lr_shared.total_pushed() + pending_skipped + pending.len();
    let seed_tail = std::mem::take(&mut seed.tail);

    // Slice boundaries: one fixed chunk per slice keeps the near-window
    // reach (tail + slice) identical to the sequential path, so the
    // long-range band stays live; slices run in waves of `threads` with
    // results appended wave-by-wave (bounded memory, deterministic output
    // independent of completion order).
    let total_chunks = data.len().div_ceil(cs).max(1);
    let n_workers = threads.clamp(1, 64);
    let mut bounds = vec![0usize];
    let mut c = 0usize;
    while c < total_chunks {
        c += 1;
        let b = (c * cs).min(data.len());
        if b <= *bounds.last().unwrap() {
            break;
        }
        bounds.push(b);
    }
    if *bounds.last().unwrap() != data.len() {
        bounds.push(data.len());
    }
    let n = bounds.len() - 1;

    // Issue 15 step ③: one shared, read-only row index over the whole buffer,
    // built once per call. With it the workers never build a lookbehind frame,
    // so the per-slice insert of up to 8 MiB per 2 MiB slice disappears and the
    // priced DP becomes affordable at MT speed.
    // The long-range table and the row index both only need `data`, and
    // neither reads the other: run them at once instead of one after the
    // other. Serial they cost 28 ms + 100 ms of the member's wall time; the
    // pair takes the longer of the two.
    let mt_clock = std::env::var_os("RAR_RS_MTTIME").is_some();
    let pool = crate::parallel::compression_pool();
    // The row index is the member-wide candidate structure every worker queries.
    // `RAR_RS_NOINDEX` drops it and falls back to the per-slice chain tier,
    // which pays its lookbehind insert per slice instead of a whole-member
    // build: the measurement that decides whether the build is worth its
    // queries.
    // The fast rungs take the official's shape for the candidate structure: a
    // position-indexed table filled in one sequential pass per part, with no
    // counting sort. On the kernel image at `-m3` that is 42 ms of prelude
    // against the row index's 120 ms, and at a shallow candidate depth the
    // chain's pointer chase does not show (78 ms of waves at depth 4 against 89).
    // At depth 64 the chase loses badly (141 against 110), so the ratio rungs
    // keep the sorted index. `RAR_RS_PREV4` forces it everywhere.
    let use_prev4 = walk_tier && (super::parse::prev4_enabled() || level <= PREV4_MAX_LEVEL);
    // The candidate depth is the MT tier's level dial: [`PREV4_DEPTH`] for the
    // position-indexed rungs, whose chain is a pointer chase, and
    // [`MT_ROW_INDEX_DEPTH`] for the sorted ones, whose bucket walk is
    // contiguous. `RAR_RS_MT_DEPTH` overrides both.
    let row_depth = std::env::var("RAR_RS_MT_DEPTH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(if use_prev4 {
            PREV4_DEPTH[level]
        } else {
            MT_ROW_INDEX_DEPTH[level]
        });
    let need_index = !no_index_enabled() && !use_prev4;
    // The chain's first wave is built here; the rest is built one wave ahead in
    // the loop below. A chain cannot be built in independent ranges — each
    // range's head tables would start empty and a walk stops at that range's
    // first occurrence of a prefix (measured on 9,251,209 B of markdown: exact
    // 244,419 B, 2 ranges 393,459, 8 ranges 1,231,478) — so building all of it up
    // front costs 48 ms of serial prelude on a 13.9 MB member. Extending it is
    // one sequential pass over the new range, and wave `w` only ever reads
    // `prev[..bounds[last]]`, so the ranges stay disjoint and that pass runs
    // while the wave it feeds parses.
    let first_wave_end = bounds[n_workers.min(n)];
    let lr_stage = std::time::Instant::now();
    let ((), (row_index, mut chain_builder)) = pool.join(
        || {
            if pending_skipped > 0 {
                lr_shared.skip(pending_skipped);
            }
            lr_shared.push(&pending);
        },
        || {
            (
                need_index.then(|| super::parse::RowIndex::build_with_threads(data, n_workers)),
                use_prev4.then(|| {
                    let mut builder = super::parse::Prev4Builder::new(data.len());
                    builder.extend(data, first_wave_end);
                    builder
                }),
            )
        },
    );
    if mt_clock {
        eprintln!(
            "MTTIME prelude={}ms (long_range + chain prefix + row_index together)",
            lr_stage.elapsed().as_millis()
        );
    }
    let row_index_ref = row_index.as_ref();
    let mt_waves = std::time::Instant::now();
    // Rayon's Scope::spawn returns nothing; each worker deposits its
    // packed bytes into its own slot and we collect them wave-by-wave in
    // order after the scope joins everything.
    // One persistent encoder state per worker, reused across waves: the
    // low-step parse's `head`/`prev` ring arrays stay warm (a fresh 64 MiB
    // two-array allocation per slice cost page faults and per-frame
    // memsets). Waves run sequentially so each state is used by exactly one
    // thread at a time.
    let mt_states = std::time::Instant::now();
    let mut worker_states: Vec<EncoderState> =
        (0..n_workers).map(|_| EncoderState::default()).collect();
    if mt_clock {
        eprintln!("MTTIME states={}ms", mt_states.elapsed().as_millis());
    }
    let mut output = Vec::new();
    // Two-phase mode collects each slice's parsed runs instead of its blocks;
    // the serial pass below turns them into symbols and blocks.
    let two_phase = slice_two_phase();
    let mut collected: Vec<Vec<(u32, u32)>> = Vec::new();
    let mut first = 0usize;
    while first < n {
        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        let last = (first + n_workers).min(n);
        let next_last = (last + n_workers).min(n);
        // Split the chain: this wave reads `prev[..built]`, and one more task in
        // the same scope fills `prev[built..next_end]`. The ranges are disjoint
        // (the workers never look past their own position, which is below
        // `built`), so the extension costs no wall time.
        let mut extend_task: Option<(&mut [u32], &mut [u32], usize)> = None;
        let chain_ref: Option<&[u32]> = match chain_builder.as_mut() {
            Some(builder) => {
                let built = builder.built;
                if next_last > last {
                    let to = bounds[next_last].min(data.len().saturating_sub(3));
                    let (read_part, rest) = builder.prev.split_at_mut(built);
                    let (write_part, _) = rest.split_at_mut(to - built);
                    extend_task = Some((&mut builder.heads, write_part, built));
                    Some(&*read_part)
                } else {
                    Some(&builder.prev[..built])
                }
            }
            None => None,
        };
        let wave = &mut worker_states[..last - first];
        let results = std::sync::Mutex::new(
            (0..last - first)
                .map(|_| None::<SliceOut>)
                .collect::<Vec<_>>(),
        );
        {
            // Shared read-only handles for the worker closures.
            let tail_ref = &seed_tail;
            let lr_ref = &lr_shared;
            let results_ref = &results;
            pool.scope(|scope| {
                if let Some((heads, out, from)) = extend_task {
                    scope.spawn(move |_| super::parse::prev4_extend(heads, data, out, from));
                }
                for (i, state) in wave.iter_mut().enumerate() {
                    let k = first + i;
                    let (s0, e0) = (bounds[k], bounds[k + 1]);
                    scope.spawn(move |_| {
                        let blocks = encode_mt_slice(
                            MtSlice {
                                data,
                                s0,
                                e0,
                                entry_len,
                                lr_shared: lr_ref,
                                max_match,
                                dict_size,
                                long_range,
                                variant,
                            },
                            &MtJob {
                                chain_len,
                                lazy_thresh,
                                depth: row_depth,
                                reach: walk_reach,
                                walk_lazy: walk_lazy(),
                                index: row_index_ref,
                                prev4: chain_ref,
                                is_last_block_of_member: is_final && k + 1 == n,
                                records,
                                // Only the first slice sees chain history as its
                                // lookbehind; later slices look into this window.
                                force_seed_tail: k == 0 && !tail_ref.is_empty(),
                                walk: walk_tier,
                            },
                            tail_ref,
                            state,
                        );
                        results_ref.lock().unwrap()[i] = Some(blocks);
                    });
                }
            });
        }
        for r in results.into_inner().unwrap() {
            match r.expect("worker slot filled") {
                SliceOut::Bytes(bytes) => output.extend(bytes),
                SliceOut::Tokens(tokens) => collected.push(tokens),
            }
        }
        if let Some(cb) = progress.as_deref_mut()
            && bounds[last] > bounds[first]
        {
            cb(bounds[last] as u64, data.len() as u64);
        }
        if let Some(builder) = chain_builder.as_mut()
            && next_last > last
        {
            // The extension task above has finished with the scope.
            builder.built = bounds[next_last].min(data.len().saturating_sub(3));
        }
        first = last;
    }

    if two_phase {
        // ADR 0012 phase 2: walk the slices in order with ONE repeat cache, so
        // a distance that repeats what the previous slice emitted stays a
        // `CacheRef`/`Repeat` instead of being re-priced as a fresh match. This
        // is the pass the official decoder's serial adjudicator performs
        // (`FUN_140044600`); it costs one `convert_tokens` sweep per slice and
        // is why mt1..mt24 differ by ~125 B there.
        let mut shared = EncoderMatchState::new([0u32; DIST_CACHE_SIZE], 0);
        for (k, tokens) in collected.iter().enumerate() {
            let (mut symbols, _freqs) =
                convert_tokens(tokens, data, bounds[k]..bounds[k + 1], &mut shared, variant);
            let mut lead = slice_records(records, bounds[k], bounds[k + 1]);
            if !lead.is_empty() {
                lead.append(&mut symbols);
                symbols = lead;
            }
            let mut bs = 0usize;
            while bs < symbols.len() {
                let (be, _) = find_block_end_adaptive(&symbols, bs, emitted_block_cap());
                let is_last = is_final && k + 1 == n && be >= symbols.len();
                output.extend(encode_block(&symbols[bs..be], is_last, variant));
                bs = be;
            }
        }
    }

    if mt_clock {
        eprintln!(
            "MTTIME waves={}ms slices={n} workers={n_workers}",
            mt_waves.elapsed().as_millis()
        );
    }

    // Continue the chain after this window: tail = suffix of the window
    // (seeded with the entry tail so it spans windows like sequential
    // mode), long-range state swaps in the shared table, repeat-distance
    // cache resets (documented divergence from the sequential path).
    let keep = dict_size.min(near_window_max());
    if keep <= data.len() {
        seed.tail = data[data.len() - keep..].to_vec();
    } else {
        let take = (keep - data.len()).min(seed_tail.len());
        let st = seed_tail.len();
        let mut t = Vec::with_capacity(take + data.len());
        t.extend_from_slice(&seed_tail[st - take..]);
        t.extend_from_slice(data);
        seed.tail = t;
    }
    seed.dist_cache = [0u32; DIST_CACHE_SIZE];
    seed.last_length = 0;
    // Hold this member's bytes for the member that may follow instead of indexing
    // them now (see the pending push above). Only the last `max_hist` bytes can
    // ever be candidates, so anything older is remembered as a count.
    let hold = match_finder::LONG_RANGE_MAX.min(dict_size.max(match_finder::LONG_RANGE_STEP));
    if data.len() > hold {
        seed.pending_skipped = data.len() - hold;
        seed.pending = data[data.len() - hold..].to_vec();
    } else {
        seed.pending_skipped = 0;
        seed.pending.clear();
        seed.pending.extend_from_slice(data);
    }
    seed.long_range = Some(lr_shared);
    // Worker slices are independent frames, so the sequential path's
    // persistent tree must not survive a window the MT path consumed: its
    // links are frame offsets, and a later sequential member would rebase
    // them against the wrong history (the silent corruption this project
    // already fixed once for cross-chunk growth).
    seed.tree = None;
    seed.combined_len = 0;
    Ok(output)
}

/// Cheap per-slice probe: would inserting this tail into the low-step
/// finder ever pay off? A tail whose sampled 4-byte windows are (almost)
/// all distinct has no long repeats, so seeding it is wasted work — random
/// media, compressed/encrypted data — while text, code and structured
/// binary keep their repeated windows and seed normally. Shares the window
/// statistic with the write path's incompressibility screen
/// ([`crate::codec::common::incompressible::distinct_window_percent`]).
#[cfg(feature = "parallel")]
fn mt_tail_is_incompressible(tail: &[u8]) -> bool {
    const PROBE_LEN: usize = 256 * 1024;
    const DISTINCT_PERCENT: usize = 95;
    let probe = &tail[..tail.len().min(PROBE_LEN)];
    crate::codec::common::incompressible::distinct_window_percent(probe)
        .is_some_and(|percent| percent >= DISTINCT_PERCENT)
}

/// Candidates the MT row-index parse compares per position, by level: this is
/// the MT tier's speed/ratio dial, the job the chain budget did for the old
/// greedy tier, and what makes `-m` mean something under `-mt` again.
///
/// Measured at `-mt24`, dict 32m, real CLI on a 13.9 MB kernel image and a
/// 5.9 MB source tree (`level 0 is store, unused`). Sizes are exact; times
/// are ranges because this machine drifts ±25% between runs — taken from
/// interleaved repeats, never from one pass:
///
/// | level | depth | 13.9 MB kernel  | 5.9 MB sources |
/// | ----- | ----- | --------------- | -------------- |
/// | 1     | 16    | 530-545 / 5,969,224 | 300-310 / 1,167,853 |
/// | 2     | 24    | 520-650 / 5,936,057 | 230-245 / 1,151,079 |
/// | 3     | 64    | 575-595 / 5,878,519 | 270-280 / 1,119,405 |
/// | 4     | 128   | 635-675 / 5,850,530 | 305-315 / 1,103,730 |
/// | 5     | 384   | 765-925 / 5,820,788 | 435-445 / 1,087,081 |
///
/// The rungs are spaced so each one buys a similar slice of ratio rather than
/// doubling the dial: on the kernel the steps are 0.55%, 0.96%, 0.47% and
/// 0.51%. The old ladder (16/32/64/128/256) spent its budget backwards — its
/// steps were 0.88%, 0.64%, 0.48% and 0.35%, so the top rung was starved
/// while the bottom two overlapped. Depth is a weak cost dial (48x the depth
/// moves the kernel by about 1.5x in time) but a real ratio dial (3.7% of
/// size over the same range), which is why the rungs are chosen by how much
/// size each step buys.
///
/// The packed column is monotone in depth on both corpora; the **time is not**,
/// which is worth remembering when tuning: a deeper walk finds the long match
/// that lets the priced DP commit and skip every position it covers, so on text
/// m3 -> m5 is both smaller *and* faster (2167 -> 1891 ms), while on dense
/// binaries depth buys ratio at a real time cost (2106 -> 2791 ms on the DLL).
///
/// For scale, the sequential parse of the DLL at m3 is 5,751,821 B / 6727 ms and
/// of the source tree 922,257 B / 2669 ms, so m5 lands within 1% of the
/// sequential ratio on the DLL and *below* it on the source tree, while m1 stays
/// the cheap rung (+2.3% on the source tree, still 3.6x faster than sequential).
/// MT-only, like the row-index parse whose level dial it is.
#[cfg(feature = "parallel")]
const MT_ROW_INDEX_DEPTH: [usize; 6] = [0, 16, 32, 64, 128, 384];

/// Highest level that takes the position-indexed candidate table instead of the
/// sorted row index (see [`PREV4_DEPTH`]).
#[cfg(feature = "parallel")]
const PREV4_MAX_LEVEL: usize = 3;

/// Candidate depth for the position-indexed rungs. Their chain is a pointer
/// chase, so cost climbs quickly with depth — best of five at `-m1` on the
/// kernel image: depth 1 198 ms / 48.21%, 2 **183** / 47.14%, 3 206 / 46.64%,
/// 4 220 / 46.32%, 6 224 / 45.94%, 8 255 / 45.71%, 16 281 / 45.25%. Depth 1 is
/// *slower* than 2 because the main source usually finds nothing and the
/// short-match fallback then runs for every position. The ladder keeps the
/// cheapest point that still improves on the rung below.
#[cfg(feature = "parallel")]
const PREV4_DEPTH: [usize; 6] = [0, 2, 4, 8, 0, 0];

/// Highest level the walk parses. Every rung: the walk is what lets each level
/// match the official's time for that level (measured at `-md32m -mt24`,
/// 249/261/269/321/359 ms against the official's 138/195/239/377/388 ms), and
/// the priced DP costs 1.6-1.8x that for 1.5-2.2 pp of ratio.
#[cfg(feature = "parallel")]
const WALK_MAX_LEVEL: usize = 5;

/// The walk's one-step lazy lookahead for every rung: a match shorter than this
/// makes the walk also price the match that starts one byte later, and take the
/// literal if that one is longer.
///
/// This is the cheapest ratio we have found. On the kernel image (`-md32m -mt24`)
/// the curve is (internal time in ms, packed size) at `-m3` / `-m5`:
/// 8 -> 157/6,361,369 and 236/6,070,109; 16 -> 165/6,347,890 and 249/6,049,159;
/// 32 -> 162/6,346,001 and 250/6,046,079; 64 -> 169/6,345,598 and 259/6,045,537;
/// 128 -> 167/6,345,539 and 249/6,045,396. So 32 is the knee and 64 is within
/// 400-700 B of the plateau; both are worth the few ms because these rungs are
/// already at or under the official's time for the level. `RAR_RS_WALK_LAZY`
/// overrides it.
#[cfg(feature = "parallel")]
const WALK_LAZY_MAX: u32 = 64;

#[cfg(feature = "parallel")]
fn walk_lazy() -> u32 {
    static OVERRIDE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    OVERRIDE
        .get_or_init(|| {
            std::env::var("RAR_RS_WALK_LAZY")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
        })
        .unwrap_or(WALK_LAZY_MAX)
}

/// `RAR_RS_MT_DP=1` puts the priced DP back under `-m4`/`-m5`: our best ratio
/// (41.29% at `-m5` against the walk's 43.62%) for 1.7-2.1x the wall time.
///
/// The environment is read once per process, which a test cannot vary; the
/// seam below forces it instead (same shape as the other test switches).
#[cfg(feature = "parallel")]
fn priced_dp_requested() -> bool {
    #[cfg(test)]
    if DP_TIER_FORCED.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RAR_RS_MT_DP").is_ok_and(|value| value != "0"))
}

#[cfg(all(test, feature = "parallel"))]
pub(crate) static DP_TIER_FORCED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(all(test, feature = "parallel"))]
pub(crate) fn set_dp_tier(enabled: bool) {
    DP_TIER_FORCED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// How far one DP block spans in the MT row-index parse. The driver in
/// `parse.rs` owns the number; this is the value it uses.
///
/// MT-only, like the parse it feeds.
#[cfg(feature = "parallel")]
pub(super) const MT_ROW_INDEX_DP_BLOCK: usize = super::parse::MT_DP_BLOCK_SIZE;

/// MT-only row-index parse (issue 15 / step ③): the sequential path's optimal
/// parse over candidates from a **shared, read-only** row index built once per
/// member, instead of the greedy tier's per-frame hash chain.
///
/// With the index the workers never build a frame at all — no tail copy, no
/// hash insert of up to 8 MiB of lookbehind per 2 MiB slice, no per-worker ring
/// arrays — which is what makes the priced DP affordable at MT speed, and every
/// worker can reach the member's whole dictionary instead of just its tail.
///
/// Measured at `-m3 -mt8` against the greedy chain tier it replaces: on the
/// 12.5 MiB DLL 5,858,387 B / 2013 ms against 6,516,302 B / 1769 ms, and on a
/// 75 MB binary stream 17,838,318 B / 15936 ms against 20,021,162 B / 7513 ms —
/// the streaming number is within 0.32% of the *sequential* parse (17,894,515 B,
/// ~28 s) at 1.75x its speed.
///
/// On text it costs ratio at the default level (4.88 MiB source tree: 935,472 B
/// against sequential 922,257 B, for 2.4x the speed) because this tier spends its
/// budget on candidates while the sequential path spends it on 2-4 pricing
/// passes; deeper levels beat sequential there instead (m5: 914,457 B against
/// 921,710 B). The level's candidate depth is [`MT_ROW_INDEX_DEPTH`].
/// What one worker slice hands back: finished blocks, or (two-phase mode,
/// ADR 0012) the parsed runs for the serial adjudication pass.
#[cfg(feature = "parallel")]
enum SliceOut {
    Bytes(Vec<u8>),
    Tokens(Vec<(u32, u32)>),
}

/// Two-phase MT (ADR 0012): parse slices in parallel, adjudicate distances
/// and emit serially. Off unless `RAR_RS_MT_TWO_PHASE` is set.
#[cfg(feature = "parallel")]
fn slice_two_phase() -> bool {
    std::env::var("RAR_RS_MT_TWO_PHASE").is_ok_and(|value| value != "0")
}

/// Drop the member-wide row index and use the per-slice chain tier instead
/// (`RAR_RS_NOINDEX`, off by default). The prelude's build is ~100 ms of a
/// fast member's wall time, but it is also what makes every worker's query
/// cheap; this switch measures both sides of that trade.
#[cfg(feature = "parallel")]
fn no_index_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RAR_RS_NOINDEX").is_ok_and(|value| value != "0"))
}

#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)] // the slice, its search dials and the defer slot
fn mt_slice_symbols_row_index(
    state: &mut EncoderState,
    slice: MtSlice<'_>,
    index: &super::parse::RowIndex,
    // Candidates compared per position; the MT tier's level dial.
    depth: usize,
    // How far back the walk's candidates may reach (see [`WALK_REACH`]).
    reach: usize,
    // Lazy lookahead for the walk (see [`walk_lazy_max`]).
    lazy: u32,
    // Take the `-m1` walk instead of the priced DP.
    walk: bool,
    defer: Option<&mut Vec<(u32, u32)>>,
) -> Vec<Symbol> {
    let MtSlice {
        data,
        s0,
        e0,
        entry_len,
        lr_shared,
        max_match,
        dict_size,
        long_range,
        variant,
    } = slice;
    // A solid chain reaches back into earlier members, and those bytes are not
    // in `data`: only the sampled table can see them. A standalone member needs
    // no table (the row index covers its whole window), which is worth ~12% of
    // the wall time on a 13.9 MB member.
    let lr_q = if long_range && entry_len > 0 {
        // At `s0` the table owns `s0` bytes of history and one more per position
        // after that.
        Some((lr_shared, s0, entry_len + s0))
    } else {
        None
    };
    let mut dist_cache = state.dist_cache;
    let mut last_length = state.last_length;
    // The reach cap is the walk's dial; the priced DP keeps the whole window,
    // since its ratio is what the top rungs are for.
    let symbols = if walk {
        super::parse::windowed_walk_parse(
            data,
            s0..e0,
            &mut dist_cache,
            &mut last_length,
            super::parse::MatchLimits {
                max_match,
                window: dict_size,
                variant,
            },
            lazy,
            |pos, _cache, runs| {
                index.collect(data, pos, reach.min(pos), max_match, depth, runs);
                // A solid chain reaches back into earlier members, and those
                // bytes are not in `data`: the sampled table is the only source
                // that can see them (the same probe `windowed_priced_parse`
                // makes). Standalone members pass `lr_q = None`.
                if let Some((long_range, near_reach, anchor)) = lr_q
                    && pos + 4 <= e0
                {
                    let chunk_off = pos - s0;
                    if let Some((long_distance, long_length)) = long_range.find_from(
                        &data[s0..e0],
                        chunk_off,
                        anchor,
                        near_reach + chunk_off + 1,
                        max_match,
                    ) {
                        runs.push((long_length as u32, long_distance));
                    }
                }
            },
        )
    } else {
        super::parse::windowed_priced_parse(
            data,
            s0..e0,
            &mut dist_cache,
            &mut last_length,
            super::parse::MatchLimits {
                max_match,
                window: dict_size,
                variant,
            },
            lr_q,
            defer,
            |pos, _cache, runs| {
                index.collect(data, pos, dict_size.min(pos), max_match, depth, runs)
            },
        )
    };
    state.dist_cache = dist_cache;
    state.last_length = last_length;
    symbols
}

/// The walk over the position-indexed four-byte chain (`RAR_RS_PREV4`).
///
/// Same parser and same candidate contract as
/// [`mt_slice_symbols_row_index`], but the candidates come from
/// [`super::parse::prev4_collect`] instead of the counting-sorted row index, so
/// the member-wide prelude is a linear pass instead of a bucket sort.
#[cfg(feature = "parallel")]
fn mt_slice_symbols_prev4(
    state: &mut EncoderState,
    slice: MtSlice<'_>,
    chain: &[u32],
    depth: usize,
    reach: usize,
    lazy: u32,
) -> Vec<Symbol> {
    let MtSlice {
        data,
        s0,
        e0,
        entry_len,
        lr_shared,
        max_match,
        dict_size,
        long_range,
        variant,
    } = slice;
    // A solid chain reaches back into earlier members, and those bytes are not
    // in `data`: only the sampled table can see them (the same probe
    // `mt_slice_symbols_row_index` makes).
    let lr_q = if long_range && entry_len > 0 {
        Some((lr_shared, s0, entry_len + s0))
    } else {
        None
    };
    let mut dist_cache = state.dist_cache;
    let mut last_length = state.last_length;
    let symbols = super::parse::windowed_walk_parse(
        data,
        s0..e0,
        &mut dist_cache,
        &mut last_length,
        super::parse::MatchLimits {
            max_match,
            window: dict_size,
            variant,
        },
        lazy,
        |pos, _cache, runs| {
            super::parse::prev4_collect(chain, data, pos, reach.min(pos), max_match, depth, runs);
            if let Some((long_range, near_reach, anchor)) = lr_q
                && pos + 4 <= e0
            {
                let chunk_off = pos - s0;
                if let Some((long_distance, long_length)) = long_range.find_from(
                    &data[s0..e0],
                    chunk_off,
                    anchor,
                    near_reach + chunk_off + 1,
                    max_match,
                ) {
                    runs.push((long_length as u32, long_distance));
                }
            }
        },
    );
    state.dist_cache = dist_cache;
    state.last_length = last_length;
    symbols
}

/// Filter records owned by one slice `[from, to)`, as deltas from the slice
/// start — the position the reader has produced when it decodes the slice's
/// first symbol.
///
/// Splitting this way keeps every record within one slice of the bytes it
/// transforms, which the reader requires: it resolves `BlockStart` modulo the
/// window from the current produced position, so front-loading the member's
/// records at offset zero only works while the dictionary covers the member.
#[cfg(feature = "parallel")]
fn slice_records(records: &[FilterSpec], from: usize, to: usize) -> Vec<Symbol> {
    records
        .iter()
        .filter(|f| {
            let start = f.block_start as usize;
            start >= from && start < to
        })
        .map(|f| Symbol::Filter {
            block_start: (f.block_start as usize).saturating_sub(from) as u32,
            block_length: f.block_length,
            filter_type: f.filter_type,
            channels: f.channels,
        })
        .collect()
}

/// Encode one worker slice `[s0, e0)` of [`encode_chunked_mt`].
///
/// Each worker runs the MT low-step parse (hash-chain greedy+lazy over the
/// slice's tail context, shared read-only long-range table): a bounded
/// chain walk + lazy skip per position instead of the sequential path's
/// tree descent. That divergence is the accepted price for MT speed (see
/// [`encode_chunked_mt`]).
#[cfg(feature = "parallel")]
fn encode_mt_slice(
    slice: MtSlice<'_>,
    job: &MtJob<'_>,
    seed_tail: &[u8],
    state: &mut EncoderState,
) -> SliceOut {
    let MtSlice {
        data,
        s0,
        e0,
        dict_size,
        variant,
        ..
    } = slice;
    let MtJob {
        chain_len,
        lazy_thresh,
        depth,
        reach,
        walk_lazy,
        index,
        prev4,
        is_last_block_of_member,
        records,
        force_seed_tail,
        walk,
    } = *job;
    // Near-window context: the closest bytes before this slice, seeded
    // with the entry tail when the slice starts at the buffer head.
    // The window cap matches the sequential path's `NEAR_WINDOW_MAX`, so
    // matches in the (2 MiB, 8 MiB) band stay reachable instead of riding
    // the sampled long-range table: without it, distant exact copies fell
    // to ~STORE on small slices (window reach = tail + slice length, so
    // mt8's 2 MiB slices couldn't see even a 4 MiB-back copy). Aligning the
    // cap costs a longer per-slice tail as the low-step chain's lookbehind —
    // the far band is inserted into the finder, and the shared long-range
    // table covers everything beyond it.
    // Issue 15 step ③: with the shared row index there is no lookbehind frame
    // to build at all — the index already holds every position of the member,
    // so the slice copy, the tail seeding and the per-slice finder disappear.
    let want = near_window_max().min(dict_size);
    let tail_ctx: Vec<u8> = if index.is_some() {
        Vec::new()
    } else if s0 >= want {
        data[s0 - want..s0].to_vec()
    } else {
        let need = want - s0;
        let take = need.min(seed_tail.len());
        let st = seed_tail.len();
        let mut v = Vec::with_capacity(take + s0);
        v.extend_from_slice(&seed_tail[st - take..]);
        v.extend_from_slice(&data[..s0]);
        v
    };

    // The worker state is reused across waves; each slice is a fresh
    // frame (tail context as lookbehind, empty repeat-distance cache — a
    // documented divergence from the sequential path — plus the `chain_parts`
    // ring arrays recycled into the low-step finder). The shared long-range
    // table is queried with this slice's absolute anchor but never extended
    // here.
    state.tail = tail_ctx;
    state.dist_cache = [0u32; DIST_CACHE_SIZE];
    state.last_length = 0;
    state.combined_len = 0;
    // Seeding the tail into the fresh finder costs one hash plus a ring
    // link write per position — cheap, but the random probe skips it when
    // the tail has no repeated windows anyway (no match into it would be
    // found), so the parse inserts the slice itself and queries the shared
    // long-range table instead.
    let seed_tail = if force_seed_tail || std::env::var("RAR_RS_FORCE_SEED").is_ok() {
        true
    } else {
        !mt_tail_is_incompressible(&state.tail)
    };
    // Low-step parse (MT-only): hash-chain greedy+lazy over the same
    // combined tail+slice frame the optimal parse would build. The
    // shared long-range table stays read-only, queried at the slice's
    // absolute anchor — the near matches come from the chain, the far
    // ones from the sampled history. Cuts the per-position step count
    // (a bounded chain walk + lazy skip instead of a tree descent at
    // every position) at the price of MT output divergence.
    // Issue 15 step ③: the shared row index, not the greedy chain. Every MT
    // caller passes one; the chain tier below is the fallback for callers that
    // do not (the sequential-quality probe and tests).
    // ADR 0012 phase 1: parse only. The repeat-cache adjudication that turns
    // these tokens into `Symbol::CacheRef`/`Match` runs later, serially, over
    // every slice in order, so a slice's distances may refer to repeats the
    // previous slice established. Measured, that adjudication is worth ~nothing
    // here (5 B on ntoskrnl) because our tokens carry absolute distances and
    // the cache is derived from them, not the reverse; the mode stays because
    // it is the shape the official encoder uses and the hook a future
    // distance-choosing pass needs.
    let two_phase = slice_two_phase();
    let mut tokens: Vec<(u32, u32)> = Vec::new();
    let mut symbols = match (index, prev4, two_phase) {
        (Some(index), _, true) => mt_slice_symbols_row_index(
            state,
            slice,
            index,
            depth,
            reach,
            walk_lazy,
            walk,
            Some(&mut tokens),
        ),
        (Some(index), _, false) => {
            mt_slice_symbols_row_index(state, slice, index, depth, reach, walk_lazy, walk, None)
        }
        (None, Some(prev4), false) => {
            mt_slice_symbols_prev4(state, slice, prev4, depth, reach, walk_lazy)
        }
        (None, _, _) => mt_slice_symbols_low_step(
            state,
            slice,
            chain_len,
            lazy_thresh,
            seed_tail,
            depth,
            walk,
            walk_lazy,
        ),
    };
    if two_phase {
        // The records belong to the member, not the slice: phase 2 emits each
        // slice's own records ahead of that slice's stream, where they now
        // land at the position the reader has produced.
        return SliceOut::Tokens(tokens);
    }
    let mut lead = slice_records(records, s0, e0);
    if !lead.is_empty() {
        lead.append(&mut symbols);
        symbols = lead;
    }

    let mut out = Vec::new();
    let mut bs = 0usize;
    while bs < symbols.len() {
        let (be, _) = find_block_end_adaptive(&symbols, bs, emitted_block_cap());
        let is_last = is_last_block_of_member && be >= symbols.len();
        out.extend(encode_block(&symbols[bs..be], is_last, variant));
        bs = be;
    }
    SliceOut::Bytes(out)
}

/// MT-only low-step parse for one worker slice (see [`encode_chunked_mt`]):
/// the hash-chain greedy+lazy search over the slice frame instead of the
/// optimal BT4 parse. The near matches come from a fresh chain finder over
/// `state.tail + data[s0..e0]` (the tail is only inserted, never descended),
/// the far ones from the shared read-only long-range table at the slice's
/// absolute anchor — the same frame the optimal path would build, but with
/// a bounded chain walk + lazy skip per position instead of a tree descent.
///
/// This is the WinRAR-m3-flavoured search: cheap per-position steps, no
/// multi-pass pricing. MT output diverges from the sequential bytes
/// (already an accepted, documented divergence); the sequential path never
/// reaches here.
#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)] // the slice plus the chain tier's four dials
fn mt_slice_symbols_low_step(
    state: &mut EncoderState,
    slice: MtSlice<'_>,
    chain_len: usize,
    lazy_thresh: usize,
    seed_tail: bool,
    depth: usize,
    walk: bool,
    walk_lazy: u32,
) -> Vec<Symbol> {
    let MtSlice {
        data,
        s0,
        e0,
        entry_len,
        lr_shared,
        max_match,
        dict_size,
        long_range,
        variant,
    } = slice;
    let tail_ctx = &state.tail;
    let tl = tail_ctx.len();
    let mut combined = Vec::with_capacity(tl + (e0 - s0));
    combined.extend_from_slice(tail_ctx);
    combined.extend_from_slice(&data[s0..e0]);

    // WinRAR-m3 style: the low-step tier runs a hard-capped chain walk, not
    // the tree's ~log descent. A budget near the level's chain_len inherits
    // the exact failure the tree replaced (96-step chains on dense x86 are
    // slower than a 5-step descent), so the MT tier caps it well below the
    // level setting.
    const MT_LOW_STEP_CHAIN: usize = 16;
    let chain = chain_len.min(MT_LOW_STEP_CHAIN);
    // The walk wants the same frontier of candidates the row index hands it,
    // so it walks the chain to the level's depth rather than to the greedy
    // tier's hard-capped budget.
    let chain = if walk { depth.max(4) } else { chain };
    let parts = state.chain_parts.take();
    let mut finder = match parts {
        Some((head, prev, base)) => match_finder::MatchFinder::reuse(
            &combined, 2, max_match, chain, dict_size, head, prev, base,
        ),
        None => match_finder::MatchFinder::new(&combined, 2, max_match, chain, dict_size),
    };
    if seed_tail {
        for pos in 0..tl {
            finder.insert(pos);
        }
    }

    if walk {
        let mut dist_cache = [0u32; DIST_CACHE_SIZE];
        let mut last_length = 0u32;
        let mut inserted = tl;
        let symbols = {
            let finder_ref = &mut finder;
            super::parse::windowed_walk_parse(
                &combined,
                tl..combined.len(),
                &mut dist_cache,
                &mut last_length,
                super::parse::MatchLimits {
                    max_match,
                    window: dict_size,
                    variant,
                },
                walk_lazy,
                |pos, _cache, runs| {
                    while inserted < pos {
                        finder_ref.insert(inserted);
                        inserted += 1;
                    }
                    finder_ref.collect_candidates(pos, dict_size.min(pos), max_match, depth, runs);
                },
            )
        };
        state.chain_parts = Some(finder.into_parts());
        return symbols;
    }
    // Long-range candidates only beyond what the near chain covers.
    let lr_q = if long_range {
        Some((lr_shared, tl + (e0 - s0), entry_len + s0))
    } else {
        None
    };

    let mut dist_cache = [0u32; DIST_CACHE_SIZE];
    let mut last_length = 0u32;
    let symbols = find_matches_in_range(
        &combined,
        &mut finder,
        tl,
        combined.len(),
        lazy_thresh,
        &mut dist_cache,
        &mut last_length,
        max_match,
        lr_q,
    );
    state.chain_parts = Some(finder.into_parts());
    symbols
}
