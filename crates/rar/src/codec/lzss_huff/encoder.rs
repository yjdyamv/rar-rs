//! RAR5 encoder: LZSS+Huffman compression.
//!
//! Clean-room implementation for software conservation and educational
//! purposes.
//!
//! License: BSD-2-Clause

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::codec::bitstream::BitWriter;
use crate::codec::filters::apply_filter_encode;
use crate::codec::huffman::{EncodeTable, build_code_lengths_from_freqs, encode_symbol};
use crate::codec::match_finder::{self, MatchFinder};
#[cfg(feature = "parallel")]
use crate::error::RarError;
use crate::error::RarResult;
use crate::version::ArchiveVersion;

// ── Compression level parameters ───────────────────────────────────────────

// (chain_len, lazy_threshold, max_match)
const LEVEL_PARAMS: [(usize, usize, usize); 6] = [
    (0, 0, 0),         // 0: store (unused)
    (4, 0, 0x1001),    // 1: fastest
    (16, 0, 0x1001),   // 2: fast
    (96, 8, 0x1001),   // 3: normal
    (256, 8, 0x1001),  // 4: good
    (1024, 8, 0x1001), // 5: best
];

const MAX_BLOCK_SIZE: usize = 0x20000; // 128 KB (parse block cap; prices stay localised)

/// Cap for grouping parsed symbols into *emitted* blocks. The RAR5 size
/// field allows blocks up to 4 GiB, so this is purely an encoder choice:
/// on distribution-stable data (repetitive text) merging many parse blocks
/// into one emitted block amortises the per-block Huffman table definitions
/// (WinRAR writes one block per whole member there); on heterogeneous data
/// the tables stay per-parse-block because the drift check keeps the parse
/// blocks small. Only the emitted grouping is larger — the parse itself is
/// unchanged, so token choices are byte-identical to the 128 KiB cap.
const EMITTED_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Maximum length of one RAR5 filter block.
///
/// RARLAB readers (unrar/WinRAR) refuse filter regions larger than 256 KiB
/// (`0x40000`); the reference writer splits members into filter blocks of at
/// most `0x3FFFF` bytes. Same value as the `rars` project's
/// `MAX_FILTER_BLOCK_LENGTH` (MIT OR Apache-2.0).
pub const MAX_FILTER_BLOCK_LENGTH: u32 = 0x3FFFF;

/// Default input chunk size for the encoder. Processing input in bounded
/// slices keeps the symbol table (and match finder) memory proportional to
/// the chunk size instead of the whole file.
pub const DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Near-window (tail) context cap shared by the sequential and parallel
/// encoders: the hash-chain matcher only needs short distances, longer
/// ones come from the sampled long-range history.
const NEAR_WINDOW_MAX: usize = 8 * 1024 * 1024;

/// A RAR5 output filter applied to a region of the decompressed member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FilterSpec {
    /// FILTER_DELTA, FILTER_E8, FILTER_E8E9 or FILTER_ARM.
    pub filter_type: u8,
    /// Delta channel count (1-4); ignored for other filter types.
    pub channels: u8,
    /// Start offset of the filtered region within the member.
    pub block_start: u32,
    /// Length of the filtered region.
    pub block_length: u32,
}

impl FilterSpec {
    pub fn new(filter_type: u8, channels: u8, block_start: u32, block_length: u32) -> Self {
        Self {
            filter_type,
            channels,
            block_start,
            block_length,
        }
    }
}

/// Symbol representation for the match finder output.
#[derive(Clone, Debug)]
pub(crate) enum Symbol {
    Literal(u8),
    Match {
        distance: u32,
        length: u32,
    },
    CacheRef {
        index: usize,
        length: u32,
    },
    Repeat,
    Filter {
        block_start: u32,
        block_length: u32,
        filter_type: u8,
        channels: u8,
    },
}

/// Persistent encoder state for solid archive support.
///
/// Carries the lookbehind window tail, distance cache and last length
/// across files (and across chunks within a file) so consecutive
/// compressed members share one LZ window. Also carries the long-range
/// match history (WinRAR `-mcl` style sampled table over the recent
/// input) so distant repeated blocks compress across chunk boundaries.
#[derive(Default)]
pub struct EncoderState {
    tail: Vec<u8>,
    dist_cache: [u32; DIST_CACHE_SIZE],
    last_length: u32,
    /// Long-range match history; `None` for compression levels where the
    /// long range search is disabled (method 1, like WinRAR).
    long_range: Option<match_finder::LongRange>,
    /// BT4 tree finder reused across chunks of a member. Rebuilding it per
    /// chunk cost a full 32 MiB son-array memset plus page faults (and the
    /// tail re-seed walked the cold array); persisting it keeps the array
    /// warm, with links rebased by the frame slide instead of re-seeding.
    tree: Option<match_finder::TreeMatchFinder>,
    /// Length of the previous chunk's `combined` frame; the persistent
    /// finder's links are rebased by `combined_len - keep` when the frame
    /// slides.
    combined_len: usize,
}

impl EncoderState {
    /// Reset the solid chain. Call after any member that does not
    /// participate in the LZ window (directories, STORE files, empty
    /// files).
    pub fn reset(&mut self) {
        self.tail.clear();
        self.dist_cache = [0; DIST_CACHE_SIZE];
        self.last_length = 0;
        if let Some(lr) = self.long_range.as_mut() {
            lr.reset();
        }
        self.tree = None;
        self.combined_len = 0;
    }
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
#[allow(clippy::too_many_arguments)]
pub fn encode_chunked_raw(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    state: Option<&mut EncoderState>,
    is_final: bool,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
    variant: ArchiveVersion,
) -> RarResult<Vec<u8>> {
    if data.is_empty() {
        return Ok(encode_empty_block(variant));
    }

    let level = (method as usize).clamp(1, 5);
    let (chain_len, lazy_thresh, max_match) = LEVEL_PARAMS[level];
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
    let mut next_report = 0u64;

    while chunk_start < data.len() {
        let chunk_end = (chunk_start + chunk_size).min(data.len());
        let chunk = &data[chunk_start..chunk_end];
        // Levels 2-5 use the optimal (shortest-path) parse; level 1 keeps
        // the greedy+lazy matcher (it exists to be quick, like WinRAR's
        // own fastest rung).
        let symbols = if level >= 2 {
            find_matches_optimal(
                state,
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
                state,
                chunk,
                chain_len,
                lazy_thresh,
                max_match,
                dict_size,
                long_range,
            )
        };

        let mut block_start = 0usize;
        while block_start < symbols.len() {
            let (block_end, _) = find_block_end_adaptive(&symbols, block_start, EMITTED_BLOCK_SIZE);
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

    Ok(output)
}

/// Multi-threaded encoding of one contiguous window of a member (see
/// [`encode_chunked_mt_with_progress`]; this is the no-progress form).
#[cfg(not(feature = "parallel"))]
#[allow(clippy::too_many_arguments)]
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

#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)]
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
        method,
        dict_size_log,
        chunk_size,
        seed,
        threads,
        is_final,
        variant,
        None,
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
/// `lead_symbols` are prepended to the first slice's symbol stream — the
/// filter records of a filtered member, which must be read before any
/// output (their positions are member-relative).
#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_chunked_mt_with_progress(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    seed: &mut EncoderState,
    threads: usize,
    is_final: bool,
    variant: ArchiveVersion,
    lead_symbols: Option<&[Symbol]>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<Vec<u8>> {
    let level = (method as usize).clamp(1, 5);
    let (chain_len, _lazy_thresh, max_match) = LEVEL_PARAMS[level];
    let dict_size = 128 * 1024 * (1usize << dict_size_log as u32);
    let long_range = level >= 2;
    // Adaptive slice size: with a fixed 4 MiB slice a 13 MB member gives
    // only 4 slices and the pool sits mostly idle. Target ~2x the thread
    // count slices (floor 2 MiB) so medium members parallelize properly;
    // members whose caller chunk already yields plenty of slices (big
    // members, repetitive text) keep the caller's size unchanged.
    let cs = {
        let caller = chunk_size.max(1);
        let by_count = data.len() / (threads.max(1) * 2);
        let floored = by_count.max(caller.min(2 * 1024 * 1024));
        caller.min(floored).max(1)
    };

    // Shared long-range table over entry history plus this window; built
    // once, read-only afterwards (workers query with absolute anchors).
    let entry_hist = seed
        .long_range
        .as_ref()
        .map(|l| l.hist_bytes().to_vec())
        .unwrap_or_default();
    let mut lr_shared = match_finder::LongRange::new(dict_size);
    lr_shared.push(&entry_hist);
    lr_shared.push(data);
    let entry_len = entry_hist.len();
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

    let pool = crate::parallel::compression_pool();
    // Rayon's Scope::spawn returns nothing; each worker deposits its
    // packed bytes into its own slot and we collect them wave-by-wave in
    // order after the scope joins everything.
    // One persistent encoder state per worker, reused across waves: the
    // BT4 tree's son array stays warm (a fresh 16 MiB allocation per slice
    // cost page faults and cold-tree parse time — 6x slower per byte than
    // the sequential path's rebased tree). Waves run sequentially so each
    // state is used by exactly one thread at a time.
    let mut worker_states: Vec<EncoderState> =
        (0..n_workers).map(|_| EncoderState::default()).collect();
    let mut output = Vec::new();
    let mut first = 0usize;
    while first < n {
        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        let last = (first + n_workers).min(n);
        let wave = &mut worker_states[..last - first];
        let results = std::sync::Mutex::new(vec![None::<Vec<u8>>; last - first]);
        {
            // Shared read-only handles for the worker closures.
            let tail_ref = &seed_tail;
            let lr_ref = &lr_shared;
            let results_ref = &results;
            pool.scope(|scope| {
                for (i, state) in wave.iter_mut().enumerate() {
                    let k = first + i;
                    let (s0, e0) = (bounds[k], bounds[k + 1]);
                    scope.spawn(move |_| {
                        let blocks = encode_mt_slice(
                            data,
                            s0,
                            e0,
                            tail_ref,
                            lr_ref,
                            entry_len,
                            chain_len,
                            max_match,
                            dict_size,
                            long_range,
                            OPTIMAL_PARSE_PASSES[level],
                            is_final && k + 1 == n,
                            variant,
                            (k == 0).then_some(lead_symbols).flatten(),
                            state,
                        );
                        results_ref.lock().unwrap()[i] = Some(blocks);
                    });
                }
            });
        }
        for r in results.into_inner().unwrap() {
            output.extend(r.expect("worker slot filled"));
        }
        if let Some(cb) = progress.as_deref_mut()
            && bounds[last] > bounds[first]
        {
            cb(bounds[last] as u64, data.len() as u64);
        }
        first = last;
    }

    // Continue the chain after this window: tail = suffix of the window
    // (seeded with the entry tail so it spans windows like sequential
    // mode), long-range state swaps in the shared table, repeat-distance
    // cache resets (documented divergence from the sequential path).
    let keep = dict_size.min(NEAR_WINDOW_MAX);
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
    seed.long_range = Some(lr_shared);
    Ok(output)
}

/// Cheap per-slice probe: would seeding this tail ever pay off? Samples
/// 4-byte windows every [`MT_SEED_PROBE_STRIDE`] bytes over the tail's
/// head. A tail whose sampled windows are (almost) all distinct has no
/// long repeats, so the fresh-tree seeding of it is wasted work — random
/// media, compressed/encrypted data — while text, code and structured
/// binary keep their repeated windows and seed normally.
///
/// The 4-byte windows are compared raw (no hash), so the distinct count
/// is exact: random input measures ~100%, any input with real repeats
/// (including base64/hex text, whose alphabet still cycles within a
/// window) stays well below the threshold.
#[cfg(feature = "parallel")]
fn mt_tail_is_incompressible(tail: &[u8]) -> bool {
    const STRIDE: usize = 16;
    const PROBE_LEN: usize = 256 * 1024;
    const MIN_WINDOWS: usize = 4096; // 64 KiB of sampled windows
    const DISTINCT_PERCENT: usize = 95;
    let probe = &tail[..tail.len().min(PROBE_LEN)];
    let mut seen = std::collections::HashSet::with_capacity(probe.len() / STRIDE + 1);
    let mut windows = 0usize;
    let mut off = 0usize;
    while off + 4 <= probe.len() {
        let v = u32::from_le_bytes([probe[off], probe[off + 1], probe[off + 2], probe[off + 3]]);
        seen.insert(v);
        windows += 1;
        off += STRIDE;
    }
    windows >= MIN_WINDOWS && seen.len() * 100 >= windows * DISTINCT_PERCENT
}

/// Encode one worker slice `[s0, e0)` of [`encode_chunked_mt`].
///
/// Each worker runs the same optimal parse as the sequential path (per-slice
/// tree over its tail context, shared read-only long-range table), so the
/// multi-threaded output matches the sequential parse quality instead of the
/// old greedy+lazy fallback.
#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)]
fn encode_mt_slice(
    data: &[u8],
    s0: usize,
    e0: usize,
    seed_tail: &[u8],
    lr_shared: &match_finder::LongRange,
    entry_len: usize,
    chain_len: usize,
    max_match: usize,
    dict_size: usize,
    long_range: bool,
    passes: usize,
    is_last_block_of_member: bool,
    variant: ArchiveVersion,
    // Filter records of a filtered member, prepended to the first slice's
    // symbol stream so they precede all output (member-relative positions).
    lead_symbols: Option<&[Symbol]>,
    state: &mut EncoderState,
) -> Vec<u8> {
    // Near-window context: the closest bytes before this slice, seeded
    // with the entry tail when the slice starts at the buffer head.
    // Multi-threaded workers seed a fresh tree per slice, so the near
    // window is capped well below the sequential path: distant matches
    // come from the shared long-range table, and a shorter seed keeps
    // the per-slice tree warm (the sequential path persists its tree and
    // keeps the full 8 MiB window without re-seeding).
    let want = (2 * 1024 * 1024).min(dict_size);
    let tail_ctx: Vec<u8> = if s0 >= want {
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
    // documented divergence from the sequential path — and the tree
    // re-armed via `combined_len = 0` so `find_matches_optimal` clears
    // the head and seeds the new tail). The shared long-range table is
    // queried with this slice's absolute anchor but never extended here.
    state.tail = tail_ctx;
    state.dist_cache = [0u32; DIST_CACHE_SIZE];
    state.last_length = 0;
    state.combined_len = 0;
    // Seeding a fresh tree over the tail costs a random-access descent per
    // position into the multi-MiB son array (hundreds of ms on random
    // data, tens on text). Skip it when the tail probes incompressible:
    // no match into it would be found anyway, and the parse's own
    // insertions plus the shared long-range table cover everything else.
    let seed_tail = !mt_tail_is_incompressible(&state.tail);
    let mut symbols = find_matches_optimal(
        state,
        &data[s0..e0],
        chain_len,
        0,
        max_match,
        dict_size,
        long_range,
        long_range.then_some(lr_shared),
        entry_len + s0,
        variant,
        passes,
        seed_tail,
    );
    if let Some(lead) = lead_symbols {
        let mut joined = lead.to_vec();
        joined.append(&mut symbols);
        symbols = joined;
    }

    let mut out = Vec::new();
    let mut bs = 0usize;
    while bs < symbols.len() {
        let (be, _) = find_block_end_adaptive(&symbols, bs, EMITTED_BLOCK_SIZE);
        let is_last = is_last_block_of_member && be >= symbols.len();
        out.extend(encode_block(&symbols[bs..be], is_last, variant));
        bs = be;
    }
    out
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
pub fn encode_with_filters(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    filters: &[FilterSpec],
    variant: ArchiveVersion,
) -> RarResult<Vec<u8>> {
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
            let (block_end, _) = find_block_end(&symbols, block_start, MAX_BLOCK_SIZE);
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

/// Merge overlapping or adjacent ranges (the x86 scan can return a broad
/// span plus tighter clusters inside it; overlapping filter records would
/// double-transform the overlap).
fn merge_ranges(ranges: &mut Vec<std::ops::Range<usize>>) {
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
    let mut ranges_e9 = crate::codec::filters::auto_x86_filter_ranges(data, true);
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

    let mut ranges_e8 = crate::codec::filters::auto_x86_filter_ranges(data, false);
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
    let sample_len = data.len().min(1 << 16);
    let sample = &data[..sample_len];
    let plain = encode_with_filters(sample, method, dict_size_log, &[], variant)?;
    let mut best: Option<(u8, usize)> = None;
    for &ch in crate::codec::filters::AUTO_DELTA_CHANNELS {
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
/// [`filters::auto_delta_filter_channels`] gate passes), the best channel
/// count is chosen by compressed size on a leading sample and the whole member
/// is forward-transformed and packed as a standalone (non-solid) filter
/// member — but only when it strictly beats plain LZSS. Size-based channel
/// selection is what WinRAR does and is robust to byte-wrapping at sample
/// boundaries (a raw magnitude heuristic is fooled into picking a wider channel
/// by the large deltas that wrapping introduces), and the plain-LZSS
/// comparison guarantees structured-but-not-multi-channel data (text, prose)
/// is never made worse than the unfiltered pack.
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
    if crate::codec::filters::auto_delta_filter_channels(data).is_none() {
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

// ── Match finding ──────────────────────────────────────────────────────────

/// Find matches for `chunk`, searching against `state.tail` as lookbehind.
/// Advances `state` so a following chunk/file continues the LZ window.
/// When `long_range` is set, distances beyond the near window are found
/// through the sampled long-range history (WinRAR `-mcl` semantics).
fn find_matches_with_tail(
    state: &mut EncoderState,
    chunk: &[u8],
    chain_len: usize,
    lazy_thresh: usize,
    max_match: usize,
    window: usize,
    long_range: bool,
) -> Vec<Symbol> {
    let tail_len = state.tail.len();
    let mut combined = Vec::with_capacity(tail_len + chunk.len());
    combined.extend_from_slice(&state.tail);
    combined.extend_from_slice(chunk);

    let mut finder = MatchFinder::new(&combined, 2, max_match, chain_len, window);
    for pos in 0..tail_len {
        finder.insert(pos);
    }

    // Borrow the long-range state read-only for the search; the history
    // is only updated (mutably) after the symbol stream is produced.
    let lr = if long_range {
        let lr = state
            .long_range
            .get_or_insert_with(|| match_finder::LongRange::new(window));
        // The near finder covers distances up to tail + chunk; long-range
        // candidates only matter beyond that.
        let near_max = tail_len + chunk.len();
        Some((&*lr, near_max, lr.total_pushed()))
    } else {
        None
    };

    let mut dist_cache = state.dist_cache;
    let mut last_length = state.last_length;
    let symbols = find_matches_in_range(
        &combined,
        &mut finder,
        tail_len,
        combined.len(),
        lazy_thresh,
        &mut dist_cache,
        &mut last_length,
        max_match,
        lr,
    );

    if long_range && let Some(lr) = state.long_range.as_mut() {
        lr.push(chunk);
    }

    // The near window (tail) only needs to cover short-distance matches:
    // longer distances come from the sampled long-range history. Capping
    // the tail keeps the per-chunk rebuild cost (inserting the whole
    // tail into the hash chain) bounded instead of O(window) per chunk.
    let keep = window.min(NEAR_WINDOW_MAX).min(combined.len());
    state.tail = combined[combined.len() - keep..].to_vec();
    state.dist_cache = dist_cache;
    state.last_length = last_length;
    symbols
}

/// Match-finding loop over `data[start..end]` with a distance cache.
/// `lr` (when present) adds long-range candidates from the sampled
/// history: `(long_range, near_max)` where `near_max` is the largest
/// distance the near finder can produce (tail + chunk), so long-range
/// hits are only considered beyond it.
#[allow(clippy::too_many_arguments)]
fn find_matches_in_range(
    data: &[u8],
    finder: &mut MatchFinder<'_>,
    start: usize,
    end: usize,
    lazy_thresh: usize,
    dist_cache: &mut [u32; DIST_CACHE_SIZE],
    last_length: &mut u32,
    max_match: usize,
    // Long-range probe: (table, near_max, anchor). Anchor is the absolute
    // stream position of data[start]; sequential callers pass the history total
    // total pushed length, parallel workers pass their slice's absolute start.
    lr: Option<(&match_finder::LongRange, usize, usize)>,
) -> Vec<Symbol> {
    let mut symbols = Vec::with_capacity(end - start);
    let mut pos = start;

    // After this many consecutive non-matching positions the finder
    // switches to fast mode: every position is still inserted into the
    // hash chain (the window stays complete for later matches), the
    // literal is emitted directly, and only the sampled long-range table
    // is probed (on its 16-byte grid). Incompressible runs (random data,
    // media) then cost one hash insertion per byte instead of a full
    // match attempt with its cache-missing random accesses, while the
    // distant repeats that justify the compression pass are still found.
    let mut no_match_run = 0usize;
    let mut fast = false;

    while pos < end {
        let (mut dist, mut length) = if fast {
            finder.insert(pos);
            let mut d = 0usize;
            let mut l = 0usize;
            // Periodic recovery: a full search every FAST_RECOVER_INTERVAL
            // positions even in fast mode.
            if (pos & (FAST_RECOVER_INTERVAL - 1)) == 0 {
                (d, l) = finder.find_match_cached(pos, dist_cache);
            }
            if let Some((long_range, near_max, anchor)) = lr
                && pos + 4 <= end
                && (pos & (match_finder::LONG_RANGE_STEP - 1)) == 0
            {
                let chunk_off = pos - start;
                if let Some((ld, ll)) = long_range.find_from(
                    &data[start..end],
                    chunk_off,
                    anchor,
                    near_max + 1,
                    max_match,
                ) && ll > l
                {
                    d = ld as usize;
                    l = ll;
                }
            }
            (d, l)
        } else {
            let (mut d, mut l) = finder.find_match_cached(pos, dist_cache);

            // Long-range candidate: only when the near window found
            // nothing useful (a good near match is never worse than a
            // far one).
            if let Some((long_range, near_max, anchor)) = lr
                && l < 64
                && pos + 4 <= end
            {
                let chunk_off = pos - start;
                if let Some((ld, ll)) = long_range.find_from(
                    &data[start..end],
                    chunk_off,
                    anchor,
                    near_max + 1,
                    max_match,
                ) && ll > l
                {
                    d = ld as usize;
                    l = ll;
                }
            }
            (d, l)
        };

        if dist > 0 && lazy_thresh > 0 && length < lazy_thresh && pos + 1 < end {
            let (dist2, length2) = finder.find_match_cached(pos + 1, dist_cache);
            if length2 > length {
                symbols.push(Symbol::Literal(data[pos]));
                *last_length = 0;
                pos += 1;
                dist = dist2;
                length = length2;
            }
        }

        if dist > 0 {
            if fast {
                // A match (long-range) resumes full matching.
                fast = false;
                no_match_run = 0;
            }
            let cache_idx = cache_find(dist_cache, dist as u32);
            if let Some(idx) = cache_idx {
                if idx == 0 && length as u32 == *last_length && *last_length > 0 {
                    symbols.push(Symbol::Repeat);
                } else {
                    symbols.push(Symbol::CacheRef {
                        index: idx,
                        length: length as u32,
                    });
                    *last_length = length as u32;
                }
                cache_touch(dist_cache, idx);
            } else {
                let raw_length = remove_length_bonus(length as u32, dist as u32);
                if raw_length >= 2 {
                    symbols.push(Symbol::Match {
                        distance: dist as u32,
                        length: raw_length,
                    });
                    cache_push(dist_cache, dist as u32);
                    *last_length = apply_length_bonus(raw_length, dist as u32);
                } else {
                    for i in 0..length {
                        symbols.push(Symbol::Literal(data[pos + i]));
                        finder.insert(pos + i);
                    }
                    *last_length = 0;
                    pos += length;
                    continue;
                }
            }

            for i in 1..length {
                finder.insert(pos + i);
            }
            pos += length;
        } else {
            symbols.push(Symbol::Literal(data[pos]));
            *last_length = 0;
            no_match_run += 1;
            if !fast && no_match_run >= FAST_MODE_AFTER {
                fast = true;
            }
            pos += 1;
        }
    }

    symbols
}

// ── Optimal parse (compression levels 2-5) ──────────────────────────────────
//
// A forward shortest-path parse ported from the `rars` project (MIT OR
// Apache-2.0) `codec/rar50.rs` (`optimal_tokens` + `TokenPrices`). The
// greedy+lazy matcher looks one symbol ahead; this prices every path
// through a block and keeps the cheapest, which is where WinRAR's m2-m5
// ratio advantage comes from. Each node carries the whole four-slot
// distance memory the cheapest path to it leaves behind, so the next hop
// is priced against what that path would really have remembered (two paths
// reaching one node with different memories still collapse into whichever
// was cheaper — an approximation, but far closer than lazy matching).

/// Longest match the optimal parse commits to and steps over without
/// pricing the bytes it covers (rars `NICE_MATCH_LENGTH`).
pub(crate) const NICE_MATCH_LENGTH: usize = 64;

/// After this many consecutive positions with no match found, the optimal
/// parse's match collection stops probing the long-range table on every
/// position and drops to a [`FAST_RECOVER_INTERVAL`] cadence (incompressible
/// runs then cost the tree search instead of a cache-missing probe per byte;
/// the lazy matcher fast mode does the same).
const FAST_MODE_AFTER: usize = 64 * 1024;

/// Full-search cadence inside fast mode (power of two): every this many
/// literal positions a real long-range probe runs, so the mode recovers
/// when compressible data returns (without it the first 64 KiB
/// incompressible run would lock the probe off for the whole member).
const FAST_RECOVER_INTERVAL: usize = 128;

/// Consecutive failed tree probes before the block collector's tree walk
/// drops to the [`FAST_RECOVER_INTERVAL`] cadence. A probe fails only when
/// the tree found *no match at all* (not merely a short one): on
/// text-like data 4-15 byte matches are real signal (word prefixes) and
/// must keep the full search cadence — gating on them would let fast mode
/// starve the recovery searches of candidates and measurably worsen the
/// ratio on text — while on truly incompressible data a 4-byte
/// hash-collision match is ~2^-32 per position, so the miss run still
/// accumulates and the mode engages after a couple of KiB of wasted
/// cache-missing descents into the multi-MiB son array.
const COLLECT_TREE_MISS_THRESHOLD: usize = 256;

/// Same gating for the long-range probe: `COLLECT_TREE_MISS_THRESHOLD`
/// failed probes into the multi-MiB random-access table drops it to the
/// recovery cadence. A spurious short tree match must not reset this — only
/// an actual long-range hit pays for the probe.
const COLLECT_LR_MISS_THRESHOLD: usize = 256;

/// Test seam: force the full pricing passes even for matchless blocks, to
/// prove the matchless fast path is byte-identical.
static DISABLE_MATCHLESS_FAST_PATH: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
pub(crate) fn set_fast_path_enabled(enabled: bool) {
    DISABLE_MATCHLESS_FAST_PATH.store(!enabled, Ordering::Relaxed);
}
/// Estimated cost of a literal before any block has been priced, in the
/// same bit units as the match cost estimates (a main-table symbol out of
/// 256 plus the odds that the table is skewed).
const ESTIMATED_LITERAL_COST: u32 = 9;

/// How many length-slot prices the optimal parse computes per position, at
/// most. A run spans every length slot between its endpoints, and the parse
/// prices each slot's endpoint (longer in the same slot is always strictly
/// better, so the slot ends are the only lengths worth a look); a position in
/// repetitive data can span a dozen slots across its runs. The cheapest path
/// through a position only ever relaxes a handful of targets, so stepping
/// the slot loop is the parse's hot inner step and the whole pricing pass
/// stops after this many.
const MAX_PARSE_STEPS_PER_POSITION: usize = 12;

/// What a symbol the first pass never used is assumed to cost. Reaching for
/// one is not forbidden, only expensive: the tables are rebuilt from
/// whatever the last pass chose, so a symbol that earns its place gets a
/// real code.
const UNUSED_SYMBOL_COST: usize = 15;

/// How many times the optimal parse runs over a block. The first pass
/// guesses prices; the rest reprice against the tables the pass before
/// produced. Fewer passes is proportionally cheaper, so the ladder
/// trades ratio for speed here: m2/m3 (fast/normal) do one reprice,
/// m4 two and m5 three.
const OPTIMAL_PARSE_PASSES: [usize; 6] = [0, 0, 2, 2, 3, 4];

/// Base parse-block size; blocks extend up to [`MAX_BLOCK_SIZE`] while the
/// byte distribution stays stable (see [`BlockSplitter`]).
const OPT_BLOCK_SIZE: usize = 64 * 1024;

/// The encoder's distance memory, mirroring the decoder's four-slot cache
/// and last-length state exactly. `remember` matches the decoder's cache
/// transitions so a token's cost and its validity are priced against the
/// same state the decoder will hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EncoderMatchState {
    reps: [u32; DIST_CACHE_SIZE],
    last_length: u32,
}

impl EncoderMatchState {
    fn new(reps: [u32; DIST_CACHE_SIZE], last_length: u32) -> Self {
        Self { reps, last_length }
    }

    /// Classify a match the way the encoder will emit it. `None` when the
    /// match cannot be encoded: a fresh distance's length bonus can exceed
    /// the raw length (the format's minimum match length is 2), and the
    /// optimal parser must not price a token the writer cannot emit.
    fn encode_match(
        &self,
        length: u32,
        distance: u32,
        variant: ArchiveVersion,
    ) -> Option<EncodedMatch> {
        if distance == self.reps[0] && length == self.last_length && self.last_length != 0 {
            return Some(EncodedMatch::Repeat);
        }
        if let Some(index) = self.reps.iter().position(|&d| d == distance && d != 0) {
            let len_slot = encode_length_slot(length);
            return Some(EncodedMatch::CacheRef {
                index,
                len_slot,
                len_extra: length_extra_bits(length, len_slot),
            });
        }
        let raw_length = length.checked_sub(length_bonus(distance))?;
        if raw_length < 2 {
            return None;
        }
        let len_slot = encode_length_slot(raw_length);
        let (dist_slot, dist_extra, dbits) = encode_distance_slot(distance, variant);
        Some(EncodedMatch::New {
            len_slot,
            len_extra: length_extra_bits(raw_length, len_slot),
            dist_slot,
            dist_extra,
            dbits,
        })
    }

    /// Advance the distance memory for an emitted match, mirroring the
    /// decoder's cache transitions.
    fn remember(&mut self, length: u32, distance: u32) {
        if distance == self.reps[0] && length == self.last_length {
            return;
        }
        if let Some(index) = self.reps.iter().position(|&d| d == distance) {
            self.reps[..=index].rotate_right(1);
        } else {
            self.reps.rotate_right(1);
        }
        self.reps[0] = distance;
        self.last_length = length;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncodedMatch {
    Repeat,
    CacheRef {
        index: usize,
        len_slot: usize,
        len_extra: usize,
    },
    New {
        len_slot: usize,
        len_extra: usize,
        dist_slot: usize,
        dist_extra: u32,
        dbits: usize,
    },
}

/// The length bonus added at decode time for a match at `distance`.
fn length_bonus(distance: u32) -> u32 {
    u32::from(distance > 0x100) + u32::from(distance > 0x2000) + u32::from(distance > 0x40000)
}

/// Extra bits written after a length slot (0 for slots below 8).
fn length_extra_bits(_length: u32, slot: usize) -> usize {
    if slot < 8 { 0 } else { slot / 4 - 1 }
}

/// Estimated bit cost of a match before any block has been priced (rars
/// `estimated_match_cost`). `None` when the match cannot be encoded.
fn estimated_match_cost(
    state: &EncoderMatchState,
    length: u32,
    distance: u32,
    variant: ArchiveVersion,
) -> Option<usize> {
    match state.encode_match(length, distance, variant)? {
        EncodedMatch::Repeat => Some(2),
        EncodedMatch::CacheRef { len_slot, .. } => Some(5 + length_extra_bits(length, len_slot)),
        EncodedMatch::New {
            len_slot, dbits, ..
        } => {
            let raw = length - length_bonus(distance);
            Some(10 + length_extra_bits(raw, len_slot) + dbits)
        }
    }
}

/// The Huffman code lengths a block of symbols produces, as the block
/// writer computes them (same frequency counting, same `ensure_nonzero`,
/// same length-limit pass). The optimal parse needs these to know what each
/// token it is considering will actually cost.
struct TokenPrices<'a> {
    nc: &'a [u8],
    dc: &'a [u8],
    ldc: &'a [u8],
    rc: &'a [u8],
}

impl TokenPrices<'_> {
    fn code(bits: u8) -> usize {
        if bits == 0 {
            UNUSED_SYMBOL_COST
        } else {
            usize::from(bits)
        }
    }

    fn literal(&self, byte: u8) -> usize {
        Self::code(self.nc[byte as usize])
    }

    fn match_cost(
        &self,
        state: &EncoderMatchState,
        length: u32,
        distance: u32,
        variant: ArchiveVersion,
    ) -> Option<usize> {
        match state.encode_match(length, distance, variant)? {
            EncodedMatch::Repeat => Some(Self::code(self.nc[SYM_REPEAT])),
            EncodedMatch::CacheRef {
                index,
                len_slot,
                len_extra,
            } => Some(
                Self::code(self.nc[SYM_CACHE_BASE + index])
                    + Self::code(self.rc[len_slot])
                    + len_extra,
            ),
            EncodedMatch::New {
                len_slot,
                len_extra,
                dist_slot,
                dist_extra,
                dbits,
            } => {
                let distance_bits = if dbits >= 4 {
                    dbits - 4 + Self::code(self.ldc[(dist_extra & 0xF) as usize])
                } else {
                    dbits
                };
                Some(
                    Self::code(self.nc[SYM_MATCH_BASE + len_slot])
                        + len_extra
                        + Self::code(self.dc[dist_slot])
                        + distance_bits,
                )
            }
        }
    }
}

/// One match finder result list for a block: every position's runs, one
/// position after another. Each run is `(length, distance)`; the sequence
/// per position has strictly increasing lengths, and the first distance to
/// reach a length is the cheapest one that can (nearest-first chains).
struct BlockMatches {
    runs: Vec<(u32, u32)>,
    starts: Vec<u32>,
}

impl BlockMatches {
    fn at(&self, index: usize) -> &[(u32, u32)] {
        &self.runs[self.starts[index] as usize..self.starts[index + 1] as usize]
    }
}

/// Decides where one parse block ends, from the raw bytes alone. Blocks
/// grow over data whose byte distribution is not moving (rars
/// `BlockSplitter`).
struct BlockSplitter {
    counts: [u32; 256],
    total: u64,
}

impl BlockSplitter {
    const DRIFT_DIVISOR: u64 = 128;

    fn new() -> Self {
        Self {
            counts: [0; 256],
            total: 0,
        }
    }

    fn accept(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            self.counts[usize::from(byte)] += 1;
        }
        self.total += chunk.len() as u64;
    }

    /// Whether the open block should swallow `chunk` rather than end before
    /// it. Integer arithmetic throughout, so a block boundary can never
    /// depend on platform float behaviour.
    fn extends(&self, chunk: &[u8]) -> bool {
        let open = self.total;
        if open == 0 || chunk.is_empty() {
            return false;
        }
        if open + chunk.len() as u64 > MAX_BLOCK_SIZE as u64 {
            return false;
        }
        let mut counts = [0u32; 256];
        for &byte in chunk {
            counts[usize::from(byte)] += 1;
        }
        let chunk_len = chunk.len() as u64;
        let mut misplaced = 0u64;
        for (theirs, ours) in counts.iter().zip(&self.counts) {
            misplaced += (u64::from(*theirs) * open).abs_diff(u64::from(*ours) * chunk_len);
        }
        misplaced / open <= chunk_len / Self::DRIFT_DIVISOR
    }
}

/// Collect the matches the optimal parse will price at each position of a
/// block, taking the positions into the shared tree finder as it goes
/// (blocks must arrive in order, each exactly once).
///
/// Long-range candidates beyond the near window are folded in here: they
/// do not depend on the prices, so collecting them once lets every parse
/// pass replay the same answers. `lr` is `(table, near_max, anchor)` as in
/// the lazy path, with the LR query slice being the chunk part of
/// `combined` (which starts at `tail_len`); the probe only runs where the
/// tree found nothing useful (a good near match is never worse than a
/// far one).
#[allow(clippy::too_many_arguments)]
fn collect_block_matches(
    finder: &mut match_finder::TreeMatchFinder,
    combined: &[u8],
    block: std::ops::Range<usize>,
    tail_len: usize,
    chain_len: usize,
    max_match: usize,
    window: usize,
    lr: Option<(&match_finder::LongRange, usize, usize)>,
) -> BlockMatches {
    let span = block.end - block.start;
    let mut matches = BlockMatches {
        runs: Vec::with_capacity(span),
        starts: Vec::with_capacity(span + 1),
    };
    let mut committed_through = block.start;
    // Scratch for the tree finder's per-position reports.
    let mut scratch: Vec<(u32, u32)> = Vec::new();
    let mut lr_fast = false;
    let mut lr_misses = 0usize;
    let mut tree_misses = 0usize;
    let mut fast_tree = false;
    for pos in block.clone() {
        matches.starts.push(matches.runs.len() as u32);
        let searching = pos >= committed_through;
        let max_distance = pos.min(window);
        let max_length = (block.end - pos).min(max_match);
        let before = matches.runs.len();
        // Inserting into a tree is the same descent as searching it, so a
        // position the parse steps over is stepped over here too rather
        // than inserted for nothing (its bytes are a copy of what the
        // match already points at, so the tree loses little by not holding
        // them).
        //
        // Fast mode: once the tree has found nothing for a long run of
        // positions (incompressible data), the descent stops paying — it
        // walks links through a multi-MiB son array that every probe
        // misses in. Skip the search (and with it the insertion) except
        // for a full recovery search every FAST_RECOVER_INTERVAL
        // positions, and resume when any real match (>= 16 bytes, past
        // spurious 4-byte hash coincidences) shows up.
        if searching
            && max_distance > 0
            && max_length >= 4
            && pos + 3 < combined.len()
            && (!fast_tree || (pos & (FAST_RECOVER_INTERVAL - 1)) == 0)
        {
            let avail = combined.len() - pos;
            let len_limit = avail.min(NICE_MATCH_LENGTH);
            scratch.clear();
            finder.matches(
                combined,
                pos,
                len_limit,
                max_distance,
                chain_len,
                &mut scratch,
            );
            // The tree's internal ordering invariants can break when it is
            // reused across chunks (budget-limited descents against a dense
            // persistent tree — the DLL reproduction hit this: a bogus
            // match copied the MZ header over real code, and the corrupt
            // member was silently written). Verify every report byte-exactly
            // before the parse can price it; a report whose bytes do not
            // actually match is dropped, a short over-report is truncated to
            // the true length. Cheap: the descent already compared these
            // bytes, and matches are sparse relative to positions.
            let mut w = 0usize;
            for r in 0..scratch.len() {
                let (_len, dist) = scratch[r];
                let actual = match_length_at(combined, pos, dist as usize, len_limit);
                if actual >= 4 {
                    scratch[w] = (actual as u32, dist);
                    w += 1;
                }
            }
            scratch.truncate(w);
            matches.runs.extend(scratch.iter().copied());
            // Measure the last report out to its real end: the tree
            // stops comparing at the limit, and a match reaching it
            // is what the parse commits to and steps over.
            if let Some(&(length, distance)) = scratch.last()
                && length as usize == len_limit
                && len_limit < avail.min(max_match)
            {
                let full = match_length_at(combined, pos, distance as usize, avail.min(max_match));
                if let Some(last) = matches.runs.last_mut() {
                    last.0 = full as u32;
                }
            }
        }
        let mut longest = matches.runs[before..]
            .iter()
            .map(|&(len, _)| len as usize)
            .max()
            .unwrap_or(0);
        // Fast mode gates on the tree finding *nothing at all* (`longest
        // == 0`), not on a short match: on text-like data 4-15 byte
        // matches are real signal (word prefixes) and must keep the full
        // search cadence, while on truly incompressible data a 4-byte
        // hash-collision match is ~2^-32 per position, so the miss run
        // still accumulates and the mode engages as quickly as ever.
        if longest == 0 {
            tree_misses += 1;
            if !fast_tree && tree_misses >= COLLECT_TREE_MISS_THRESHOLD {
                fast_tree = true;
            }
        } else {
            tree_misses = 0;
            fast_tree = false;
        }
        // Long-range probe gating: the probe misses in a multi-MiB
        // random-access table, so once it has failed for a long run of
        // positions (incompressible data) it drops to the
        // FAST_RECOVER_INTERVAL cadence. Any hit resumes full probing —
        // a spurious short tree match must not reset this, only an actual
        // long-range hit pays for the probe.
        if let Some((long_range, near_max, anchor)) = lr
            && searching
            && longest < 64
            && pos + 4 <= combined.len()
            && (!lr_fast || (pos & (FAST_RECOVER_INTERVAL - 1)) == 0)
        {
            let chunk_off = pos - tail_len;
            let before = matches.runs.len();
            if let Some((ld, ll)) = long_range.find_from(
                &combined[tail_len..],
                chunk_off,
                anchor,
                near_max + 1,
                max_length,
            ) && ll > longest
            {
                matches.runs.push((ll as u32, ld));
                longest = ll;
            }
            if matches.runs.len() > before {
                lr_fast = false;
                lr_misses = 0;
            } else {
                lr_misses += 1;
                // A 64 KiB parse block holds 64 K positions, so a
                // 64 K-probe threshold would only fire at the last
                // position of the block and never pay off; a few hundred
                // failed probes (a couple of KiB of incompressible data)
                // is already definitive and leaves room to act within
                // the block.
                if !lr_fast && lr_misses >= COLLECT_LR_MISS_THRESHOLD {
                    lr_fast = true;
                }
            }
        }
        // The parse can only take a match the block still has room for, so
        // the reach it will commit to is measured the way it measures it.
        let reach = longest.min(block.end - pos).min(max_match);
        if reach >= NICE_MATCH_LENGTH {
            committed_through = pos + reach;
        }
    }
    matches.starts.push(matches.runs.len() as u32);
    matches
}

/// The longest match at `distance` that costs exactly what a match of
/// `length` costs. Only the length slot varies with length, and a slot
/// covers a run of consecutive lengths, so the end of that run is the last
/// length worth pricing (rars `same_price_run_end`).
fn same_price_run_end(
    state: &EncoderMatchState,
    length: u32,
    distance: u32,
    variant: ArchiveVersion,
    max_match: usize,
) -> u32 {
    // Repeating the last distance at the last length codes in a couple of
    // bits, so that one length must be priced on its own rather than
    // folded into the run around it.
    let repeat_length = (distance == state.reps[0] && state.last_length != 0)
        .then_some(state.last_length)
        .filter(|&repeat_length| repeat_length >= length);
    if repeat_length == Some(length) {
        return length;
    }
    let repeated = state.reps.iter().any(|&d| d == distance && d != 0);
    let bonus = if repeated { 0 } else { length_bonus(distance) };
    let Some(value) = length.checked_sub(2 + bonus) else {
        return length;
    };
    if value < 8 {
        return length;
    }
    let bit_count = value.ilog2() as usize - 2;
    let last_value = (((value >> bit_count) + 1) << bit_count) - 1;
    let mut end = (last_value + 2 + bonus).max(length);
    if let Some(repeat_length) = repeat_length {
        end = end.min(repeat_length - 1);
    }
    let _ = variant;
    end.max(length).min(max_match as u32)
}

/// Prices every path through the block and keeps the cheapest (rars
/// `optimal_tokens`, adapted). `prices` is `None` for the first pass, which
/// guesses with [`estimated_match_cost`]. `initial` seeds the distance
/// memory at the block start (the real encoder state, so cross-block cache
/// reuse is priced correctly). Returns the chosen tokens as
/// `(length, distance)` pairs, `(0, byte)` for literals.
#[allow(clippy::too_many_arguments)]
fn optimal_parse_tokens(
    combined: &[u8],
    block: std::ops::Range<usize>,
    max_match: usize,
    window: usize,
    variant: ArchiveVersion,
    prices: Option<&TokenPrices<'_>>,
    matches: &BlockMatches,
    initial: EncoderMatchState,
) -> Vec<(u32, u32)> {
    let start = block.start;
    let end = block.end;
    let span = end - start;

    let mut price = vec![u32::MAX; span + 1];
    let mut arrive_length = vec![0u32; span + 1];
    let mut arrive_distance = vec![0u32; span + 1];
    let mut arrive_reps = vec![[0u32; DIST_CACHE_SIZE]; span + 1];
    let mut arrive_last_length = vec![0u32; span + 1];
    price[0] = 0;
    arrive_reps[0] = initial.reps;
    arrive_last_length[0] = initial.last_length;

    // Runs of `(shortest, longest, distance)` from the position being
    // priced, in the order the collector found them. Reused to keep one
    // allocation.
    let mut reaches: Vec<(u32, u32, u32)> = Vec::new();
    // The first position past a match the parse committed to; nothing is
    // priced before it. See [`NICE_MATCH_LENGTH`].
    let mut committed_through = 0usize;

    for index in 0..span {
        let pos = start + index;
        if index < committed_through {
            continue;
        }
        let here = price[index];
        if here == u32::MAX {
            continue;
        }
        let literal_cost = prices.map_or(ESTIMATED_LITERAL_COST, |prices| {
            prices.literal(combined[pos]) as u32
        });
        let literal = here.saturating_add(literal_cost);
        if literal < price[index + 1] {
            price[index + 1] = literal;
            arrive_length[index + 1] = 0;
            arrive_distance[index + 1] = combined[pos] as u32;
            // A literal emits no distance, so it leaves the remembered
            // distances exactly as it found them.
            arrive_reps[index + 1] = arrive_reps[index];
            arrive_last_length[index + 1] = arrive_last_length[index];
        }

        let max_distance = pos.min(window);
        let max_length = (end - pos).min(max_match);
        if max_distance == 0 || max_length < 4 {
            continue;
        }

        let state = EncoderMatchState::new(arrive_reps[index], arrive_last_length[index]);

        reaches.clear();
        let mut longest = 0u32;

        // A match at a remembered distance is priced out of the main table
        // alone, so it earns its place even when shorter than anything the
        // collector found. The collector only reports a candidate that
        // beats the longest found so far, so these have to be asked for
        // separately. Only the first two cached distances are probed: a
        // repeat of the most recent distance (or the one before it) is
        // where the cheap symbols live, and probing all four roughly
        // doubled the per-position pricing cost for a fraction of a
        // percent of ratio.
        for &repeat in state.reps.iter().take(2) {
            if repeat == 0 || repeat > max_distance as u32 {
                continue;
            }
            let length = match_length_at(combined, pos, repeat as usize, max_length);
            if length >= 4 {
                reaches.push((4, length as u32, repeat));
            }
        }

        // The collector reports nearest first, so the first distance to
        // reach a length is the cheapest one that can. Each report that
        // improves on the longest so far owns one run of lengths.
        for &(length, distance) in matches.at(index) {
            let length = length.min(max_length as u32);
            if length > longest {
                reaches.push((longest + 1, length, distance));
                longest = length;
            }
        }

        // Matches that share a distance and a length slot cost the same, so
        // only the longest of each run is worth pricing. Stepping slot to
        // slot turns a four-thousand-step loop into a few dozen on data
        // that matches long.
        //
        // The collector lists nearest first, so the tail of reaches is the
        // longest end; a position buried in repetitive data can span a
        // dozen length slots across its runs, and the cheapest path almost
        // never wants the short tail of that list. Pricing stops after
        // MAX_PARSE_STEPS_PER_POSITION slot endpoints, which bounds the
        // hot inner loop without dropping the candidates that actually
        // win (the longest runs are priced first).
        // The longest run must always be priced to its end: its reach
        // feeds the committed_through skip below, which is what keeps the
        // parse sublinear on highly repetitive data (a position covered by
        // a long match is never priced again). The remaining runs share
        // MAX_PARSE_STEPS_PER_POSITION, so a position buried in runs of
        // overlapping length slots cannot blow the budget.
        let mut longest_idx = 0usize;
        for (i, &(_, run_end, _)) in reaches.iter().enumerate() {
            if run_end > reaches[longest_idx].1 {
                longest_idx = i;
            }
        }
        let mut steps_left = MAX_PARSE_STEPS_PER_POSITION;
        for (i, &(run_start, run_end, distance)) in reaches.iter().enumerate().rev() {
            let mut length = run_start.max(4);
            while length <= run_end {
                if i != longest_idx {
                    if steps_left == 0 {
                        break;
                    }
                    steps_left -= 1;
                }
                let reach =
                    same_price_run_end(&state, length, distance, variant, max_match).min(run_end);
                let cost = match prices {
                    Some(prices) => prices.match_cost(&state, reach, distance, variant),
                    None => estimated_match_cost(&state, reach, distance, variant),
                };
                // `None`: a fresh-distance match whose length bonus would
                // underflow the encodable raw length — the writer cannot
                // emit it, so it is not a candidate.
                if let Some(cost) = cost {
                    let reached = here.saturating_add(cost as u32);
                    let target = index + reach as usize;
                    if reached < price[target] {
                        price[target] = reached;
                        arrive_length[target] = reach;
                        arrive_distance[target] = distance;
                        let mut next = state;
                        next.remember(reach, distance);
                        arrive_reps[target] = next.reps;
                        arrive_last_length[target] = next.last_length;
                    }
                }
                length = reach + 1;
            }
        }

        // The loop above has priced every run to its end, so the longest
        // match here is already on the board. If it is long enough to
        // commit to, stepping over the bytes it covers changes nothing
        // except the work not done. Only step over a node the parse can
        // actually reach: pricing a match can be skipped, and skipping to a
        // node no path arrives at would leave the rest of the block
        // unreachable and emitted as literals.
        let longest_reach = reaches.iter().map(|&(_, length, _)| length).max();
        if let Some(reach) = longest_reach
            && reach >= NICE_MATCH_LENGTH as u32
            && price[index + reach as usize] != u32::MAX
        {
            committed_through = index + reach as usize;
        }
    }

    let mut reversed = Vec::with_capacity(span);
    let mut index = span;
    while index > 0 {
        let length = arrive_length[index] as usize;
        if length == 0 {
            reversed.push((0, arrive_distance[index]));
            index -= 1;
        } else {
            reversed.push((arrive_length[index], arrive_distance[index]));
            index -= length;
        }
    }
    reversed.reverse();
    reversed
}

/// Length of the match at `pos` against `distance` bytes back, capped at
/// `max_length` (64-bit word compares with a scalar tail).
fn match_length_at(data: &[u8], pos: usize, distance: usize, max_length: usize) -> usize {
    let cand = pos.wrapping_sub(distance);
    let limit = max_length.min(data.len() - pos).min(data.len() - cand);
    let mut l = 0usize;
    while l + 8 <= limit {
        let a = u64::from_le_bytes(data[cand + l..cand + l + 8].try_into().unwrap());
        let b = u64::from_le_bytes(data[pos + l..pos + l + 8].try_into().unwrap());
        if a != b {
            return l + ((a ^ b).trailing_zeros() / 8) as usize;
        }
        l += 8;
    }
    while l < limit && data[cand + l] == data[pos + l] {
        l += 1;
    }
    l
}

/// Convert a token stream into symbols with a live cache walk, counting
/// symbol frequencies at the same time (the block writer counts them the
/// same way, so prices from these frequencies are exact). Returns the
/// symbols and the four frequency vectors. `state` is advanced, mirroring
/// the decoder's cache transitions.
#[allow(clippy::too_many_arguments)]
/// Frequency vectors for the four Huffman tables, counted the same way the
/// block writer counts them.
type TokenFrequencies = (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>);

/// Symbols plus their frequency vectors, as produced by [`convert_tokens`].
type ConvertedTokens = (Vec<Symbol>, TokenFrequencies);

fn convert_tokens(
    tokens: &[(u32, u32)],
    combined: &[u8],
    block: std::ops::Range<usize>,
    state: &mut EncoderMatchState,
    variant: ArchiveVersion,
) -> ConvertedTokens {
    let dc_count = if variant.uses_extra_dist() {
        HUFF_DCX
    } else {
        HUFF_DC
    };
    let mut nc_freq = vec![0u32; HUFF_NC];
    let mut dc_freq = vec![0u32; dc_count];
    let mut ldc_freq = vec![0u32; HUFF_LDC];
    let mut rc_freq = vec![0u32; HUFF_RC];
    let mut symbols = Vec::with_capacity(tokens.len());
    let mut pos = block.start;
    for &(length, distance) in tokens {
        if length == 0 {
            symbols.push(Symbol::Literal(distance as u8));
            nc_freq[distance as usize] += 1;
            pos += 1;
            continue;
        }
        if distance == state.reps[0] && length == state.last_length && state.last_length != 0 {
            symbols.push(Symbol::Repeat);
            nc_freq[SYM_REPEAT] += 1;
            pos += length as usize;
            continue;
        }
        if let Some(index) = state.reps.iter().position(|&d| d == distance && d != 0) {
            symbols.push(Symbol::CacheRef { index, length });
            nc_freq[SYM_CACHE_BASE + index] += 1;
            let len_slot = encode_length_slot(length);
            rc_freq[len_slot] += 1;
            state.remember(length, distance);
            pos += length as usize;
            continue;
        }
        let raw_length = length - length_bonus(distance);
        if raw_length < 2 {
            // Unreachable (the parser rejects unencodable fresh-distance
            // matches), but never emit an invalid match: fall back to
            // literals for the token's span.
            for _ in 0..length {
                let byte = combined[pos];
                symbols.push(Symbol::Literal(byte));
                nc_freq[byte as usize] += 1;
                pos += 1;
            }
            continue;
        }
        symbols.push(Symbol::Match {
            distance,
            length: raw_length,
        });
        let len_slot = encode_length_slot(raw_length);
        nc_freq[SYM_MATCH_BASE + len_slot] += 1;
        let (dist_slot, dist_extra, dbits) = encode_distance_slot(distance, variant);
        dc_freq[dist_slot] += 1;
        if dist_slot >= 4 && dbits >= 4 {
            ldc_freq[(dist_extra & 0xF) as usize] += 1;
        }
        state.remember(length, distance);
        pos += length as usize;
    }
    (symbols, (nc_freq, dc_freq, ldc_freq, rc_freq))
}

/// Build code lengths for the four tables from frequency vectors, matching
/// the block writer exactly.
fn prices_from_frequencies(
    nc_freq: &[u32],
    dc_freq: &[u32],
    ldc_freq: &[u32],
    rc_freq: &[u32],
) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut nc = nc_freq.to_vec();
    let mut dc = dc_freq.to_vec();
    let mut ldc = ldc_freq.to_vec();
    let mut rc = rc_freq.to_vec();
    ensure_nonzero(&mut nc);
    ensure_nonzero(&mut dc);
    ensure_nonzero(&mut ldc);
    ensure_nonzero(&mut rc);
    (
        build_code_lengths_from_freqs(&nc, MAX_CODE_LENGTH),
        build_code_lengths_from_freqs(&dc, MAX_CODE_LENGTH),
        build_code_lengths_from_freqs(&ldc, MAX_CODE_LENGTH),
        build_code_lengths_from_freqs(&rc, MAX_CODE_LENGTH),
    )
}

/// Find matches for `chunk` with the optimal parse (levels 2-5), searching
/// against `state.tail` as lookbehind. Advances `state` so a following
/// chunk/file continues the LZ window. The chunk is split into parse
/// blocks by byte distribution; each block is matched once and priced
/// [`OPTIMAL_PARSE_PASSES`] times against its own previous-pass tables.
///
/// Returns symbols in the same form as [`find_matches_with_tail`] (the
/// caller cuts blocks and encodes).
///
/// `seed_tail` is `false` only when the caller proved the tail holds no
/// useful matches (a multi-threaded worker whose tail probes as
/// incompressible): the tree head is still cleared, the parse inserts the
/// chunk's own positions, and within-chunk plus long-range matches are
/// unaffected — only the wasted fresh-tree seeding of a random tail is
/// skipped.
#[allow(clippy::too_many_arguments)]
fn find_matches_optimal(
    state: &mut EncoderState,
    chunk: &[u8],
    chain_len: usize,
    _lazy_thresh: usize,
    max_match: usize,
    window: usize,
    long_range: bool,
    // Multi-threaded path: a read-only shared long-range table plus the
    // absolute stream anchor of `chunk` (workers query one shared table
    // and never extend it). `None` uses (and extends) the state's own
    // table, the sequential behaviour.
    lr_shared: Option<&match_finder::LongRange>,
    lr_anchor: usize,
    variant: ArchiveVersion,
    passes: usize,
    seed_tail: bool,
) -> Vec<Symbol> {
    let tail_len = state.tail.len();
    let mut combined = Vec::with_capacity(tail_len + chunk.len());
    combined.extend_from_slice(&state.tail);
    combined.extend_from_slice(chunk);

    // The tree finder serves the whole parse. Unlike the chain, one
    // descent per position stays logarithmic even when history makes the
    // hash chains deep (x86 code, generated source), which is where the
    // chain walk spent hundreds of milliseconds per block at high levels.
    // The finder persists across chunks of a member: its links are frame
    // offsets into `combined`, so as the frame slides (the tail drops its
    // oldest bytes) the links are rebased by the slide amount instead of
    // rebuilding the tree and re-seeding the tail — re-seeding cost
    // hundreds of milliseconds per chunk on dense data. Only a fresh
    // finder (multi-threaded workers parse one slice against a brand-new
    // tree) seeds its tail, with budget-limited descents (their matches
    // are already encoded; only their place in the tree matters).
    let tree_window = window.min(combined.len());
    let mut tree_finder = state
        .tree
        .get_or_insert_with(|| match_finder::TreeMatchFinder::new(tree_window));
    tree_finder.grow_to(tree_window);
    let keep = window.min(NEAR_WINDOW_MAX).min(combined.len());
    // `combined_len == 0` marks a fresh frame: the first chunk of a member
    // (or a multi-threaded worker slice, whose state is reused across
    // waves with the tree re-armed per slice). In that case the head must
    // be cleared — the tree may hold links from an earlier frame — and the
    // tail seeded. A continued frame instead rebases the links by the
    // slide amount, keeping the tail's positions valid without re-seeding.
    if state.combined_len == 0 {
        tree_finder.clear_head();
        if seed_tail && tail_len > 0 {
            // Budget-limited descents: a fresh finder only ever occurs in the
            // multi-threaded path, where the tree is built once per slice.
            // Full-depth seeding would walk every dense bucket (tens of
            // thousands of cache misses); 4 nodes per position keeps the
            // newest candidates reachable and the long-range table covers
            // the rest.
            let mut seed: Vec<(u32, u32)> = Vec::new();
            let tail_end = tail_len.min(combined.len().saturating_sub(4));
            for pos in 0..tail_end {
                tree_finder.matches(&combined, pos, 4, tree_window, chain_len.min(4), &mut seed);
            }
        }
    } else if state.combined_len > keep {
        tree_finder.rebase(state.combined_len - keep);
    }
    let finder_kind = &mut tree_finder;

    let lr = if long_range {
        // The near finder (tree) covers distances up to tail + chunk;
        // long-range candidates only matter beyond that.
        let near_max = tail_len + chunk.len();
        match lr_shared {
            Some(table) => Some((table, near_max, lr_anchor)),
            None => {
                let own = state
                    .long_range
                    .get_or_insert_with(|| match_finder::LongRange::new(window));
                Some((&*own, near_max, own.total_pushed()))
            }
        }
    } else {
        None
    };

    let dist_cache = state.dist_cache;
    let last_length = state.last_length;
    let mut state_for_blocks = EncoderMatchState::new(dist_cache, last_length);
    let mut symbols: Vec<Symbol> = Vec::with_capacity(chunk.len());

    // Split the chunk into parse blocks by byte distribution.
    let mut splitter = BlockSplitter::new();
    let mut block_start = tail_len;
    let mut sub_start = tail_len;
    while sub_start < combined.len() {
        let sub_end = (sub_start + OPT_BLOCK_SIZE).min(combined.len());
        let sub = &combined[sub_start..sub_end];
        if !splitter.extends(sub) && splitter.total > 0 {
            // Close the open block at sub_start.
            let block_range = block_start..sub_start;
            let block_symbols = parse_one_block(
                &combined,
                block_range,
                tail_len,
                finder_kind,
                &mut state_for_blocks,
                chain_len,
                max_match,
                window,
                lr,
                variant,
                passes,
            );
            symbols.extend(block_symbols);
            splitter = BlockSplitter::new();
            block_start = sub_start;
        }
        splitter.accept(sub);
        sub_start = sub_end;
    }
    if block_start < combined.len() {
        let block_range = block_start..combined.len();
        let block_symbols = parse_one_block(
            &combined,
            block_range,
            tail_len,
            finder_kind,
            &mut state_for_blocks,
            chain_len,
            max_match,
            window,
            lr,
            variant,
            passes,
        );
        symbols.extend(block_symbols);
    }

    if long_range
        && lr_shared.is_none()
        && let Some(lr) = state.long_range.as_mut()
    {
        lr.push(chunk);
    }

    let keep = window.min(NEAR_WINDOW_MAX).min(combined.len());
    state.tail = combined[combined.len() - keep..].to_vec();
    state.dist_cache = state_for_blocks.reps;
    state.last_length = state_for_blocks.last_length;
    state.combined_len = combined.len();
    symbols
}

/// Parse one block with the optimal parse: collect matches once, then run
/// [`OPTIMAL_PARSE_PASSES`] passes, each repricing against the tables the
/// pass before produced. The last pass's tokens are converted to symbols
/// with the live cache state (which is advanced, so cross-block and
/// cross-chunk cache reuse is exact).
#[allow(clippy::too_many_arguments)]
fn parse_one_block(
    combined: &[u8],
    block: std::ops::Range<usize>,
    tail_len: usize,
    finder: &mut match_finder::TreeMatchFinder,
    state: &mut EncoderMatchState,
    chain_len: usize,
    max_match: usize,
    window: usize,
    lr: Option<(&match_finder::LongRange, usize, usize)>,
    variant: ArchiveVersion,
    passes: usize,
) -> Vec<Symbol> {
    let matches = collect_block_matches(
        finder,
        combined,
        block.clone(),
        tail_len,
        chain_len,
        max_match,
        window,
        lr,
    );

    // Fast path: a block with no match candidates at all parses to pure
    // literals, deterministically — the pricing passes would price the
    // same literal at every position (all three tables rebuild to the
    // same byte histogram) and never relax a match. The collector's tree
    // is heuristic and can miss an exact byte-match at a cached repeat
    // distance (the pricing pass probes those itself), so confirm the two
    // repeat probes stay clean before taking the fast path; the check is
    // a couple of byte compares per position and the result is
    // byte-identical to running the full passes. This is the hot case on
    // incompressible data, where the per-position price bookkeeping (a
    // 1 MiB arrive_reps array plus four more arrays per pass) dominated
    // the parse.
    if matches.runs.is_empty() && !DISABLE_MATCHLESS_FAST_PATH.load(Ordering::Relaxed) {
        let span = block.end - block.start;
        // With no live cached distance the repeat probes are no-ops at
        // every position, so the all-literal conclusion needs no
        // per-position check at all — the common case on incompressible
        // data (no match has ever been emitted, so the entry reps stay
        // zero from the member head). A live rep re-arms the per-position
        // probe loop, which the pricing pass would also run.
        let reps_live = state.reps.iter().take(2).any(|&r| r != 0);
        let mut all_literal = !reps_live;
        if reps_live {
            'probe: for index in 0..span {
                let pos = block.start + index;
                let max_distance = pos.min(window);
                let max_length = (block.end - pos).min(max_match);
                if max_distance == 0 || max_length < 4 {
                    continue;
                }
                // Literals leave the distance memory untouched, so the reps
                // here are the block-entry reps at every position, exactly
                // what the pricing pass would probe with.
                for &repeat in state.reps.iter().take(2) {
                    if repeat == 0 || repeat > max_distance as u32 {
                        continue;
                    }
                    if match_length_at(combined, pos, repeat as usize, max_length) >= 4 {
                        all_literal = false;
                        break 'probe;
                    }
                }
            }
        }
        if all_literal {
            let mut symbols = Vec::with_capacity(span);
            for index in 0..span {
                symbols.push(Symbol::Literal(combined[block.start + index]));
            }
            // Literals leave the repeat cache and last-length exactly as
            // the pricing passes would have.
            return symbols;
        }
    }

    let initial = *state;
    let mut tokens = optimal_parse_tokens(
        combined,
        block.clone(),
        max_match,
        window,
        variant,
        None,
        &matches,
        initial,
    );
    for _ in 1..passes {
        let mut screen = *state;
        let (_, (nc, dc, ldc, rc)) =
            convert_tokens(&tokens, combined, block.clone(), &mut screen, variant);
        let (nc_l, dc_l, ldc_l, rc_l) = prices_from_frequencies(&nc, &dc, &ldc, &rc);
        let prices = TokenPrices {
            nc: &nc_l,
            dc: &dc_l,
            ldc: &ldc_l,
            rc: &rc_l,
        };
        tokens = optimal_parse_tokens(
            combined,
            block.clone(),
            max_match,
            window,
            variant,
            Some(&prices),
            &matches,
            initial,
        );
    }
    let (symbols, _) = convert_tokens(&tokens, combined, block.clone(), state, variant);
    symbols
}

fn find_block_end(symbols: &[Symbol], start: usize, max_uncompressed: usize) -> (usize, usize) {
    let mut count = 0usize;
    let mut last_len = 0u32;
    for (offset, symbol) in symbols[start..].iter().enumerate() {
        let i = start + offset;
        match symbol {
            Symbol::Literal(_) => {
                count += 1;
                last_len = 0;
            }
            Symbol::Match { distance, length } => {
                last_len = apply_length_bonus(*length, *distance);
                count += last_len as usize;
            }
            Symbol::CacheRef { length, .. } => {
                last_len = *length;
                count += *length as usize;
            }
            Symbol::Repeat => {
                count += last_len as usize;
            }
            Symbol::Filter { .. } => {}
        }
        if count >= max_uncompressed {
            return (i + 1, count);
        }
    }
    (symbols.len(), count)
}

/// Adaptive variant of [`find_block_end`]: group symbols into emitted blocks
/// of up to `cap` uncompressed bytes, but close the block early when the
/// symbol stream's *local* literal distribution drifts between adjacent
/// ~64 KiB sub-spans.
///
/// The parse-side splitter compares each sub-block against the cumulative
/// counts of the open block, which cannot see section boundaries once the
/// cumulative mix stabilises (a DLL's code+data+padding blend looks stable
/// over a 1 MiB span). Comparing each sub-span against the *previous* one
/// catches those boundaries: repetitive text stays merged (WinRAR writes
/// one block per member there), heterogeneous binaries keep small blocks
/// (WinRAR's ~64 KiB DLL blocks). The token stream itself is untouched —
/// only the emitted grouping changes, so parsers and decoders behave the
/// same.
fn find_block_end_adaptive(symbols: &[Symbol], start: usize, cap: usize) -> (usize, usize) {
    const SUB_SPAN: usize = 64 * 1024;
    const DRIFT_DIVISOR: usize = 128;
    const LIT: usize = 256;
    const DIST: usize = 5;
    const LEN: usize = 3;
    const BUCKETS: usize = LIT + DIST + LEN;
    fn dist_bucket(d: u32) -> usize {
        if d < 4096 {
            0
        } else if d < 65536 {
            1
        } else if d < 1 << 20 {
            2
        } else if d < 4 << 20 {
            3
        } else {
            4
        }
    }
    fn len_bucket(l: u32) -> usize {
        if l < 16 {
            0
        } else if l < 64 {
            1
        } else {
            2
        }
    }
    let mut count = 0usize;
    let mut last_len = 0u32;
    let mut cur = [0u64; BUCKETS];
    let mut prev = [0u64; BUCKETS];
    let mut span_out = 0usize;
    let mut drifted = false;
    let mut have_prev = false;
    for (offset, symbol) in symbols[start..].iter().enumerate() {
        let i = start + offset;
        match symbol {
            Symbol::Literal(b) => {
                cur[*b as usize] += 1;
                count += 1;
                span_out += 1;
                last_len = 0;
            }
            Symbol::Match { distance, length } => {
                last_len = apply_length_bonus(*length, *distance);
                count += last_len as usize;
                span_out += last_len as usize;
                cur[LIT + dist_bucket(*distance)] += 1;
                cur[LIT + DIST + len_bucket(last_len)] += 1;
            }
            Symbol::CacheRef { length, .. } => {
                last_len = *length;
                count += *length as usize;
                span_out += *length as usize;
                cur[LIT + DIST + len_bucket(last_len)] += 1;
            }
            Symbol::Repeat => {
                count += last_len as usize;
                span_out += last_len as usize;
            }
            Symbol::Filter { .. } => {}
        }
        if span_out >= SUB_SPAN {
            // Full sub-span collected: local drift vs the previous sub-span.
            if have_prev && !drifted {
                let mut misplaced = 0u64;
                for (a, b) in cur.iter().zip(prev.iter()) {
                    misplaced += a.abs_diff(*b);
                }
                if misplaced > SUB_SPAN as u64 / DRIFT_DIVISOR as u64 {
                    drifted = true;
                }
            }
            have_prev = true;
            prev = cur;
            cur = [0u64; BUCKETS];
            span_out = 0;
        }
        if drifted || count >= cap {
            return (i + 1, count);
        }
    }
    (symbols.len(), count)
}

// ── Block encoding ─────────────────────────────────────────────────────────

fn encode_block(symbols: &[Symbol], is_last: bool, variant: ArchiveVersion) -> Vec<u8> {
    // Collect frequencies
    let dc_count = if variant.uses_extra_dist() {
        HUFF_DCX
    } else {
        HUFF_DC
    };
    let mut nc_freq = vec![0u32; HUFF_NC];
    let mut dc_freq = vec![0u32; dc_count];
    let mut ldc_freq = vec![0u32; HUFF_LDC];
    let mut rc_freq = vec![0u32; HUFF_RC];

    for sym in symbols {
        match sym {
            Symbol::Literal(b) => nc_freq[*b as usize] += 1,
            Symbol::Match { distance, length } => {
                let len_slot = encode_length_slot(*length);
                nc_freq[SYM_MATCH_BASE + len_slot] += 1;
                let (dist_slot, _, _) = encode_distance_slot(*distance, variant);
                dc_freq[dist_slot] += 1;
                if dist_slot >= 4 {
                    let dbits = dist_slot / 2 - 1;
                    if dbits >= 4 {
                        let base = (2 | (dist_slot & 1)) << dbits;
                        let low_dist = ((*distance as usize) - 1 - base) & 0xF;
                        ldc_freq[low_dist] += 1;
                    }
                }
            }
            Symbol::CacheRef { index: _, length } => {
                nc_freq[SYM_CACHE_BASE] += 1; // simplified — actual index varies
                let len_slot = encode_length_slot(*length);
                rc_freq[len_slot] += 1;
            }
            Symbol::Repeat => nc_freq[SYM_REPEAT] += 1,
            Symbol::Filter { .. } => nc_freq[SYM_FILTER] += 1,
        }
    }

    // Re-count cache refs with actual indices
    // (The above was a simplification; let's do it properly)
    nc_freq[SYM_CACHE_BASE] = 0;
    for sym in symbols {
        if let Symbol::CacheRef { index, .. } = sym {
            nc_freq[SYM_CACHE_BASE + index] += 1;
        }
    }
    // Subtract the earlier simplified count was only for SYM_CACHE_BASE,
    // but we zeroed it — the per-index counts are correct now.

    ensure_nonzero(&mut nc_freq);
    ensure_nonzero(&mut dc_freq);
    ensure_nonzero(&mut ldc_freq);
    ensure_nonzero(&mut rc_freq);

    let nc_lengths = build_code_lengths_from_freqs(&nc_freq, MAX_CODE_LENGTH);
    let dc_lengths = build_code_lengths_from_freqs(&dc_freq, MAX_CODE_LENGTH);
    let ldc_lengths = build_code_lengths_from_freqs(&ldc_freq, MAX_CODE_LENGTH);
    let rc_lengths = build_code_lengths_from_freqs(&rc_freq, MAX_CODE_LENGTH);

    let enc_nc = EncodeTable::new(&nc_lengths);
    let enc_dc = EncodeTable::new(&dc_lengths);
    let enc_ldc = EncodeTable::new(&ldc_lengths);
    let enc_rc = EncodeTable::new(&rc_lengths);

    let mut writer = BitWriter::new();

    write_tables(
        &mut writer,
        &nc_lengths,
        &dc_lengths,
        &ldc_lengths,
        &rc_lengths,
    );

    for sym in symbols {
        match sym {
            Symbol::Literal(b) => encode_symbol(&enc_nc, &mut writer, *b as usize),
            Symbol::Match { distance, length } => {
                write_match(
                    &mut writer,
                    &enc_nc,
                    &enc_dc,
                    &enc_ldc,
                    *distance,
                    *length,
                    variant,
                );
            }
            Symbol::CacheRef { index, length } => {
                write_cache_ref(&mut writer, &enc_nc, &enc_rc, *index, *length);
            }
            Symbol::Repeat => encode_symbol(&enc_nc, &mut writer, SYM_REPEAT),
            Symbol::Filter {
                block_start,
                block_length,
                filter_type,
                channels,
            } => {
                encode_symbol(&enc_nc, &mut writer, SYM_FILTER);
                write_filter_data(&mut writer, *block_start);
                write_filter_data(&mut writer, *block_length);
                writer.write_bits(*filter_type as u32, 3);
                if *filter_type == FILTER_DELTA {
                    writer.write_bits(channels.saturating_sub(1) as u32, 5);
                }
            }
        }
    }

    let total_bits = writer.bit_count();
    let block_data = writer.into_bytes();

    let (total_bits, block_data) = if total_bits == 0 {
        (8, vec![0u8])
    } else {
        (total_bits, block_data)
    };

    build_block_header(&block_data, total_bits, is_last, true)
}

fn encode_empty_block(variant: ArchiveVersion) -> Vec<u8> {
    let mut writer = BitWriter::new();
    let nc_lengths = {
        let mut v = vec![0u8; HUFF_NC];
        v[0] = 1;
        v
    };
    let dc_lengths = {
        let mut v = vec![
            0u8;
            if variant.uses_extra_dist() {
                HUFF_DCX
            } else {
                HUFF_DC
            }
        ];
        v[0] = 1;
        v
    };
    let ldc_lengths = {
        let mut v = vec![0u8; HUFF_LDC];
        v[0] = 1;
        v
    };
    let rc_lengths = {
        let mut v = vec![0u8; HUFF_RC];
        v[0] = 1;
        v
    };

    write_tables(
        &mut writer,
        &nc_lengths,
        &dc_lengths,
        &ldc_lengths,
        &rc_lengths,
    );

    let total_bits = writer.bit_count();
    let block_data = writer.into_bytes();
    let (total_bits, block_data) = if total_bits == 0 {
        (8, vec![0u8])
    } else {
        (total_bits, block_data)
    };

    build_block_header(&block_data, total_bits, true, true)
}

fn build_block_header(
    block_data: &[u8],
    total_bits: usize,
    is_last: bool,
    table_present: bool,
) -> Vec<u8> {
    let block_size = block_data.len();
    let valid_last_bits = total_bits - (block_size - 1) * 8;
    let bit_size = if valid_last_bits > 0 {
        (valid_last_bits - 1) as u8
    } else {
        7
    };

    let byte_count: u8 = if block_size <= 0xFF {
        1
    } else if block_size <= 0xFFFF {
        2
    } else {
        3
    };

    let mut flags: u8 = 0;
    if table_present {
        flags |= 1 << 7;
    }
    if is_last {
        flags |= 1 << 6;
    }
    flags |= (byte_count - 1) << 3;
    flags |= bit_size & 7;

    let mut size_bytes = vec![0u8; byte_count as usize];
    for (i, byte) in size_bytes.iter_mut().enumerate() {
        *byte = ((block_size >> (i * 8)) & 0xFF) as u8;
    }

    let mut checksum = BLOCK_CHECKSUM_SEED ^ flags;
    for &b in &size_bytes {
        checksum ^= b;
    }

    let mut header = Vec::with_capacity(2 + size_bytes.len() + block_data.len());
    header.push(flags);
    header.push(checksum);
    header.extend(&size_bytes);
    header.extend(block_data);
    header
}

// ── Huffman table writing ──────────────────────────────────────────────────

fn write_tables(
    writer: &mut BitWriter,
    nc_lengths: &[u8],
    dc_lengths: &[u8],
    ldc_lengths: &[u8],
    rc_lengths: &[u8],
) {
    let mut all_lengths = Vec::with_capacity(HUFF_NC + HUFF_DC + HUFF_LDC + HUFF_RC);
    all_lengths.extend_from_slice(nc_lengths);
    all_lengths.extend_from_slice(dc_lengths);
    all_lengths.extend_from_slice(ldc_lengths);
    all_lengths.extend_from_slice(rc_lengths);

    let rle_symbols = rle_encode_lengths(&all_lengths);

    let mut bc_freq = vec![0u32; HUFF_BC];
    for item in &rle_symbols {
        bc_freq[item.0 as usize] += 1;
    }
    ensure_nonzero(&mut bc_freq);

    let bc_lengths = build_code_lengths_from_freqs(&bc_freq, MAX_CODE_LENGTH);
    write_bc_nibbles(writer, &bc_lengths);

    let enc_bc = EncodeTable::new(&bc_lengths);
    for item in &rle_symbols {
        encode_symbol(&enc_bc, writer, item.0 as usize);
        match item.0 {
            16 => writer.write_bits(item.1 as u32 - 3, 3),
            17 => writer.write_bits(item.1 as u32 - 11, 7),
            18 => writer.write_bits(item.1 as u32 - 3, 3),
            19 => writer.write_bits(item.1 as u32 - 11, 7),
            _ => {}
        }
    }
}

fn write_bc_nibbles(writer: &mut BitWriter, bc_lengths: &[u8]) {
    let mut i = 0;
    while i < HUFF_BC {
        let val = bc_lengths[i];
        if val == 0 {
            let mut run = 0;
            while i + run < HUFF_BC && bc_lengths[i + run] == 0 {
                run += 1;
            }
            while run > 0 {
                if run >= 3 {
                    let count = run.min(16);
                    writer.write_bits(NIBBLE_ESCAPE as u32, 4);
                    writer.write_bits((count - 2) as u32, 4);
                    run -= count;
                    i += count;
                } else {
                    writer.write_bits(0, 4);
                    run -= 1;
                    i += 1;
                }
            }
        } else if val == 15 {
            writer.write_bits(NIBBLE_ESCAPE as u32, 4);
            writer.write_bits(0, 4);
            i += 1;
        } else {
            writer.write_bits(val as u32, 4);
            i += 1;
        }
    }
}

/// RLE item: (symbol, repeat_count). For symbols 0-15, count is 0.
struct RleItem(u8, usize);

fn rle_encode_lengths(lengths: &[u8]) -> Vec<RleItem> {
    let mut result = Vec::new();
    let n = lengths.len();
    let mut i = 0;

    while i < n {
        let val = lengths[i];
        if val == 0 {
            let mut run = 0;
            while i + run < n && lengths[i + run] == 0 {
                run += 1;
            }
            while run > 0 {
                if run >= 11 {
                    let count = run.min(138);
                    result.push(RleItem(19, count));
                    run -= count;
                    i += count;
                } else if run >= 3 {
                    let count = run.min(10);
                    result.push(RleItem(18, count));
                    run -= count;
                    i += count;
                } else {
                    result.push(RleItem(0, 0));
                    run -= 1;
                    i += 1;
                }
            }
        } else {
            result.push(RleItem(val, 0));
            i += 1;
            let mut run = 0;
            while i + run < n && lengths[i + run] == val {
                run += 1;
            }
            while run > 0 {
                if run >= 11 {
                    let count = run.min(138);
                    result.push(RleItem(17, count));
                    run -= count;
                    i += count;
                } else if run >= 3 {
                    let count = run.min(10);
                    result.push(RleItem(16, count));
                    run -= count;
                    i += count;
                } else {
                    break;
                }
            }
        }
    }

    result
}

// ── Match/distance encoding helpers ────────────────────────────────────────

fn write_match(
    writer: &mut BitWriter,
    enc_nc: &EncodeTable,
    enc_dc: &EncodeTable,
    enc_ldc: &EncodeTable,
    dist: u32,
    length: u32,
    variant: ArchiveVersion,
) {
    let len_slot = encode_length_slot(length);
    encode_symbol(enc_nc, writer, SYM_MATCH_BASE + len_slot);
    write_length_extra(writer, length, len_slot);

    let (dist_slot, extra, dbits) = encode_distance_slot(dist, variant);
    encode_symbol(enc_dc, writer, dist_slot);
    write_distance_extra(writer, enc_ldc, dist_slot, extra, dbits);
}

fn write_cache_ref(
    writer: &mut BitWriter,
    enc_nc: &EncodeTable,
    enc_rc: &EncodeTable,
    cache_idx: usize,
    length: u32,
) {
    encode_symbol(enc_nc, writer, SYM_CACHE_BASE + cache_idx);
    let len_slot = encode_length_slot(length);
    encode_symbol(enc_rc, writer, len_slot);
    write_length_extra(writer, length, len_slot);
}

fn encode_length_slot(length: u32) -> usize {
    if length < 2 {
        return 0;
    }
    if length <= 9 {
        return (length - 2) as usize;
    }
    let val = length - 2;
    let high_bit = 32 - val.leading_zeros() - 1;
    if high_bit < 2 {
        return (length - 2) as usize;
    }
    let lbits = high_bit - 2;
    let sub = (val >> lbits) & 3;
    let slot = 4 * (lbits + 1) + sub;
    slot.min(HUFF_RC as u32 - 1) as usize
}

fn write_length_extra(writer: &mut BitWriter, length: u32, slot: usize) {
    if slot >= 8 {
        let lbits = (slot / 4 - 1) as u8;
        let base = 2 + ((4 | (slot & 3)) << lbits) as u32;
        let extra = length - base;
        if lbits > 0 {
            writer.write_bits(extra, lbits);
        }
    }
}

/// Map a match distance to its distance-code slot.
///
/// RAR5 caps the table at 64 entries (`HUFF_DC`), RAR7 (v70) at 80
/// (`HUFF_DCX`), which covers distances up to 1 TiB. `dbits >= 4` slots
/// split their extra bits between a raw prefix and the low nibble encoded
/// through the LDC table.
fn encode_distance_slot(dist: u32, variant: ArchiveVersion) -> (usize, u32, usize) {
    if dist <= 4 {
        return ((dist - 1) as usize, 0, 0);
    }
    let val = dist - 1;
    let high_bit = (32 - val.leading_zeros() - 1) as usize;
    if high_bit < 1 {
        return ((dist - 1) as usize, 0, 0);
    }
    let dbits = high_bit - 1;
    let sub = (val >> dbits) & 1;
    let slot = 2 * (dbits + 1) + sub as usize;
    let base = (2 | sub) << dbits;
    let extra = val - base;
    let max_slot = if variant.uses_extra_dist() {
        HUFF_DCX - 1
    } else {
        HUFF_DC - 1
    };
    (slot.min(max_slot), extra, dbits)
}

fn write_distance_extra(
    writer: &mut BitWriter,
    enc_ldc: &EncodeTable,
    dist_slot: usize,
    extra: u32,
    dbits: usize,
) {
    if dist_slot >= 4 && dbits > 0 {
        if dbits >= 4 {
            if dbits > 4 {
                let upper = extra >> 4;
                writer.write_bits(upper, (dbits - 4) as u8);
            }
            let low = (extra & 0xF) as usize;
            encode_symbol(enc_ldc, writer, low);
        } else {
            writer.write_bits(extra, dbits as u8);
        }
    }
}

// ── Cache helpers ──────────────────────────────────────────────────────────

fn cache_find(cache: &[u32; DIST_CACHE_SIZE], dist: u32) -> Option<usize> {
    cache.iter().position(|&d| d == dist)
}

fn cache_push(cache: &mut [u32; DIST_CACHE_SIZE], dist: u32) {
    cache[3] = cache[2];
    cache[2] = cache[1];
    cache[1] = cache[0];
    cache[0] = dist;
}

fn cache_touch(cache: &mut [u32; DIST_CACHE_SIZE], idx: usize) {
    let val = cache[idx];
    for i in (1..=idx).rev() {
        cache[i] = cache[i - 1];
    }
    cache[0] = val;
}

fn apply_length_bonus(length: u32, dist: u32) -> u32 {
    let mut l = length;
    if dist > 0x100 {
        l += 1;
    }
    if dist > 0x2000 {
        l += 1;
    }
    if dist > 0x40000 {
        l += 1;
    }
    l
}

fn remove_length_bonus(length: u32, dist: u32) -> u32 {
    let mut l = length;
    if dist > 0x100 {
        l = l.saturating_sub(1);
    }
    if dist > 0x2000 {
        l = l.saturating_sub(1);
    }
    if dist > 0x40000 {
        l = l.saturating_sub(1);
    }
    l
}

fn ensure_nonzero(freq: &mut [u32]) {
    let nonzero = freq.iter().filter(|&&f| f > 0).count();
    if nonzero < 2 {
        let mut added = 0;
        for f in freq.iter_mut() {
            if *f == 0 {
                *f = 1;
                added += 1;
                if nonzero + added >= 2 {
                    break;
                }
            }
        }
    }
}

/// Emit RAR5 filter data: `[2 bits byte_count-1][LE bytes]`.
fn write_filter_data(writer: &mut BitWriter, value: u32) {
    let bytes = value.to_le_bytes();
    let count = if value <= 0xFF {
        1
    } else if value <= 0xFFFF {
        2
    } else if value <= 0xFF_FFFF {
        3
    } else {
        4
    };
    writer.write_bits((count - 1) as u32, 2);
    for &b in &bytes[..count] {
        writer.write_bits(b as u32, 8);
    }
}

#[cfg(test)]
mod encode_tests {
    use super::decode_to_writer;
    use super::*;
    use crate::codec::huffman::DecodeTable;

    fn one_symbol_table(count: usize) -> Vec<u8> {
        let mut v = vec![0u8; count];
        v[0] = 1;
        v
    }

    /// Build a compressed RAR5 stream containing a Delta filter followed by
    /// delta-encoded literals, then verify the streaming decoder applies
    /// the filter and produces the original bytes.
    #[test]
    fn streaming_decode_applies_delta_filter() {
        let original: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
        // delta_decode computes cumulative negative sums:
        // result[i] = result[i-1] - D[i], so D[i] = prev - original[i].
        let mut delta = vec![0u8; original.len()];
        let mut prev = 0u8;
        for (i, &b) in original.iter().enumerate() {
            delta[i] = prev.wrapping_sub(b);
            prev = b;
        }

        let mut nc_freq = vec![0u32; HUFF_NC];
        for &b in &delta {
            nc_freq[b as usize] += 1;
        }
        nc_freq[SYM_FILTER] += 1;
        ensure_nonzero(&mut nc_freq);

        let nc_lengths = build_code_lengths_from_freqs(&nc_freq, MAX_CODE_LENGTH);
        let dc_lengths = build_code_lengths_from_freqs(&vec![1u32; HUFF_DC], MAX_CODE_LENGTH);
        let ldc_lengths = build_code_lengths_from_freqs(&[1u32; HUFF_LDC], MAX_CODE_LENGTH);
        let rc_lengths = build_code_lengths_from_freqs(&[1u32; HUFF_RC], MAX_CODE_LENGTH);
        let enc_nc = EncodeTable::new(&nc_lengths);

        let mut writer = BitWriter::new();
        write_tables(
            &mut writer,
            &nc_lengths,
            &dc_lengths,
            &ldc_lengths,
            &rc_lengths,
        );

        // Filter symbol + data (offset 0, length = original, delta, 1 channel).
        encode_symbol(&enc_nc, &mut writer, SYM_FILTER);
        write_filter_data(&mut writer, 0);
        write_filter_data(&mut writer, original.len() as u32);
        writer.write_bits(FILTER_DELTA as u32, 3);
        writer.write_bits(0, 5); // channels - 1

        for &b in &delta {
            encode_symbol(&enc_nc, &mut writer, b as usize);
        }

        let total_bits = writer.bit_count();
        let block_data = writer.into_bytes();
        let stream = build_block_header(&block_data, total_bits, true, true);

        let mut out = Vec::new();
        let written = decode_to_writer(
            &stream,
            original.len() as u64,
            DecodeOptions {
                dict_size_log: 0,
                ..Default::default()
            },
            &mut out,
        )
        .expect("decode");
        assert_eq!(written as usize, original.len());
        assert_eq!(out, original);
    }

    #[test]
    fn encoder_state_carries_matches_across_chunks() {
        // A long run of identical bytes must still compress when the input
        // is split across chunk boundaries with a shared encoder state.
        let data = vec![0xABu8; 3 * DEFAULT_CHUNK_SIZE + 12345];
        let mut state = EncoderState::default();
        let mut packed = Vec::new();
        let chunks: Vec<&[u8]> = data.chunks(DEFAULT_CHUNK_SIZE).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            packed.extend(
                encode_chunked_raw(
                    chunk,
                    5,
                    3,
                    DEFAULT_CHUNK_SIZE,
                    Some(&mut state),
                    i + 1 == chunks.len(),
                    None,
                    ArchiveVersion::Rar50,
                )
                .unwrap(),
            );
        }
        assert!(
            packed.len() * 4 < data.len(),
            "long repeats must compress well across chunks: {} vs {}",
            packed.len(),
            data.len()
        );
        let roundtrip = crate::codec::decode_standalone(
            &packed,
            data.len() as u64,
            3,
            None,
            ArchiveVersion::Rar50,
        )
        .unwrap();
        assert_eq!(roundtrip, data);
    }

    #[test]
    fn one_symbol_tables_are_valid() {
        let table = DecodeTable::new(&one_symbol_table(4));
        assert_eq!(table.num_symbols, 4);
    }

    #[test]
    fn v70_extra_dist_self_roundtrip() {
        use crate::codec::decode_standalone;
        // Varied data (literal-heavy) at several sizes.
        for size in [1usize, 100, 1000, 100_000, 300_000] {
            let data: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(31) >> 3) as u8).collect();
            let packed = encode_raw(&data, 3, 0, ArchiveVersion::Rar70);
            let back = decode_standalone(
                &packed,
                size as u64,
                0,
                Some(128 * 1024),
                ArchiveVersion::Rar70,
            )
            .unwrap();
            assert_eq!(back, data, "size {size}");
        }
        // Repeated data (match/cache-heavy).
        let data = vec![0xABu8; 300_000];
        let packed = encode_raw(&data, 3, 0, ArchiveVersion::Rar70);
        let back = decode_standalone(
            &packed,
            data.len() as u64,
            0,
            Some(128 * 1024),
            ArchiveVersion::Rar70,
        )
        .unwrap();
        assert_eq!(back, data);
    }

    /// Deterministic pseudo-random bytes (LCG) — incompressible.
    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    /// Long-range matches (WinRAR `-mcl` semantics): a repeated block far
    /// beyond the near window (tail + chunk) must still compress, and the
    /// stream must round-trip byte-identically.
    #[test]
    fn long_range_matches_compress_distant_repeats() {
        use crate::codec::decode_standalone;
        // 2 MiB random + 2 MiB exact copy: distance 2 MiB, far beyond the
        // 128 KiB near window of a 64 KiB chunk encoder.
        let half = 2 * 1024 * 1024usize;
        let mut data = pseudo_random(half, 42);
        let copy = data[..half].to_vec();
        data.extend_from_slice(&copy);
        let packed = encode_chunked_raw(
            &data,
            3,
            8,
            64 * 1024,
            None,
            true,
            None,
            ArchiveVersion::Rar50,
        )
        .unwrap();
        // The copy half must compress to a small fraction; the random half
        // stores at ~1:1. Well below 1.5 MiB total proves the 2 MiB
        // repeat was matched.
        assert!(
            packed.len() < half + half / 4,
            "distant repeat must compress: {} vs {}",
            packed.len(),
            data.len()
        );
        // Byte-identical round-trip (dictionary 32 MiB covers the 2 MiB
        // distance; unpacked size over the RAR5 4 GiB cap is irrelevant).
        let back =
            decode_standalone(&packed, data.len() as u64, 8, None, ArchiveVersion::Rar50).unwrap();
        assert_eq!(back, data);
    }

    /// Long-range matches respect the dictionary window: repeats beyond
    /// the declared dictionary must NOT be encoded as matches (the decoder
    /// window could not reach them). With a 128 KiB dictionary the 2 MiB
    /// copy is incompressible, so the encoder must store it.
    #[test]
    fn long_range_respects_dictionary_window() {
        // Same distant repeat as above, but with dict_size_log = 0
        // (128 KiB): the 2 MiB distance exceeds the window.
        let half = 2 * 1024 * 1024usize;
        let mut data = pseudo_random(half, 7);
        let copy = data[..half].to_vec();
        data.extend_from_slice(&copy);
        let packed = encode_chunked_raw(
            &data,
            3,
            0,
            64 * 1024,
            None,
            true,
            None,
            ArchiveVersion::Rar50,
        )
        .unwrap();
        // Random half ~1:1 + copy half ~1:1 → near 4 MiB. Bail-out may
        // truncate; either way it must not shrink below ~half.
        assert!(
            packed.len() > half,
            "beyond-window repeats must not match: {} vs {}",
            packed.len(),
            data.len()
        );
    }

    /// The long-range history slides: after more than LONG_RANGE_MAX
    /// bytes, the oldest bytes drop out and the table is rebuilt; the
    /// newest candidates keep matching.
    #[test]
    fn long_range_slides_window_and_finds() {
        use crate::codec::match_finder::{LONG_RANGE_MAX, LongRange};
        let mut lr = LongRange::new(128 * 1024 * 1024);
        let chunk = pseudo_random(64 * 1024, 11);
        // Push enough identical 64 KiB chunks to slide the 64 MiB window
        // several times over.
        for _ in 0..(LONG_RANGE_MAX / chunk.len() + 8) {
            lr.push(&chunk);
        }
        assert!(lr.hist_len() <= LONG_RANGE_MAX);
        // The chunk still matches against the (rebuilt) history; max_len
        // caps the match at 4096 bytes. (All blocks are identical, so the
        // table keeps only the most recent candidate — near the window
        // end; min_dist 1 accepts it.)
        let (dist, len) = lr.find(&chunk, 0, 1, 4096).expect("must find");
        assert_eq!(len, 4096, "full match must be found after sliding");
        assert!(dist as usize > 0 && dist as usize <= LONG_RANGE_MAX);
    }

    /// Debug reproduction: a distant copy of a random block must be found
    /// through the long-range table (the pair.bin scenario).
    #[test]
    fn long_range_debug_distant_copy() {
        use crate::codec::match_finder::LongRange;
        let half = 2 * 1024 * 1024usize;
        let first = pseudo_random(half, 42);
        let mut lr = LongRange::new(32 * 1024 * 1024);
        for c in first.chunks(64 * 1024) {
            lr.push(c);
        }
        let r = lr.find(&first, 0, 128 * 1024, 4096);
        assert!(
            matches!(r, Some((_, l)) if l > 1000),
            "distant copy must be found, got {r:?}"
        );
    }

    /// Simulates the streaming write path: one `encode_chunked` call per
    /// 4 MiB buffer with a shared encoder state. A 64 MiB distant copy
    /// must compress (WinRAR `-mcl` semantics for large files).
    #[test]
    fn long_range_streaming_simulation() {
        let half = 4 * 1024 * 1024usize;
        let first = pseudo_random(half, 42);
        let mut data = first.clone();
        data.extend_from_slice(&first);
        let mut state = EncoderState::default();
        let mut packed = Vec::new();
        for chunk in data.chunks(DEFAULT_CHUNK_SIZE) {
            let is_final = chunk.len() < DEFAULT_CHUNK_SIZE;
            packed.extend(
                encode_chunked_raw(
                    chunk,
                    3,
                    8,
                    DEFAULT_CHUNK_SIZE,
                    Some(&mut state),
                    is_final,
                    None,
                    ArchiveVersion::Rar50,
                )
                .unwrap(),
            );
        }
        assert!(
            packed.len() < half + half / 4,
            "streaming long-range must compress the distant copy: {}",
            packed.len()
        );
    }
}
