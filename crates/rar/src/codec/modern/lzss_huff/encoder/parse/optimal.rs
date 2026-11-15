//! The symbol parsers: the fast/lazy walker and the price-driven optimal parse.
//!
//! Both share the match finders (hash chain, BT4 tree, sampled long range) and
//! emit the [`Symbol`] stream consumed by [`super::super::emit`]; the emitted
//! grouping lives in [`super::block`], the candidate collection in
//! [`super::collect`].

use super::super::*;

#[cfg(feature = "parallel")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::super::super::{
    HUFF_DC, HUFF_DCX, HUFF_LDC, HUFF_NC, HUFF_RC, MAX_CODE_LENGTH, SYM_CACHE_BASE, SYM_MATCH_BASE,
    SYM_REPEAT,
};
use super::super::emit::{
    apply_length_bonus, cache_find, cache_push, cache_touch, encode_distance_slot,
    encode_length_slot, ensure_nonzero, remove_length_bonus,
};
use super::super::profile;
use super::collect::{
    BlockMatches, DISABLE_MATCHLESS_FAST_PATH, FAST_RECOVER_INTERVAL, NICE_MATCH_LENGTH,
    RELAXED_MATCHLESS_MAX_LEN, SearchState, ShortFinder, collect_block_matches, match_length_at,
};
use crate::codec::common::huffman::build_code_lengths_from_freqs;
use crate::codec::common::match_finder::MatchFinder;
use crate::version::ArchiveVersion;
use std::time::Instant;

/// Fresh-frame tail seeding shape from the MT worker path (issue 12): the
/// newest `MT_NEAR_TIGHT_FRONTIER` tail bytes are seeded densely, and the
/// older tail up to `NEAR_WINDOW_MAX` at `MT_FAR_SEED_STRIDE` with a shorter
/// descent budget. Matched copies anchor on the first seeded source position
/// within a few bytes of the copy start, so the stride preserves the far
/// reach at a fraction of the insert cost. (Dormant since the MT workers
/// moved to the low-step chain tier — the `lr_shared` gate that fed the
/// stride never fires on the remaining sequential-only callers; the shape is
/// kept for the record.)
const MT_NEAR_TIGHT_FRONTIER: usize = 2 * 1024 * 1024;
const MT_FAR_SEED_STRIDE: usize = 16;
const MT_FAR_SEED_CHAIN: usize = 2;

/// Find matches for `chunk`, searching against `state.tail` as lookbehind.
/// Advances `state` so a following chunk/file continues the LZ window.
/// When `long_range` is set, distances beyond the near window are found
/// through the sampled long-range history (WinRAR `-mcl` semantics).
pub(crate) fn find_matches_with_tail(
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
    let keep = window.min(near_window_max()).min(combined.len());
    state.tail = combined[combined.len() - keep..].to_vec();
    state.dist_cache = dist_cache;
    state.last_length = last_length;
    symbols
}

/// The priced tier's driver: a block-global DP (the sequential path's
/// `optimal_parse_tokens`) over candidates supplied by `emit`.
///
/// `emit(pos, cache, runs)` reports the candidates worth starting at `pos` as
/// `(length, distance)` pairs with **increasing** length, nearest distance
/// first — the same shape and order the sequential collector produces, so the
/// DP's "each report that improves on the longest so far owns one run of
/// lengths" reading applies unchanged. A source that only has one candidate
/// pushes one pair.
///
/// `lr` (when present) adds candidates from the sampled long-range table:
/// `(long_range, near_reach, anchor)`, where `near_reach` is how many bytes of
/// history the near source holds at `start` and `anchor` is the absolute
/// history offset of `start`. A position `k` bytes into the buffer must be
/// complemented from distance `near_reach + k + 1` on, which is what keeps the
/// buffer's own bytes from being probed twice.
///
/// MT-only: the driver is `chunked::mt_slice_symbols_row_index`, which is
/// compiled only with the `parallel` feature.
#[cfg(feature = "parallel")]
pub(crate) fn windowed_priced_parse<F>(
    combined: &[u8],
    span: std::ops::Range<usize>,
    dist_cache: &mut [u32; DIST_CACHE_SIZE],
    last_length: &mut u32,
    limits: super::MatchLimits,
    lr: Option<(&match_finder::LongRange, usize, usize)>,
    mut emit: F,
) -> Vec<Symbol>
where
    F: FnMut(usize, &[u32; DIST_CACHE_SIZE], &mut Vec<(u32, u32)>),
{
    let start = span.start;
    let end = span.end;
    let max_match = limits.max_match;
    let variant = limits.variant;
    let mut symbols = Vec::with_capacity(end - start);
    let mut state = EncoderMatchState::new(*dist_cache, *last_length);
    let probe_cache = *dist_cache;
    let block_size = super::super::chunked::MT_ROW_INDEX_DP_BLOCK;
    // The previous block's code-length table, carried into this block's first
    // pass. A worker's blocks run in order inside one slice, so the table it
    // hands forward is the one the parse would have rebuilt itself: pricing
    // against a real distribution instead of the flat first-pass estimate is
    // worth roughly what a second repricing pass buys (0.4-0.8pp on text) and
    // costs no second DP. The first block of a slice has no predecessor and
    // keeps the estimate; that keeps slices independent, which is what makes
    // them parallel at all.
    let carry = !PRICE_CARRY_DISABLED.load(Ordering::Relaxed);
    let mut carried: Option<TokenPrices> = None;
    // Re-used across the slice's blocks: a `BlockMatches` costs a `starts`
    // entry per position and the DP tables 32 bytes per position.
    let mut matches = BlockMatches {
        runs: Vec::new(),
        starts: Vec::new(),
    };
    let mut arrays = DpArrays::default();
    let mut block_start = start;
    while block_start < end {
        let block_end = (block_start + block_size).min(end);
        let span = block_end - block_start;
        matches.reset(span);
        let mut longest = 0usize;
        for pos in block_start..block_end {
            matches.starts.push(matches.runs.len() as u32);
            let before = matches.runs.len();
            emit(pos, &probe_cache, &mut matches.runs);
            let mut length = 0usize;
            for &(run_length, _) in &matches.runs[before..] {
                length = length.max(run_length as usize);
            }
            // The shared long-range table is the only source of matches into
            // history before this buffer (a solid chain's earlier members, or an
            // earlier MT window), so probe it where the greedy tier does: a good
            // near match is never worse than a far one.
            if let Some((long_range, near_reach, anchor)) = lr
                && length < 64
                && pos + 4 <= end
            {
                let chunk_off = pos - start;
                if let Some((long_dist, long_length)) = long_range.find_from(
                    &combined[start..end],
                    chunk_off,
                    anchor,
                    near_reach + chunk_off + 1,
                    max_match,
                ) && long_length > length
                {
                    matches.runs.push((long_length as u32, long_dist));
                    length = long_length;
                }
            }
            longest = longest.max(length);
        }
        matches.starts.push(matches.runs.len() as u32);

        let live_repeat = state.reps.iter().any(|&distance| distance != 0);
        if longest <= RELAXED_MATCHLESS_MAX_LEN && !live_repeat {
            symbols.extend(
                combined[block_start..block_end]
                    .iter()
                    .map(|&byte| Symbol::Literal(byte)),
            );
            state.last_length = 0;
        } else {
            let tokens = optimal_parse_tokens(
                combined,
                block_start..block_end,
                limits,
                carried.as_ref(),
                &matches,
                state,
                &mut arrays,
            );
            let (block_symbols, (nc, dc, ldc, rc)) = convert_tokens(
                &tokens,
                combined,
                block_start..block_end,
                &mut state,
                variant,
            );
            if carry {
                let (nc_l, dc_l, ldc_l, rc_l) = prices_from_frequencies(&nc, &dc, &ldc, &rc);
                carried = Some(TokenPrices::new(&nc_l, &dc_l, &ldc_l, &rc_l));
            }
            symbols.extend(block_symbols);
        }
        block_start = block_end;
    }
    *dist_cache = state.reps;
    *last_length = state.last_length;
    symbols
}

/// Test seam: stop a DP block from handing its price table to the next one,
/// so a test can measure what the hand-off buys.
#[cfg(feature = "parallel")]
pub(super) static PRICE_CARRY_DISABLED: AtomicBool = AtomicBool::new(false);

#[cfg(all(test, feature = "parallel"))]
#[allow(dead_code)]
pub(crate) fn set_price_carry_enabled(enabled: bool) {
    PRICE_CARRY_DISABLED.store(!enabled, Ordering::Relaxed);
}

/// Buckets in [`RowIndex`]: a 4-byte hash, like the sequential finders' key
/// length. Hashing four bytes means every bucket member is a potential 4-byte
/// match, so no candidate is probed and thrown away on the minimum length.
///
/// MT-only (see [`windowed_priced_parse`]).
#[cfg(feature = "parallel")]
const ROW_INDEX_HASH_BITS: u32 = 20;
#[cfg(feature = "parallel")]
const ROW_INDEX_BUCKETS: usize = 1 << ROW_INDEX_HASH_BITS;

/// A shared, read-only row index over one member buffer: every position grouped
/// by the hash of its first three bytes, as a counting-sorted CSR.
///
/// Unlike the BT4 tree or the per-frame hash chain it carries no
/// insertion-order state and needs no per-worker copy, so workers can query the
/// member's whole history without re-inserting it — which is exactly what made
/// a per-slice *sequential* parse cost more than the sequential member (the tree
/// re-insert of a slice's lookbehind measured ~5x the member at ~5.3 MiB/s).
/// Candidates come out newest-first within the distance window, like the chain's.
///
/// MT-only: built by `chunked::mt_slice_symbols_row_index` and queried by
/// [`windowed_priced_parse`], both `parallel`-gated.
#[cfg(feature = "parallel")]
pub(crate) struct RowIndex {
    starts: Vec<u32>,
    positions: Vec<u32>,
}

#[cfg(feature = "parallel")]
impl RowIndex {
    /// Build the index in two passes (count, then place). O(n) at memory
    /// bandwidth, paid once per member instead of per slice.
    pub(crate) fn build(data: &[u8]) -> Self {
        let count = data.len().saturating_sub(3);
        let mut starts = vec![0u32; ROW_INDEX_BUCKETS + 1];
        for pos in 0..count {
            starts[Self::hash(data, pos) + 1] += 1;
        }
        for hash in 0..ROW_INDEX_BUCKETS {
            starts[hash + 1] += starts[hash];
        }
        // Place through a private cursor: `starts` must keep the bucket ends,
        // so writing back into it would collapse every range to empty.
        let mut cursor = starts.clone();
        let mut positions = vec![0u32; count];
        for pos in 0..count {
            let hash = Self::hash(data, pos);
            positions[cursor[hash] as usize] = pos as u32;
            cursor[hash] += 1;
        }
        Self { starts, positions }
    }

    fn hash(data: &[u8], pos: usize) -> usize {
        let value = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        (value.wrapping_mul(0x9E37_79B1) >> (32 - ROW_INDEX_HASH_BITS)) as usize
    }

    /// Longest match at `pos` among the newest `depth` candidates within
    /// `max_distance` bytes, or `None` when nothing reaches four bytes.
    #[cfg(test)]
    pub(crate) fn longest(
        &self,
        data: &[u8],
        pos: usize,
        max_distance: usize,
        max_match: usize,
        depth: usize,
    ) -> Option<(u32, u32)> {
        let mut runs = Vec::with_capacity(4);
        self.collect(data, pos, max_distance, max_match, depth, &mut runs);
        runs.last().copied()
    }

    /// Report the candidates at `pos` the way the sequential collector does:
    /// walks the bucket newest-first (distance ascending) and pushes one
    /// `(length, distance)` run per candidate that beats the best length so
    /// far, so the DP can trade a shorter near match against a longer far one.
    /// At most `depth` candidates are compared.
    pub(crate) fn collect(
        &self,
        data: &[u8],
        pos: usize,
        max_distance: usize,
        max_match: usize,
        depth: usize,
        out: &mut Vec<(u32, u32)>,
    ) {
        if pos + 3 >= data.len() {
            return;
        }
        let hash = Self::hash(data, pos);
        let bucket = &self.positions[self.starts[hash] as usize..self.starts[hash + 1] as usize];
        let end = bucket.partition_point(|&candidate| (candidate as usize) < pos);
        let max_length = max_match.min(data.len() - pos);
        let mut best_length = 0usize;
        let mut checked = 0usize;
        for &candidate in bucket[..end].iter().rev() {
            let distance = pos - candidate as usize;
            if distance > max_distance {
                break;
            }
            // A candidate only improves the best when its byte at the current
            // best length matches, so probe that byte before the full compare.
            if best_length == 0 || data[candidate as usize + best_length] == data[pos + best_length]
            {
                let length = match_length_at(data, pos, distance, max_length);
                if length >= 4 && length > best_length {
                    out.push((length as u32, distance as u32));
                    best_length = length;
                    if length == max_length {
                        break;
                    }
                }
            }
            checked += 1;
            if checked >= depth {
                break;
            }
        }
    }
}

/// How far one DP block spans in [`windowed_priced_parse`]. Bigger blocks buy
/// lookahead and fewer block restarts at the cost of the DP's per-position
/// arrays (about 16 bytes per block byte, per worker).
///
/// MT-only, like its only reader [`windowed_priced_parse`].
#[cfg(feature = "parallel")]
pub(crate) const MT_DP_BLOCK_SIZE: usize = 256 * 1024;

/// Match-finding loop over `data[start..end]` with a distance cache.
/// `lr` (when present) adds long-range candidates from the sampled
/// history: `(long_range, near_max)` where `near_max` is the largest
/// distance the near finder can produce (tail + chunk), so long-range
/// hits are only considered beyond it.
///
/// The argument list keeps its `too_many_arguments` allow on purpose: unlike
/// the block parser's clump, it mixes the two mutable decoder registers
/// (`dist_cache` / `last_length`, borrowed as disjoint fields of the encoder
/// state by both callers) with the search limits, so bundling what is left
/// would have to move those registers into a struct and change the callers'
/// data flow for one allow.
#[allow(clippy::too_many_arguments)]
pub(crate) fn find_matches_in_range(
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

/// After this many consecutive positions with no match found, the optimal
/// parse's match collection stops probing the long-range table on every
/// position and drops to a [`FAST_RECOVER_INTERVAL`] cadence (incompressible
/// runs then cost the tree search instead of a cache-missing probe per byte;
/// the lazy matcher fast mode does the same).
const FAST_MODE_AFTER: usize = 64 * 1024;

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

    /// The distance-side half of a match's classification: everything
    /// [`MatchDistancePlan::shape`] needs besides the length. The optimal
    /// parser walks one run of lengths at a fixed distance, so it computes
    /// this once per run instead of re-deriving the cache slot, the length
    /// bonus and the distance slot for every candidate length (the per-pair
    /// recomputation zstd's optimal parser does and Fast LZMA2 hoists).
    fn plan(&self, distance: u32, variant: ArchiveVersion) -> MatchDistancePlan {
        let repeat_length = if distance == self.reps[0] && self.last_length != 0 {
            self.last_length
        } else {
            0
        };
        let cache_index = self.reps.iter().position(|&d| d == distance && d != 0);
        let fresh = cache_index.is_none().then(|| {
            let (dist_slot, dist_extra, dbits) = encode_distance_slot(distance, variant);
            FreshDistancePlan {
                bonus: length_bonus(distance),
                dist_slot,
                dist_extra,
                dbits,
            }
        });
        MatchDistancePlan {
            repeat_length,
            cache_index,
            fresh,
        }
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

/// What a match costs besides its length: the distance-side facts the
/// parser resolves once per length run (see [`EncoderMatchState::plan`]).
#[derive(Debug, Clone, Copy)]
struct MatchDistancePlan {
    /// The remembered length that codes as a repeat at `reps[0]`; `0` when
    /// this distance is not the repeat distance.
    repeat_length: u32,
    /// Cache slot of a remembered distance; `None` means a fresh distance.
    cache_index: Option<usize>,
    /// The fresh-distance facts, present exactly when `cache_index` is `None`.
    fresh: Option<FreshDistancePlan>,
}

/// A fresh (uncached) distance's fixed part: the length bonus the raw length
/// must absorb, and the distance slot, extra value and bit depth the slot
/// costs.
#[derive(Debug, Clone, Copy)]
struct FreshDistancePlan {
    bonus: u32,
    dist_slot: usize,
    dist_extra: u32,
    dbits: usize,
}

impl FreshDistancePlan {
    /// Bits a match at this distance spends on the distance itself beyond its
    /// slot symbol: the extra value, plus v70's `ldc` symbol for the low
    /// nibble of a slot that carries four or more bits.
    fn distance_bits(self, prices: &TokenPrices) -> u32 {
        if self.dbits >= 4 {
            self.dbits as u32 - 4 + prices.ldc_cost[(self.dist_extra & 0xF) as usize]
        } else {
            self.dbits as u32
        }
    }
}

/// One length's worth of a [`MatchDistancePlan`], mirroring the three shapes
/// the writer can emit. Resolved one candidate at a time, and kept only as the
/// reference [`SlotPricer`] is pinned against.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum MatchShape {
    Repeat,
    Cache {
        index: usize,
        len_slot: usize,
    },
    New {
        len_slot: usize,
        fresh: FreshDistancePlan,
    },
}

impl MatchDistancePlan {
    /// Resolve the shape for one length. `None` when the match cannot be
    /// encoded: a fresh distance's length bonus can exceed the raw length
    /// (the format's minimum match length is 2), and the optimal parser must
    /// not price a token the writer cannot emit.
    #[cfg(test)]
    fn shape(self, length: u32) -> Option<MatchShape> {
        if self.repeat_length != 0 && length == self.repeat_length {
            return Some(MatchShape::Repeat);
        }
        if let Some(index) = self.cache_index {
            return Some(MatchShape::Cache {
                index,
                len_slot: encode_length_slot(length),
            });
        }
        let fresh = self.fresh?;
        let raw_length = length.checked_sub(fresh.bonus)?;
        if raw_length < 2 {
            return None;
        }
        Some(MatchShape::New {
            len_slot: encode_length_slot(raw_length),
            fresh,
        })
    }
}

/// The length bonus added at decode time for a match at `distance`.
fn length_bonus(distance: u32) -> u32 {
    u32::from(distance > 0x100) + u32::from(distance > 0x2000) + u32::from(distance > 0x40000)
}

/// Extra bits written after a length slot (0 for slots below 8). Depends on
/// the slot alone, so a pass can tabulate it (see [`TokenPrices`]).
const fn slot_extra_bits(slot: usize) -> u32 {
    if slot < 8 { 0 } else { (slot / 4 - 1) as u32 }
}

/// [`slot_extra_bits`] as a table, so the first pass — which has no Huffman
/// code lengths to price with yet — steps length slots through the same
/// [`SlotPricer`] the repriced passes use.
static SLOT_EXTRA_BITS: [u32; HUFF_RC] = {
    let mut table = [0u32; HUFF_RC];
    let mut slot = 0;
    while slot < HUFF_RC {
        table[slot] = slot_extra_bits(slot);
        slot += 1;
    }
    table
};

/// Estimated bit cost of a match before any block has been priced (rars
/// `estimated_match_cost`). `None` when the match cannot be encoded.
#[cfg(test)]
fn estimated_match_cost(plan: &MatchDistancePlan, length: u32) -> Option<u32> {
    Some(match plan.shape(length)? {
        MatchShape::Repeat => 2,
        MatchShape::Cache { len_slot, .. } => 5 + slot_extra_bits(len_slot),
        MatchShape::New { len_slot, fresh } => 10 + slot_extra_bits(len_slot) + fresh.dbits as u32,
    })
}

/// The Huffman code lengths a block of symbols produces, as the block
/// writer computes them (same frequency counting, same `ensure_nonzero`,
/// same length-limit pass), reduced once per pricing pass to the per-symbol
/// bit costs the optimal parse adds up.
///
/// The parse prices a candidate once per length slot of every run, so it adds
/// precomputed components instead of re-deriving slot, extra bits and code
/// length per candidate — the same tabulation zstd's optimal parser wants for
/// `ZSTD_getMatchPrice`/`ZSTD_litLengthPrice`.
struct TokenPrices {
    /// Cost of one literal byte (and, at the symbol bases, of a cache/repeat
    /// or match symbol).
    nc_cost: [u32; HUFF_NC],
    repeat_cost: u32,
    /// Cost of a cached distance's symbol, by cache slot.
    cache_cost: [u32; DIST_CACHE_SIZE],
    /// `rc[len_slot]` cost plus the slot's extra bits.
    cache_len_cost: [u32; HUFF_RC],
    /// `nc[SYM_MATCH_BASE + len_slot]` cost plus the slot's extra bits.
    new_len_cost: [u32; HUFF_RC],
    /// `dc[dist_slot]` cost (v70's wider table included).
    dc_cost: [u32; HUFF_DCX],
    /// `ldc` cost for a distance slot's low extra nibble.
    ldc_cost: [u32; 16],
}

impl TokenPrices {
    /// Derive the cost tables from one pass's code lengths.
    fn new(nc: &[u8], dc: &[u8], ldc: &[u8], rc: &[u8]) -> Self {
        let mut nc_cost = [0u32; HUFF_NC];
        for (cost, &bits) in nc_cost.iter_mut().zip(nc) {
            *cost = Self::code(bits);
        }
        let mut dc_cost = [0u32; HUFF_DCX];
        for (cost, &bits) in dc_cost.iter_mut().zip(dc) {
            *cost = Self::code(bits);
        }
        let mut ldc_cost = [0u32; 16];
        for (cost, &bits) in ldc_cost.iter_mut().zip(ldc) {
            *cost = Self::code(bits);
        }
        let mut cache_len_cost = [0u32; HUFF_RC];
        let mut new_len_cost = [0u32; HUFF_RC];
        for slot in 0..HUFF_RC {
            let extra = slot_extra_bits(slot);
            cache_len_cost[slot] = Self::code(rc[slot]) + extra;
            new_len_cost[slot] = nc_cost[SYM_MATCH_BASE + slot] + extra;
        }
        let mut cache_cost = [0u32; DIST_CACHE_SIZE];
        for (index, cost) in cache_cost.iter_mut().enumerate() {
            *cost = nc_cost[SYM_CACHE_BASE + index];
        }
        Self {
            nc_cost,
            repeat_cost: nc_cost[SYM_REPEAT],
            cache_cost,
            cache_len_cost,
            new_len_cost,
            dc_cost,
            ldc_cost,
        }
    }

    fn code(bits: u8) -> u32 {
        if bits == 0 {
            UNUSED_SYMBOL_COST as u32
        } else {
            u32::from(bits)
        }
    }

    fn literal(&self, byte: u8) -> u32 {
        self.nc_cost[byte as usize]
    }

    /// Bits to code a match of `length` bytes at the plan's distance. Resolves
    /// the shape, the length slot and the distance side separately, for one
    /// candidate at a time; [`SlotPricer::step`] is the fused form the parser
    /// actually runs.
    #[cfg(test)]
    fn match_cost(&self, plan: &MatchDistancePlan, length: u32) -> Option<u32> {
        Some(match plan.shape(length)? {
            MatchShape::Repeat => self.repeat_cost,
            MatchShape::Cache { index, len_slot } => {
                self.cache_cost[index] + self.cache_len_cost[len_slot]
            }
            MatchShape::New { len_slot, fresh } => {
                self.new_len_cost[len_slot]
                    + self.dc_cost[fresh.dist_slot]
                    + fresh.distance_bits(self)
            }
        })
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

/// The longest match at `distance` that costs exactly what a match of
/// `length` costs. Only the length slot varies with length, and a slot
/// covers a run of consecutive lengths, so the end of that run is the last
/// length worth pricing (rars `same_price_run_end`). `plan` already carries
/// what the distance side of the price depends on — whether it repeats the
/// last match, whether it sits in the cache, and the length bonus a fresh
/// distance costs — so the run is computed without looking at the encoder
/// state again.
#[cfg(test)]
fn same_price_run_end(plan: &MatchDistancePlan, length: u32, max_match: usize) -> u32 {
    // Repeating the last distance at the last length codes in a couple of
    // bits, so that one length must be priced on its own rather than
    // folded into the run around it.
    let repeat_length = (plan.repeat_length >= length).then_some(plan.repeat_length);
    if repeat_length == Some(length) {
        return length;
    }
    // Whether the distance is already in the cache is what [`EncoderMatchState::plan`]
    // recorded, and a cached distance costs no length bonus at all.
    let bonus = plan.fresh.map_or(0, |fresh| fresh.bonus);
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
    end.max(length).min(max_match as u32)
}

/// One run's worth of pricing, hoisted out of the per-length step. The
/// distance side of a run — the length-slot cost table, the distance cost, the
/// length bonus a fresh distance makes the raw length absorb, and the length
/// that codes as a repeat — is the same for every length in the run, so the run
/// classifies it once and each step is then one `ilog2`, one shift and two
/// table lookups, instead of resolving a shape, a slot and the distance side per
/// candidate (as rars does, and as Fast LZMA2 hoists away).
///
/// Every step is pinned to the expanded form — [`same_price_run_end`] plus
/// [`TokenPrices::match_cost`] or [`estimated_match_cost`], both kept below as
/// the reference — by `slot_pricer_tests`: same reach, same price, same `None`s,
/// for every length of every run shape.
struct SlotPricer<'a> {
    /// Per-slot length cost, without the distance side.
    slot_cost: &'a [u32; HUFF_RC],
    /// What the distance itself costs: a cache slot's symbol, or a fresh
    /// distance's slot symbol plus its extra bits; on the first pass, the guess
    /// the estimated price uses.
    base: u32,
    repeat_cost: u32,
    /// Length bonus a fresh distance costs, zero for a cached one.
    bonus: u32,
    /// The remembered length that codes as a repeat, `0` when this distance is
    /// not the repeat distance. Pricing never steps through that length.
    repeat_length: u32,
    /// Price of `repeat_length - 1`, resolved once per run: that length can
    /// fall in a later slot than the run's other steps.
    repeat_bound: Option<u32>,
}

impl<'a> SlotPricer<'a> {
    /// Classify one run of lengths at `distance`, priced against the distance
    /// memory `state` holds at the run's start.
    fn for_run(
        state: &EncoderMatchState,
        distance: u32,
        variant: ArchiveVersion,
        prices: Option<&'a TokenPrices>,
    ) -> Self {
        let plan = state.plan(distance, variant);
        let (slot_cost, base) = match (prices, plan.cache_index, plan.fresh) {
            (Some(prices), Some(index), _) => (&prices.cache_len_cost, prices.cache_cost[index]),
            (Some(prices), None, Some(fresh)) => (
                &prices.new_len_cost,
                prices.dc_cost[fresh.dist_slot] + fresh.distance_bits(prices),
            ),
            // The first pass has no code lengths yet, so a slot costs the extra
            // bits it carries (see [`estimated_match_cost`]).
            (None, Some(_), _) => (&SLOT_EXTRA_BITS, 5),
            (None, None, Some(fresh)) => (&SLOT_EXTRA_BITS, 10 + fresh.dbits as u32),
            // A plan is always either cached or fresh; pricing stays total
            // anyway, at what a symbol no table assigned costs.
            (_, None, None) => (&SLOT_EXTRA_BITS, UNUSED_SYMBOL_COST as u32),
        };
        let mut pricer = Self {
            slot_cost,
            base,
            repeat_cost: prices.map_or(2, |prices| prices.repeat_cost),
            bonus: plan.fresh.map_or(0, |fresh| fresh.bonus),
            repeat_length: plan.repeat_length,
            repeat_bound: None,
        };
        if pricer.repeat_length > 0 {
            pricer.repeat_bound = pricer.price_of_one(pricer.repeat_length - 1);
        }
        pricer
    }

    /// Price of a single length, for the lengths [`Self::step`] cannot fold into
    /// its slot arithmetic.
    fn price_of_one(&self, length: u32) -> Option<u32> {
        let raw = length.checked_sub(self.bonus)?;
        if raw < 2 {
            return None;
        }
        Some(self.base + self.slot_cost[encode_length_slot(raw)])
    }

    /// The longest reach worth pricing from `length` (never past `run_end`), and
    /// what that reach costs — the same reach [`same_price_run_end`] gives and
    /// the same price the expanded form charges for it.
    #[inline]
    fn step(&self, length: u32, run_end: u32) -> (u32, Option<u32>) {
        // Repeating the last distance at the remembered length codes in a
        // couple of bits, so that length must be priced on its own rather than
        // folded into the run around it.
        if length == self.repeat_length {
            return (length, Some(self.repeat_cost));
        }
        let Some(value) = length.checked_sub(2 + self.bonus) else {
            // The bonus outgrew the raw length, and the shortest match the
            // writer can emit is two bytes: this match cannot be encoded.
            return (length, None);
        };
        // The eight lowest slots cover one length each, so there is no run of
        // lengths to walk.
        if value < 8 {
            return (length, Some(self.base + self.slot_cost[value as usize]));
        }
        // A slot holds `2 << bits` lengths; it ends at the last value that
        // shares this one's leading bits, which is the longest length still
        // coding in the same slot and so the last worth pricing.
        let bits = value.ilog2() - 2;
        let sub = (value >> bits) & 3;
        let slot = (4 * (bits + 1) + sub).min(HUFF_RC as u32 - 1) as usize;
        let natural_end = (((value >> bits) + 1) << bits) - 1 + 2 + self.bonus;
        let cost = self.base + self.slot_cost[slot];
        let reach = natural_end.min(run_end);
        if length < self.repeat_length {
            let bound = self.repeat_length - 1;
            if reach > bound {
                // `bound` can sit in a later slot than `length`, so it carries
                // its own price — and that price may not be one at all.
                return (bound, self.repeat_bound);
            }
        }
        (reach, Some(cost))
    }
}

/// The optimal parse's per-position tables, kept across blocks. They are 32
/// bytes per position (`price`, length, distance, four reps, last length), so
/// a 64 KiB block would allocate and zero 2 MB before it prices its first
/// candidate — 200 such blocks per 13 MiB member. Sizing them once per chunk
/// and only re-filling the used prefix turns that into two memsets of the
/// same memory, with no fresh pages to fault in.
#[derive(Default)]
pub(super) struct DpArrays {
    price: Vec<u32>,
    arrive_length: Vec<u32>,
    arrive_distance: Vec<u32>,
    arrive_reps: Vec<[u32; DIST_CACHE_SIZE]>,
    arrive_last_length: Vec<u32>,
}

impl DpArrays {
    fn reset(&mut self, span: usize) {
        let reach = span + 1;
        self.price.clear();
        self.price.resize(reach, u32::MAX);
        self.arrive_length.clear();
        self.arrive_length.resize(reach, 0);
        self.arrive_distance.clear();
        self.arrive_distance.resize(reach, 0);
        self.arrive_reps.clear();
        self.arrive_reps.resize(reach, [0; DIST_CACHE_SIZE]);
        self.arrive_last_length.clear();
        self.arrive_last_length.resize(reach, 0);
    }
}

/// Per-chunk scratch shared by the blocks of one parse: the collector's
/// match table and the DP's per-position tables.
pub(super) struct ParseBuffers {
    matches: BlockMatches,
    arrays: DpArrays,
    /// The official's three recent-occurrence tables (`+0xb1e8` byte,
    /// `+0xb1f0` ushort, `+0xb1f8` u32), filled for every position and
    /// shared by all the blocks of one parse. Allocating them per block
    /// costs a 4.5 MB memset every 64 KiB of input, which is most of the
    /// time the fused path spent in the archive benchmark.
    tables: ShortTables,
}

impl Default for ParseBuffers {
    fn default() -> Self {
        Self {
            matches: BlockMatches {
                runs: Vec::new(),
                starts: Vec::new(),
            },
            arrays: DpArrays::default(),
            tables: ShortTables::default(),
        }
    }
}

/// Prices every path through the block and keeps the cheapest (rars
/// `optimal_tokens`, adapted). `prices` is `None` for the first pass, which
/// guesses with [`estimated_match_cost`]. `initial` seeds the distance
/// memory at the block start (the real encoder state, so cross-block cache
/// reuse is priced correctly). Returns the chosen tokens as
/// `(length, distance)` pairs, `(0, byte)` for literals.
fn optimal_parse_tokens(
    combined: &[u8],
    block: std::ops::Range<usize>,
    limits: super::MatchLimits,
    prices: Option<&TokenPrices>,
    matches: &BlockMatches,
    initial: EncoderMatchState,
    arrays: &mut DpArrays,
) -> Vec<(u32, u32)> {
    let start = block.start;
    let end = block.end;
    let max_match = limits.max_match;
    let window = limits.window;
    let variant = limits.variant;
    let span = end - start;

    arrays.reset(span);
    let DpArrays {
        price,
        arrive_length,
        arrive_distance,
        arrive_reps,
        arrive_last_length,
    } = arrays;
    price[0] = 0;
    arrive_reps[0] = initial.reps;
    arrive_last_length[0] = initial.last_length;

    // Runs of `(shortest, longest, distance)` from the position being
    // priced, in the order the collector found them.
    let mut reaches: Vec<(u32, u32, u32)> = Vec::with_capacity(16);
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
            prices.literal(combined[pos])
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
        if max_distance == 0 || max_length < 2 {
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
            if length >= 2 {
                reaches.push((2, length as u32, repeat));
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
            // The distance side of every candidate in this run is the same,
            // so classify it once: the per-length step below only resolves
            // the length slot (and the repeat length's exact match).
            let pricer = SlotPricer::for_run(&state, distance, variant, prices);
            let mut length = run_start.max(2);
            while length <= run_end {
                if i != longest_idx {
                    if steps_left == 0 {
                        break;
                    }
                    steps_left -= 1;
                }
                let (reach, cost) = pricer.step(length, run_end);
                // `None`: a fresh-distance match whose length bonus would
                // underflow the encodable raw length — the writer cannot
                // emit it, so it is not a candidate.
                if let Some(cost) = cost {
                    let reached = here.saturating_add(cost);
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

/// Frequency vectors for the four Huffman tables, counted the same way the
/// block writer counts them.
type TokenFrequencies = (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>);

/// Symbols plus their frequency vectors, as produced by [`convert_tokens`].
type ConvertedTokens = (Vec<Symbol>, TokenFrequencies);

/// Convert a token stream into symbols with a live cache walk, counting
/// symbol frequencies at the same time (the block writer counts them the
/// same way, so prices from these frequencies are exact). Returns the
/// symbols and the four frequency vectors. `state` is advanced, mirroring
/// the decoder's cache transitions.
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
/// useful matches: the tree head is still cleared, the parse inserts the
/// chunk's own positions, and within-chunk plus long-range matches are
/// unaffected — only the wasted fresh-tree seeding of a random tail is
/// skipped. (The multi-threaded workers do not reach this function; they
/// run the low-step chain tier in [`mt_slice_symbols_low_step`].)
pub(crate) fn find_matches_optimal(
    state: &mut EncoderState,
    chunk: &[u8],
    spec: super::SequentialSearch<'_>,
) -> Vec<Symbol> {
    let overall = Instant::now();
    let super::SequentialSearch {
        limits,
        chain_len,
        miss_threshold,
        long_range,
        lr_shared,
        lr_anchor,
        seed_tail,
        ..
    } = spec;
    let max_match = limits.max_match;
    let window = limits.window;
    let variant = limits.variant;
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
    let keep = window.min(near_window_max()).min(combined.len()); // BT4 is the only finder: the ADR 0011 P2 distance-class matcher was
    // measured and rejected (see the P2 sections of that ADR), so this is the
    // shape that shipped before it. The tree persists across the chunks of a
    // member: its links are frame offsets into `combined`, so as the frame
    // slides (the tail drops its oldest bytes) the links are rebased by the
    // slide amount instead of rebuilding the structure and re-seeding the
    // tail — re-seeding cost hundreds of milliseconds per chunk on dense data.
    // Only a fresh frame seeds its tail, with budget-limited descents (their
    // matches are already encoded; only their place in the tree matters).
    // The head table's bucket count is fixed when the finder is built (see
    // `TreeMatchFinder::head_bits_for`), while a member's frame ramps up to
    // `keep + chunk`: building it at the frame's current length would leave
    // the buckets narrow for the whole member, and widening them later means
    // rehashing, which throws the tree away mid-parse. So build it at the
    // window it will span — four times what is on the frame now, which is
    // already the whole window for a member larger than a chunk (a small
    // member is not going to grow past four times its own length, and a table
    // bigger than the data is just cache misses). Only the buckets a member
    // hashes ever fault in.
    let tree_sizing = window
        .min(near_window_max() + chunk.len())
        .min(combined.len() * 4)
        .max(4);
    let tree_finder = state
        .tree
        .get_or_insert_with(|| match_finder::TreeMatchFinder::new(tree_sizing));
    tree_finder.grow_to(tree_window);
    // `combined_len == 0` marks a fresh frame: the first chunk of a member
    // (the multi-threaded workers no longer reach here — they run the
    // low-step chain tier; the tree is sequential-only now). In that case
    // the head must be cleared — the tree may hold links from an earlier
    // frame — and the tail seeded. A continued frame instead rebases the
    // links by the slide amount, keeping the tail's positions valid without
    // re-seeding.
    if state.combined_len == 0 {
        tree_finder.clear_head();
        // The short tables only reach 8 KiB back, so seeding them costs a few
        // thousand stores and never needs the budget the tree seed uses.
        state.short.clear();
        state.search.clear();
        if tail_len > 0 {
            state.short.seed(&combined, tail_len);
        }
        if seed_tail && tail_len > 0 {
            // Flat fresh-frame seed: every tail position is inserted with a
            // budget-limited descent (`lr_shared` is always `None` here —
            // the strided far-tail thinning it used to gate on lived in the
            // MT worker path, which now runs the low-step chain tier, so
            // `mt`/`MT_*_SEED_*` below never fire and stay as the recorded
            // issue-12 shape). Dense buckets cost cache misses, but the
            // budget caps the descent; the nearest `MT_NEAR_TIGHT_FRONTIER`
            // bytes are what copies actually anchor on.
            let mt = lr_shared.is_some();
            let tight_start = tail_len.saturating_sub(MT_NEAR_TIGHT_FRONTIER);
            let mut seed: Vec<(u32, u32)> = Vec::new();
            let tail_end = tail_len.min(combined.len().saturating_sub(4));
            for pos in 0..tail_end {
                let far = mt && pos < tight_start;
                if far && pos % MT_FAR_SEED_STRIDE != 0 {
                    continue;
                }
                let budget = if far {
                    chain_len.min(MT_FAR_SEED_CHAIN)
                } else {
                    chain_len.min(4)
                };
                tree_finder.matches(match_finder::MatchQuery {
                    input: &combined,
                    pos,
                    len_limit: 4,
                    max_distance: tree_window,
                    cut: budget,
                    out: &mut seed,
                });
            }
        }
    } else if state.combined_len > keep {
        tree_finder.rebase(state.combined_len - keep);
        state.short.shift(state.combined_len - keep);
    }
    profile::add(profile::PARSE_PREP, overall);
    let finder_kind = &mut *tree_finder;
    let short_kind = &mut state.short;
    let search_kind = &mut state.search;

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

    // One block-search spec for the whole chunk: the two `parse_one_block`
    // calls below (the tail block and the final one) differ only in range.
    let block_spec = super::BlockSearch {
        tail_len,
        chain_len,
        miss_threshold,
        limits: super::MatchLimits {
            max_match,
            window,
            variant,
        },
        lr,
    };

    // Split the chunk into parse blocks by byte distribution.
    let mut splitter = BlockSplitter::new();
    let mut block_start = tail_len;
    let mut sub_start = tail_len;
    let mut buffers = ParseBuffers::default();
    while sub_start < combined.len() {
        let sub_end = (sub_start + OPT_BLOCK_SIZE).min(combined.len());
        let sub = &combined[sub_start..sub_end];
        if !splitter.extends(sub) && splitter.total > 0 {
            // Close the open block at sub_start.
            let block_range = block_start..sub_start;
            let block_symbols = parse_one_block(
                &combined,
                block_range,
                finder_kind,
                short_kind,
                search_kind,
                &mut state_for_blocks,
                block_spec,
                &mut buffers,
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
            finder_kind,
            short_kind,
            search_kind,
            &mut state_for_blocks,
            block_spec,
            &mut buffers,
        );
        symbols.extend(block_symbols);
    }

    if long_range
        && lr_shared.is_none()
        && let Some(lr) = state.long_range.as_mut()
    {
        lr.push(chunk);
    }

    let keep = window.min(near_window_max()).min(combined.len());
    state.tail = combined[combined.len() - keep..].to_vec();
    state.dist_cache = state_for_blocks.reps;
    state.last_length = state_for_blocks.last_length;
    state.combined_len = combined.len();
    profile::add(profile::PARSE_TOTAL, overall);
    symbols
}

/// Parse one block with the optimal parse: collect the block's matches once,
/// price every path through them in one DP pass, then convert the winning
/// tokens to symbols with the live cache state (which is advanced, so
/// cross-block and cross-chunk cache reuse is exact).
///
/// `buffers` carries the block's `BlockMatches` and the DP's per-position
/// tables across blocks: both are sized by the block span, so re-using them
/// is what keeps a 13 MiB member from re-allocating and re-zeroing 2 MB per
/// 64 KiB block.
fn parse_one_block(
    combined: &[u8],
    block: std::ops::Range<usize>,
    finder: &mut match_finder::TreeMatchFinder,
    short: &mut ShortFinder,
    search: &mut SearchState,
    state: &mut EncoderMatchState,
    spec: super::BlockSearch<'_>,
    buffers: &mut ParseBuffers,
) -> Vec<Symbol> {
    let limits = spec.limits;
    let max_match = limits.max_match;
    let window = limits.window;
    let variant = limits.variant;
    let ParseBuffers {
        matches,
        arrays,
        tables,
    } = buffers;
    // The fused path never uses the block's candidate list: it searches one
    // position at a time and jumps over the bytes a match covers.
    if interleave_config() {
        let parse_started = Instant::now();
        let tokens = interleaved_tokens(
            finder,
            tables,
            combined,
            &block,
            state,
            limits,
            spec.chain_len,
        );
        crate::codec::modern::lzss_huff::encoder::profile::add(
            crate::codec::modern::lzss_huff::encoder::profile::PARSE_DP,
            parse_started,
        );
        let convert_started = Instant::now();
        let (symbols, _) = convert_tokens(&tokens, combined, block, state, variant);
        crate::codec::modern::lzss_huff::encoder::profile::add(
            crate::codec::modern::lzss_huff::encoder::profile::PARSE_CONVERT,
            convert_started,
        );
        return symbols;
    }
    let collect_started = Instant::now();
    collect_block_matches(
        finder,
        short,
        search,
        combined,
        block.clone(),
        spec,
        matches,
    );
    crate::codec::modern::lzss_huff::encoder::profile::add(
        crate::codec::modern::lzss_huff::encoder::profile::PARSE_COLLECT,
        collect_started,
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
    if !DISABLE_MATCHLESS_FAST_PATH.load(Ordering::Relaxed) {
        let longest = matches
            .runs
            .iter()
            .map(|&(l, _)| l as usize)
            .max()
            .unwrap_or(0);
        // Strict matchless: not a single candidate. Relaxed matchless: only
        // trivially-short candidates (<= RELAXED_MATCHLESS_MAX_LEN == the
        // collector's length floor) and no live repeat distance. In both the
        // pricing passes resolve to all-literals — accidental 4-byte
        // hash-collision matches on incompressible data can never beat four
        // literals, and a dead repeat cache means no beneficial cached-distance
        // match either — so skipping the DP is byte-identical (proven by
        // `DISABLE_MATCHLESS_FAST_PATH` + the matchless_fast_path_is_byte_identical
        // test). Random data has ~10^-4 4-byte collisions per position, so this
        // fires for essentially every block there and recovers the DP cost
        // otherwise spent over noise.
        let strict = matches.runs.is_empty();
        // A 2- or 3-byte candidate is a *priced* option now (the parse takes
        // it where `SlotPricer` finds it cheaper than the literals it covers),
        // so the relaxed gate must exclude blocks that hold one.
        let has_short = matches.runs.iter().any(|&(l, _)| l < 4);
        let relaxed = !strict
            && !has_short
            && longest <= RELAXED_MATCHLESS_MAX_LEN
            && state.reps.iter().take(2).all(|&r| r == 0);
        if strict || relaxed {
            let span = block.end - block.start;
            let mut all_literal = true;
            if strict {
                // With no live cached distance the repeat probes are no-ops at
                // every position, so the all-literal conclusion needs no
                // per-position check at all — the common case on incompressible
                // data (no match has ever been emitted, so the entry reps stay
                // zero from the member head). A live rep re-arms the
                // per-position probe loop, which the pricing pass would also run.
                let reps_live = state.reps.iter().take(2).any(|&r| r != 0);
                all_literal = !reps_live;
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
            }
            if all_literal {
                #[cfg(test)]
                super::super::MATCHLESS_FAST_PATH_USES.fetch_add(1, Ordering::Relaxed);
                let mut symbols = Vec::with_capacity(span);
                for index in 0..span {
                    symbols.push(Symbol::Literal(combined[block.start + index]));
                }
                // Literals leave the repeat cache and last-length exactly as
                // the pricing passes would have.
                return symbols;
            }
        }
    }

    if lazy_config() {
        let parse_started = Instant::now();
        let tokens = lazy_tokens(combined, &block, matches, state, limits);
        crate::codec::modern::lzss_huff::encoder::profile::add(
            crate::codec::modern::lzss_huff::encoder::profile::PARSE_DP,
            parse_started,
        );
        let convert_started = Instant::now();
        let (symbols, _) = convert_tokens(&tokens, combined, block, state, variant);
        crate::codec::modern::lzss_huff::encoder::profile::add(
            crate::codec::modern::lzss_huff::encoder::profile::PARSE_CONVERT,
            convert_started,
        );
        return symbols;
    }

    let initial = *state;
    let parse_started = Instant::now();
    let tokens = optimal_parse_tokens(
        combined,
        block.clone(),
        limits,
        None,
        &matches,
        initial,
        arrays,
    );
    crate::codec::modern::lzss_huff::encoder::profile::add(
        crate::codec::modern::lzss_huff::encoder::profile::PARSE_DP,
        parse_started,
    );
    let convert_started = Instant::now();
    let (symbols, _) = convert_tokens(&tokens, combined, block.clone(), state, variant);
    crate::codec::modern::lzss_huff::encoder::profile::add(
        crate::codec::modern::lzss_huff::encoder::profile::PARSE_CONVERT,
        convert_started,
    );
    symbols
}
/// Whether the block parser interleaves search and parse, skipping the bytes
/// a match covers (`RAR_RS_INTERLEAVE`).
fn interleave_config() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("RAR_RS_INTERLEAVE")
            .map(|value| {
                let value = value.trim();
                !(value.is_empty() || value == "0")
            })
            .unwrap_or(false)
    })
}

/// The official's main loop, fused: search one position, decide with the
/// previous position's pending match, emit, and **advance by the token length**
/// so the bytes a match covers are never searched.
///
/// WinRAR does exactly this (`FUN_140044794`: `pos += best_length` after
/// `FUN_1400478e0` parks the match), which is where its per-file advantage comes
/// from: its per-position work is no cheaper than ours, it just visits about
/// half the positions. The cost is that skipped positions are never inserted
/// into the tree, so later matches lose those candidates — the ratio price this
/// knob measures.
fn interleaved_tokens(
    finder: &mut match_finder::TreeMatchFinder,
    tables: &mut ShortTables,
    combined: &[u8],
    block: &std::ops::Range<usize>,
    state: &EncoderMatchState,
    limits: super::MatchLimits,
    chain_len: usize,
) -> Vec<(u32, u32)> {
    let window = limits.window;
    let max_match = limits.max_match;
    let min_match = min_match_config();
    let mut reps = state.reps;
    let mut last_length = state.last_length;
    // The official's three recent-occurrence tables (`+0xb1e8` byte,
    // `+0xb1f0` ushort, `+0xb1f8` u32 in `FUN_14004115c`). Its precompute pass
    // fills them for *every* position, including the ones the main loop skips,
    // which is how it keeps short-match coverage while emitting long matches
    // greedily. Insertion is one store per table, so we can afford the same.
    let mut tokens: Vec<(u32, u32)> = Vec::with_capacity(block.end - block.start);
    let mut scratch: Vec<(u32, u32)> = Vec::with_capacity(16);
    // Ablation switches, hoisted: reading the environment per position costs
    // more than what it is supposed to measure.
    let use_rep = std::env::var_os("RAR_RS_NOREP").is_none();
    let use_tables = std::env::var_os("RAR_RS_NOTABLES").is_none();
    let use_tree = std::env::var_os("RAR_RS_NOTREE").is_none();
    // Greedy: emit the match at once instead of parking it for one position to
    // see whether the next position extends it. The official's loop is greedy
    // (`FUN_140044794`: `pos += best_len`), and the per-token search count is
    // what separates its 3.4 M searches from our 5.8 M.
    let greedy = std::env::var_os("RAR_RS_GREEDY").is_some();
    // Pick by length (nearest wins ties) instead of by estimated price: the
    // official records a candidate only when it is longer than the best so far
    // and accepts a farther one only when it is longer (`dist >> 5`).
    let pick_longest = std::env::var_os("RAR_RS_PICKLONG").is_some();
    let fused_cut = std::env::var("RAR_RS_FUSED_CUT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(chain_len);
    // The official precomputes the short-match tables for the whole window
    // before searching; keeping the two `last` maps hot in L2 is the point of
    // doing the pass separately instead of on the way through the loop.
    if use_tables {
        tables.prepare(combined, block);
    }
    let mut pos = block.start;
    // The previous position's match, deferred one step: the official calls this
    // `+0x300dc` / `+0x300d0`.
    let mut pending: Option<(u32, u32)> = None;
    while pos < block.end {
        super::super::profile::count(super::super::profile::POS, 1);
        let max_distance = pos.min(window);
        let available = (block.end - pos).min(max_match);
        let mut best = (0u32, 0u32);
        let mut cands: Vec<(u32, u32)> = Vec::with_capacity(8);
        let offset = pos - block.start;
        if max_distance > 0 && available >= 2 && pos + 2 <= combined.len() {
            let len_limit = available.min(combined.len() - pos);
            // Short candidates first, with the official's distance bounds: a
            // 2-byte match only inside 0x100, 3 bytes inside 0x2000, each
            // demanding to be much closer than whatever is already held
            // (`dist < best_dist << 7` in the original).
            if use_tables && len_limit >= 2 {
                let distance = tables.prev2[offset] as usize;
                if distance != 0 && distance <= 0x100 && distance <= max_distance {
                    let length = match_length_at(combined, pos, distance, len_limit.min(4)) as u32;
                    if length >= 2 {
                        cands.push((length, distance as u32));
                    }
                }
            }
            if use_tables && len_limit >= 3 {
                let distance = tables.prev3[offset] as usize;
                if distance != 0
                    && distance <= 0x2000
                    && distance <= max_distance
                    && (best.0 < 2 || (distance as u64) < (best.1 as u64) << 7)
                {
                    let length = match_length_at(combined, pos, distance, len_limit.min(8)) as u32;
                    if length >= 3 {
                        cands.push((length, distance as u32));
                    }
                }
            }
            if use_tree && len_limit >= 4 {
                // Prefetch the bucket we will descend in sixteen positions, as the
                // serial collector does: the fused loop keeps the short tables and
                // the emitted tokens in flight, which evicts the head table.
                if pos + 32 + 4 < combined.len() {
                    finder.prefetch_head_for(combined, pos + 32);
                }
                if pos + 16 + 4 < combined.len() {
                    finder.prefetch_node_for(combined, pos + 16);
                }
                scratch.clear();
                super::super::profile::count(super::super::profile::SEARCH, 1);
                let cut = if len_limit >= NICE_MATCH_LENGTH {
                    fused_cut
                } else {
                    fused_cut.min(4)
                };
                finder.matches(match_finder::MatchQuery {
                    input: combined,
                    pos,
                    len_limit,
                    max_distance,
                    cut,
                    out: &mut scratch,
                });
                for &(length, distance) in scratch.iter() {
                    let encodable = reps.contains(&distance)
                        || length >= length_bonus(distance).saturating_add(2);
                    if encodable {
                        cands.push((length, distance));
                    }
                }
            }
            let mut pick = (1u32, 0u32);
            for &repeat in reps.iter() {
                let repeat_len = repeat as usize;
                if repeat_len == 0 || repeat_len > max_distance || repeat == best.1 {
                    continue;
                }
                let length = match_length_at(combined, pos, repeat_len, available) as u32;
                if length > pick.0 && length >= 2 {
                    pick = (length, repeat);
                }
            }
            if pick.0 > 1 {
                cands.push(pick);
            }
            // Pick by *price*, not by length: the official never takes the
            // longest candidate (`FUN_140047698`), it takes the one whose saved
            // bits beat the distance it costs.
            for &(length, distance) in cands.iter() {
                if pick_longest {
                    if length > best.0 || (length == best.0 && distance < best.1) {
                        best = (length, distance);
                    }
                    continue;
                }
                let score = (length as i64) * 8 - price_of(distance);
                let held = (best.0 as i64) * 8 - if best.0 == 0 { 0 } else { price_of(best.1) };
                if best.0 == 0 || score > held {
                    best = (length, distance);
                }
            }
        }
        // The official's rep rule (`FUN_140047698`): a distance from the cache
        // costs no distance slot, so it is taken whenever it is nearly as long
        // as the best match found (`len + 3 >= best_len`) or proportionally
        // longer (`len * 4 >= best_len * 3`). When it is one to three bytes
        // shorter, the byte it gives up is only worth it if the alternative's
        // distance is past the matching distance class (255/2047/32767).
        if best.0 >= 2 && best.0 < 12 && available >= 2 && use_rep && use_tree {
            // Only the first `best + 4` bytes can change the verdict below, so
            // the scan stops there: a rep match is often thousands of bytes long
            // and comparing all of it costs more than the whole decision.
            let len_limit = available.min(combined.len() - pos).min(best.0 as usize + 4);
            let mut rep_len = 0u32;
            let mut rep_dist = 0u32;
            for &distance in reps.iter().take(4) {
                let distance = distance as usize;
                if distance == 0 || distance > max_distance || distance > pos {
                    continue;
                }
                let length = match_length_at(combined, pos, distance, len_limit) as u32;
                if length <= 1 || !(length + 3 >= best.0 || best.0 * 3 <= length * 4) {
                    continue;
                }
                if (length + 1 == best.0 && best.1 <= 0xff)
                    || (length + 2 == best.0 && best.1 <= 0x7ff)
                    || (length + 3 == best.0 && best.1 <= 0x7fff)
                {
                    continue;
                }
                if length > rep_len {
                    rep_len = length;
                    rep_dist = distance as u32;
                }
            }
            if rep_len > 0 {
                best = (rep_len, rep_dist);
            }
        }
        // Undo the match the previous position was holding, if this one beats
        // it: emit that byte as a literal and keep this match pending. That is
        // the whole of the official's laziness.
        if let Some((pending_len, pending_dist)) = pending
            && best.0 >= 2
            && best.0 > pending_len
            && !extends_at(combined, pos, pending_len, pending_dist)
        {
            tokens.push((0, u32::from(combined[pos - 1])));
            pending = Some(best);
            pos += 1;
            continue;
        }
        let best = if best.0 >= min_match { best } else { (0, 0) };
        match pending.take() {
            Some((length, distance)) => {
                tokens.push((length, distance));
                if !(distance == reps[0] && length == last_length) {
                    if let Some(index) = reps.iter().position(|&d| d == distance) {
                        reps[..=index].rotate_right(1);
                    } else {
                        reps.rotate_right(1);
                    }
                    reps[0] = distance;
                    last_length = length;
                }
                // Skip the bytes the match covers (the official's `pos += len`);
                // the match started at `pos - 1`, where it was parked.
                pos = pos - 1 + length as usize;
                debug_assert!(pos <= block.end, "parked match overran its block");
            }
            None => {
                if best.0 >= 2 && greedy {
                    // Emit now: the parked form would visit the next position
                    // only to re-derive the same match and then skip it.
                    let (length, distance) = best;
                    tokens.push(best);
                    if !(distance == reps[0] && length == last_length) {
                        if let Some(index) = reps.iter().position(|&d| d == distance) {
                            reps[..=index].rotate_right(1);
                        } else {
                            reps.rotate_right(1);
                        }
                        reps[0] = distance;
                        last_length = length;
                    }
                    pos += length as usize;
                } else if best.0 >= 2 {
                    pending = Some(best);
                    pos += 1;
                } else {
                    tokens.push((0, u32::from(combined[pos])));
                    pos += 1;
                }
            }
        }
    }
    if let Some((length, distance)) = pending {
        tokens.push((length, distance));
    }
    tokens
}

/// Whether the block parser runs the official's one-step lazy walk instead of
/// the price-driven DP (`RAR_RS_LAZY`).
fn lazy_config() -> bool {
    static LAZY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LAZY.get_or_init(|| {
        std::env::var("RAR_RS_LAZY")
            .map(|value| {
                let value = value.trim();
                !(value.is_empty() || value == "0")
            })
            .unwrap_or(false)
    })
}

/// One-step lazy walk over the block's candidates, shaped after WinRAR's
/// `FUN_1400478e0` + `FUN_140047698`: the longest candidate wins, a cached
/// distance only takes over when it is nearly as long, and the tolerances sit
/// on the distance-code boundaries (255, 2047, 32767) so a one/two/three-byte
/// shorter match is rejected only when the current best is far enough to cost
/// more than it saves.
///
/// Whether the lazy walk prefers the nearest nearly-as-long candidate
/// (`RAR_RS_LAZY_NEAR`), which is what WinRAR's distance rules encode. Off
/// by default: over a candidate list ordered by length it costs ratio (measured
/// 49.27 % -> 50.18 % on ntoskrnl m3), because the nearest report is usually a
/// four-byte one.
fn near_first() -> bool {
    static NEAR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NEAR.get_or_init(|| {
        std::env::var("RAR_RS_LAZY_NEAR")
            .map(|value| {
                let value = value.trim();
                !(value.is_empty() || value == "0")
            })
            .unwrap_or(false)
    })
}

/// One-step lazy walk over the block's candidates, shaped after WinRAR's
/// `FUN_1400478e0` + `FUN_140047698`: the longest candidate wins, a cached
/// distance only takes over when it is nearly as long, and the tolerances sit
/// on the distance-code boundaries (255, 2047, 32767) so a one/two/three-byte
/// shorter match is rejected only when the current best is far enough to cost
/// more than it saves.
///
/// This exists as a measurement of the official's parse, not as a candidate
/// default: the DP buys 3.5 pp of ratio for ~0.8 s, and the lazy walk needs a
/// candidate set ordered by distance class to be worth revisiting (ADR 0011).
fn lazy_tokens(
    combined: &[u8],
    block: &std::ops::Range<usize>,
    matches: &BlockMatches,
    state: &EncoderMatchState,
    limits: super::MatchLimits,
) -> Vec<(u32, u32)> {
    let window = limits.window;
    let max_match = limits.max_match;
    let mut reps = state.reps;
    let mut last_length = state.last_length;
    let mut tokens: Vec<(u32, u32)> = Vec::with_capacity(block.end - block.start);
    let mut pos = block.start;
    while pos < block.end {
        let max_distance = pos.min(window);
        let max_length = (block.end - pos).min(max_match);
        let mut best = (0u32, 0u32);
        if max_distance > 0 && max_length >= 4 {
            for &(length, distance) in matches.at(pos - block.start) {
                // A fresh distance must still be long enough to pay for its
                // distance code, which is what `convert_tokens` demands.
                let encodable =
                    reps.contains(&distance) || length >= length_bonus(distance).saturating_add(2);
                if encodable && length > best.0 {
                    best = (length, distance);
                }
            }
            if near_first() {
                // WinRAR's parse prefers the nearest candidate that is
                // nearly as long, but only when one qualifies: falling back
                // to the longest is what keeps a strict tolerance from
                // turning a real match into a literal.
                let longest = best;
                best = (0, 0);
                for &(length, distance) in matches.at(pos - block.start) {
                    let encodable = reps.contains(&distance)
                        || length >= length_bonus(distance).saturating_add(2);
                    if !encodable {
                        continue;
                    }
                    if length * 4 >= longest.0 * 3 || length.saturating_add(3) >= longest.0 {
                        best = (length, distance);
                        break;
                    }
                }
                if best.0 == 0 {
                    best = longest;
                }
            }
            let mut pick = (1u32, 0u32);
            for &repeat in reps.iter() {
                let repeat_len = repeat as usize;
                if repeat_len == 0 || repeat_len > max_distance || repeat == best.1 {
                    continue;
                }
                let length = match_length_at(combined, pos, repeat_len, max_length) as u32;
                let near_enough = best.0.saturating_sub(3) <= length || best.0 * 3 <= length * 4;
                let cheaper = (length + 1 != best.0 || best.1 > 0xff)
                    && (length + 2 != best.0 || best.1 > 0x7ff)
                    && (length + 3 != best.0 || best.1 > 0x7fff);
                if near_enough && cheaper && length > pick.0 {
                    pick = (length, repeat);
                }
            }
            if pick.0 > 1 {
                best = pick;
            }
        }
        if best.0 >= 2 {
            tokens.push(best);
            // Mirror the cache update `convert_tokens` applies, so the rule
            // above sees the cache the emission will.
            if !(best.1 == reps[0] && best.0 == last_length) {
                if let Some(index) = reps.iter().position(|&d| d == best.1) {
                    reps[..=index].rotate_right(1);
                } else {
                    reps.rotate_right(1);
                }
                reps[0] = best.1;
                last_length = best.0;
            }
            pos += best.0 as usize;
        } else {
            tokens.push((0, combined[pos] as u32));
            pos += 1;
        }
    }
    tokens
}

#[cfg(test)]
#[cfg(test)]
mod lazy_tests {
    use super::*;

    fn limits(window: usize) -> super::super::MatchLimits {
        super::super::MatchLimits {
            max_match: 0x1001,
            window,
            variant: ArchiveVersion::V50,
        }
    }

    /// A block with no candidates is all literals, one per position.
    #[test]
    fn without_candidates_every_position_is_a_literal() {
        let data = b"abcdefghij";
        let matches = BlockMatches {
            runs: Vec::new(),
            starts: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        };
        let state = EncoderMatchState::new([0; DIST_CACHE_SIZE], 0);
        let tokens = lazy_tokens(data, &(0..10), &matches, &state, limits(1 << 16));
        assert_eq!(tokens.len(), 10);
        assert!(tokens.iter().all(|&(length, _)| length == 0));
    }

    /// A candidate is emitted whole and the walk skips the bytes it covers —
    /// which is where the official encoder's per-position cost goes.
    #[test]
    fn a_candidate_is_emitted_and_its_bytes_are_skipped() {
        // `abcabcabc` at 0 triggers nothing; position 6 matches distance 3.
        let data = b"abcabcabcXYZ";
        let matches = BlockMatches {
            runs: vec![(3, 3)],
            starts: vec![0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1],
        };
        let state = EncoderMatchState::new([0; DIST_CACHE_SIZE], 0);
        let tokens = lazy_tokens(data, &(0..12), &matches, &state, limits(1 << 16));
        assert_eq!(tokens[6], (3, 3), "the candidate at 6 owns the match");
        assert_eq!(tokens.len(), 6 + 1 + 3, "three match bytes, three literals");
        assert!(tokens[..6].iter().all(|&(length, _)| length == 0));
        assert!(tokens[7..].iter().all(|&(length, _)| length == 0));
    }

    /// `FUN_140047698`'s core rule: a cached distance that is nearly as long
    /// wins, and the tolerance is the distance-code boundary, not length.
    #[test]
    fn a_cached_nearer_distance_beats_a_slightly_longer_one() {
        // A run of 8 bytes at distance 40 and one of 9 at distance 400: the
        // near one is not cheaper (40 and 400 share a slot), so the longer
        // match must stay. With a distance past 0x7ff the near one wins.
        let data = vec![b'q'; 1024];
        let data = {
            let mut data = data;
            for (index, byte) in data.iter_mut().enumerate() {
                *byte = b"abcdefgh"[index % 8];
            }
            data
        };
        let block = 1000..1008;
        let mut matches = BlockMatches {
            runs: Vec::new(),
            starts: vec![0; 9],
        };
        matches.runs.push((8, 8));
        let mut state = EncoderMatchState::new([8; DIST_CACHE_SIZE], 0);
        state.reps[0] = 8;
        let tokens = lazy_tokens(&data, &block, &matches, &state, limits(1 << 16));
        assert_eq!(tokens[0], (8, 8), "the cached distance ties and wins");
    }
}

#[cfg(test)]
mod slot_pricer_tests {
    use super::*;

    /// A cheap deterministic generator: the test needs spread, not quality.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % bound
        }
    }

    /// Code lengths with the occasional zero, which is a real case: a block
    /// need not use every symbol its table has room for, and an unused symbol
    /// is priced at [`UNUSED_SYMBOL_COST`].
    fn code_lengths(rng: &mut Rng, len: usize) -> Vec<u8> {
        (0..len)
            .map(|_| {
                if rng.below(8) == 0 {
                    0
                } else {
                    1 + rng.below(MAX_CODE_LENGTH as u64) as u8
                }
            })
            .collect()
    }

    fn prices(rng: &mut Rng) -> TokenPrices {
        TokenPrices::new(
            &code_lengths(rng, HUFF_NC),
            &code_lengths(rng, HUFF_DCX),
            &code_lengths(rng, 16),
            &code_lengths(rng, HUFF_RC),
        )
    }

    /// Every shape the fused step has to reproduce: a run at the repeat
    /// distance with a remembered length inside it, a cached distance (no
    /// length bonus), and fresh distances at each bonus step — priced and
    /// unpriced, v50 and v70.
    #[test]
    fn step_matches_the_expanded_price() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // One distance per bonus step (`length_bonus` counts the thresholds).
        let candidates = [1u32, 0x101, 0x2001, 0x40001].map(|base| base + rng.below(64) as u32);
        let mut steps = 0u32;
        for _ in 0..4000 {
            let table = prices(&mut rng);
            let distance = candidates[rng.below(candidates.len() as u64) as usize];
            let other = 1 + rng.below(0x80) as u32;
            let mut reps = [0u32; DIST_CACHE_SIZE];
            match rng.below(3) {
                // The run's distance is the repeat distance.
                0 => reps[0] = distance,
                // Cached, but not necessarily the repeat distance.
                1 => reps[rng.below(DIST_CACHE_SIZE as u64) as usize] = distance,
                // Not cached at all.
                _ => {}
            }
            if reps[0] == 0 {
                reps[0] = other;
            }
            // Zero means no remembered length, as at a block start.
            let last_length = match rng.below(3) {
                0 => 0,
                _ => 4 + rng.below(160) as u32,
            };
            let state = EncoderMatchState::new(reps, last_length);
            let variant = if rng.below(2) == 0 {
                ArchiveVersion::V50
            } else {
                ArchiveVersion::V70
            };
            let plan = state.plan(distance, variant);
            let priced = SlotPricer::for_run(&state, distance, variant, Some(&table));
            let unpriced = SlotPricer::for_run(&state, distance, variant, None);
            let max_match = 0x1000;
            let run_start = 4 + rng.below(48) as u32;
            let run_end = run_start + rng.below(200) as u32;
            let mut length = run_start;
            while length <= run_end {
                let reach = same_price_run_end(&plan, length, max_match).min(run_end);
                assert_eq!(
                    priced.step(length, run_end),
                    (reach, table.match_cost(&plan, reach)),
                    "priced {length}..{run_end} at {distance}, reps {reps:?}, last {last_length}, {variant:?}"
                );
                assert_eq!(
                    unpriced.step(length, run_end),
                    (reach, estimated_match_cost(&plan, reach)),
                    "unpriced {length}..{run_end} at {distance}, reps {reps:?}, last {last_length}"
                );
                length = reach + 1;
                steps += 1;
            }
        }
        assert!(steps > 8_000, "only {steps} steps priced");
    }
}

/// Shortest match the fused path will emit (`RAR_RS_MINMATCH`, default 4).
fn min_match_config() -> u32 {
    static MIN: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("RAR_RS_MINMATCH")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .map(|value| value.clamp(2, 32))
            .unwrap_or(4)
    })
}

/// Rough bit price of a match at `distance`: the distance slot plus overhead.
fn price_of(distance: u32) -> i64 {
    let bits = 32 - (distance | 1).leading_zeros() as i64;
    8 + bits * 2
}

/// The official's three recent-occurrence tables (`+0xb1e8` byte, `+0xb1f0`
/// ushort, `+0xb1f8` u32 in `FUN_14004115c`), filled for every position and
/// shared across the blocks of one parse.
///
/// 0 means "no entry": position 0 can never be a match source for a position
/// that matters, so giving up its slot costs nothing and keeps the tables
/// calloc'd (`vec![0; n]`), which matters — they are 4.5 MB total.
#[derive(Default)]
pub(super) struct ShortTables {
    /// Distance back to the previous position holding the same 2-byte pair
    /// (`prev2`) or the same 3-byte hash (`prev3`), indexed by block offset.
    /// One sequential load yields the candidate and the pass that builds them
    /// writes sequentially too -- that is why `+0xb1e8`/`+0xb1f0` in the
    /// original are position indexed instead of hash buckets.
    prev2: Vec<u32>,
    prev3: Vec<u32>,
    /// Last position seen for each key; kept across blocks so a candidate can
    /// point into the tail.
    last2: Vec<u32>,
    last3: Vec<u32>,
}

impl ShortTables {
    fn prepare(&mut self, combined: &[u8], block: &std::ops::Range<usize>) {
        if self.last2.is_empty() {
            self.last2 = vec![u32::MAX; 1 << 16];
            self.last3 = vec![u32::MAX; 1 << 16];
        }
        self.prev2.clear();
        self.prev2.resize(block.len(), 0);
        self.prev3.clear();
        self.prev3.resize(block.len(), 0);
        for at in block.clone() {
            if at + 4 > combined.len() {
                continue;
            }
            let two = u16::from_le_bytes([combined[at], combined[at + 1]]) as usize;
            let key = u32::from_le_bytes([
                combined[at],
                combined[at + 1],
                combined[at + 2],
                combined[at + 3],
            ]);
            let hashed = (key.wrapping_mul(0x9E37_79B1) >> 16) as usize & 0xffff;
            let index = at - block.start;
            if let last = self.last2[two]
                && (last as usize) < at
            {
                self.prev2[index] = (at - last as usize) as u32;
            }
            if let last = self.last3[hashed]
                && (last as usize) < at
            {
                self.prev3[index] = (at - last as usize) as u32;
            }
            self.last2[two] = at as u32;
            self.last3[hashed] = at as u32;
        }
    }
}

/// Whether the match parked at `pos - 1` keeps growing at `pos`, in which case
/// the official keeps it instead of switching to the new position's match
/// (`FUN_1400478e0` compares `input[pos - 1 + len]` with `input[pos + len]`
/// before it defers).
fn extends_at(combined: &[u8], pos: usize, length: u32, distance: u32) -> bool {
    let length = length as usize;
    let distance = distance as usize;
    if distance == 0 || distance > pos - 1 {
        return false;
    }
    let copy = (pos - 1 - distance) + length;
    pos + length < combined.len()
        && copy < combined.len()
        && combined[copy] == combined[pos + length]
}
