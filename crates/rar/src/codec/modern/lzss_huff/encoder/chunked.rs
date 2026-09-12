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

use super::emit::{encode_block, encode_empty_block};
#[cfg(feature = "parallel")]
use super::parse::find_matches_in_range;
use super::parse::{
    OPTIMAL_PARSE_PASSES, find_block_end_adaptive, find_matches_optimal, find_matches_with_tail,
};

#[cfg(feature = "parallel")]
use crate::codec::common::match_finder;
#[cfg(feature = "parallel")]
use crate::error::RarError;
use crate::error::RarResult;
use crate::version::ArchiveVersion;

/// Cap for grouping parsed symbols into *emitted* blocks. The RAR5 size
/// field allows blocks up to 4 GiB, so this is purely an encoder choice:
/// on distribution-stable data (repetitive text) merging many parse blocks
/// into one emitted block amortises the per-block Huffman table definitions
/// (WinRAR writes one block per whole member there); on heterogeneous data
/// the tables stay per-parse-block because the drift check keeps the parse
/// blocks small. Only the emitted grouping is larger — the parse itself is
/// unchanged, so token choices are byte-identical to the 128 KiB cap.
const EMITTED_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Default input chunk size for the encoder. Processing input in bounded
/// slices keeps the symbol table (and match finder) memory proportional to
/// the chunk size instead of the whole file.
pub const DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024;

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
    progress: Option<&mut dyn FnMut(u64, u64)>,
    variant: ArchiveVersion,
) -> RarResult<Vec<u8>> {
    encode_chunked_raw_inner(
        data,
        method,
        dict_size_log,
        chunk_size,
        state,
        is_final,
        progress,
        variant,
        None,
    )
}

/// Sequential variant of [`encode_chunked_raw`] that prepends `lead`
/// symbols (filter records of a filtered member) to the first chunk's
/// symbol stream, so the records are read before any block output. Used by
/// the streaming writer for per-window delta filter records while keeping
/// the persistent encoder state across chunks/windows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_chunked_raw_with_lead(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    state: Option<&mut EncoderState>,
    is_final: bool,
    progress: Option<&mut dyn FnMut(u64, u64)>,
    variant: ArchiveVersion,
    lead: Option<&[Symbol]>,
) -> RarResult<Vec<u8>> {
    encode_chunked_raw_inner(
        data,
        method,
        dict_size_log,
        chunk_size,
        state,
        is_final,
        progress,
        variant,
        lead,
    )
}

// Private entry point of the encoder: one argument per encode dimension
// (input, method, dictionary, chunking, solid state, finality, progress,
// codec variant, solid lead-in). Bundling them into a struct would add a layer
// to the hottest path for no behaviour change; the audit defers codec hot-path
// restructuring to the breaking release.
#[allow(clippy::too_many_arguments)]
fn encode_chunked_raw_inner(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    state: Option<&mut EncoderState>,
    is_final: bool,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
    variant: ArchiveVersion,
    lead: Option<&[Symbol]>,
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
        let mut symbols = if level >= 2 {
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
        // Filter records of a filtered member lead the first chunk's symbol
        // stream (read before any output, member-relative positions).
        if chunk_start == 0
            && let Some(lead) = lead
        {
            let mut joined = lead.to_vec();
            joined.append(&mut symbols);
            symbols = joined;
        }

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
    let (chain_len, lazy_thresh, max_match) = LEVEL_PARAMS[level];
    let dict_size = 128 * 1024 * (1usize << dict_size_log as u32);
    let long_range = level >= 2;
    // MT parse is the low-step tier by design: workers slice with the cheap
    // hash-chain greedy+lazy search instead of the optimal (BT4) parse,
    // cutting the per-position step count well below the tree's. It accepts
    // MT output divergence from the sequential path (already a documented,
    // accepted divergence) and is the MT fastest measured configuration
    // (tsc x86 2.1x, repeated text 5.2x at ~+0.6..2.9pp ratio on the A/B
    // corpus); seq is never affected.
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
    // Absolute stream position of `data[0]`. It is the *pushed* total, not
    // the retained history length: once the sampled window has slid, `hist`
    // starts at `hist_base() > 0` and the two differ by exactly that much —
    // using `hist_len()` here anchors every worker query short and silently
    // drops long-range matches later in a chain.
    let entry_len = lr_shared.total_pushed();
    lr_shared.push(data);
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
    // low-step parse's `head`/`prev` ring arrays stay warm (a fresh 64 MiB
    // two-array allocation per slice cost page faults and per-frame
    // memsets). Waves run sequentially so each state is used by exactly one
    // thread at a time.
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
                            lazy_thresh,
                            max_match,
                            dict_size,
                            long_range,
                            is_final && k + 1 == n,
                            variant,
                            (k == 0).then_some(lead_symbols).flatten(),
                            state,
                            // Only the first slice sees chain history as its
                            // lookbehind; later slices look into this window.
                            k == 0 && !tail_ref.is_empty(),
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
/// finder ever pay off? Samples 4-byte windows every [`MT_SEED_PROBE_STRIDE`]
/// bytes over the tail's head. A tail whose sampled windows are (almost)
/// all distinct has no long repeats, so seeding it is wasted work — random
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
/// Each worker runs the MT low-step parse (hash-chain greedy+lazy over the
/// slice's tail context, shared read-only long-range table): a bounded
/// chain walk + lazy skip per position instead of the sequential path's
/// tree descent. That divergence is the accepted price for MT speed (see
/// [`encode_chunked_mt`]).
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
    lazy_thresh: usize,
    max_match: usize,
    dict_size: usize,
    long_range: bool,
    is_last_block_of_member: bool,
    variant: ArchiveVersion,
    // Filter records of a filtered member, prepended to the first slice's
    // symbol stream so they precede all output (member-relative positions).
    lead_symbols: Option<&[Symbol]>,
    state: &mut EncoderState,
    // Seed the lookbehind even when its sampled windows look random. The
    // probe above only measures whether the tail repeats *inside itself*,
    // which says nothing about how the slice relates to bytes that came from
    // an earlier member of a solid chain (or an earlier window). Skipping the
    // seed there leaves a hole: the near finder never sees those positions
    // and the long-range table rejects the same distances, because its
    // `min_dist` assumes the near finder covered them — cross-member
    // duplicates then silently stop compressing.
    force_seed_tail: bool,
) -> Vec<u8> {
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
    let want = NEAR_WINDOW_MAX.min(dict_size);
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
    let seed_tail = if force_seed_tail {
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
    let mut symbols = mt_slice_symbols_low_step(
        state,
        data,
        s0,
        e0,
        lr_shared,
        entry_len,
        chain_len,
        lazy_thresh,
        max_match,
        dict_size,
        long_range,
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
#[allow(clippy::too_many_arguments)]
fn mt_slice_symbols_low_step(
    state: &mut EncoderState,
    data: &[u8],
    s0: usize,
    e0: usize,
    lr_shared: &match_finder::LongRange,
    entry_len: usize,
    chain_len: usize,
    lazy_thresh: usize,
    max_match: usize,
    dict_size: usize,
    long_range: bool,
    seed_tail: bool,
) -> Vec<Symbol> {
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
    let parts = state.chain_parts.take();
    let mut finder = match parts {
        Some((head, prev)) => {
            match_finder::MatchFinder::reuse(&combined, 2, max_match, chain, dict_size, head, prev)
        }
        None => match_finder::MatchFinder::new(&combined, 2, max_match, chain, dict_size),
    };
    if seed_tail {
        for pos in 0..tl {
            finder.insert(pos);
        }
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
