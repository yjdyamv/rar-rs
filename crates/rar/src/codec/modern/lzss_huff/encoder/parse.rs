//! Match finding and symbol parsing.
//!
//! Two parsers share the same match finders (hash chain, BT4 tree, sampled
//! long range): the fast/lazy walker used by the low levels and the
//! price-driven optimal parser for levels 2-5. Both emit the [`Symbol`]
//! stream consumed by [`super::emit`]; blocks are closed early when the local
//! literal/match distribution drifts so each emitted block gets its own
//! Huffman tables.

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

use super::super::{
    HUFF_DC, HUFF_DCX, HUFF_LDC, HUFF_NC, HUFF_RC, MAX_CODE_LENGTH, SYM_CACHE_BASE, SYM_MATCH_BASE,
    SYM_REPEAT,
};
use super::emit::{
    apply_length_bonus, cache_find, cache_push, cache_touch, encode_distance_slot,
    encode_length_slot, ensure_nonzero, remove_length_bonus,
};
use crate::codec::common::huffman::build_code_lengths_from_freqs;
use crate::codec::common::match_finder::MatchFinder;
use crate::version::ArchiveVersion;

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

// ── Match finding ──────────────────────────────────────────────────────────

/// Find matches for `chunk`, searching against `state.tail` as lookbehind.
/// Advances `state` so a following chunk/file continues the LZ window.
/// When `long_range` is set, distances beyond the near window are found
/// through the sampled long-range history (WinRAR `-mcl` semantics).
pub(super) fn find_matches_with_tail(
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
#[allow(clippy::too_many_arguments)]
pub(super) fn windowed_priced_parse<F>(
    combined: &[u8],
    start: usize,
    end: usize,
    dist_cache: &mut [u32; DIST_CACHE_SIZE],
    last_length: &mut u32,
    max_match: usize,
    window: usize,
    variant: ArchiveVersion,
    lr: Option<(&match_finder::LongRange, usize, usize)>,
    mut emit: F,
) -> Vec<Symbol>
where
    F: FnMut(usize, &[u32; DIST_CACHE_SIZE], &mut Vec<(u32, u32)>),
{
    let mut symbols = Vec::with_capacity(end - start);
    let mut state = EncoderMatchState::new(*dist_cache, *last_length);
    let probe_cache = *dist_cache;
    let block_size = super::chunked::MT_ROW_INDEX_DP_BLOCK;
    let mut block_start = start;
    while block_start < end {
        let block_end = (block_start + block_size).min(end);
        let span = block_end - block_start;
        let mut matches = BlockMatches {
            runs: Vec::with_capacity(span),
            starts: Vec::with_capacity(span + 1),
        };
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
                max_match,
                window,
                variant,
                None,
                &matches,
                state,
            );
            let (block_symbols, _) = convert_tokens(
                &tokens,
                combined,
                block_start..block_end,
                &mut state,
                variant,
            );
            symbols.extend(block_symbols);
        }
        block_start = block_end;
    }
    *dist_cache = state.reps;
    *last_length = state.last_length;
    symbols
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
    pub(super) fn build(data: &[u8]) -> Self {
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
    pub(super) fn longest(
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
    pub(super) fn collect(
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
pub(super) const MT_DP_BLOCK_SIZE: usize = 256 * 1024;

/// Match-finding loop over `data[start..end]` with a distance cache.
/// `lr` (when present) adds long-range candidates from the sampled
/// history: `(long_range, near_max)` where `near_max` is the largest
/// distance the near finder can produce (tail + chunk), so long-range
/// hits are only considered beyond it.
#[allow(clippy::too_many_arguments)]
pub(super) fn find_matches_in_range(
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
///
/// Two roles, both experiment seams below: the finder stops comparing bytes at
/// this length (a match that reaches it is measured out to its real end
/// afterwards), and the parse stops pricing positions it covers.
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
/// drops to the [`FAST_RECOVER_INTERVAL`] cadence, **by level**: this is the
/// one shortcut that measurably moved ratio when relaxed, and it is why the
/// m3-m5 ladder used to be flat.
///
/// A probe fails only when the tree found *no match at all* (not merely a short
/// one): on text-like data 4-15 byte matches are real signal (word prefixes) and
/// must keep the full search cadence, while on truly incompressible data a
/// 4-byte hash-collision match is ~2^-32 per position, so the miss run still
/// accumulates and the mode engages after a couple of KiB of wasted
/// cache-missing descents into the multi-MiB son array.
///
/// The gate is meant for the incompressible case, but on a **dense binary** it
/// also truncates the runs where a long match is about to appear, dropping
/// candidates at every level. Measured on the 12.5 MiB DLL at `-mt1` (dict 32m):
/// threshold 256 (today) 5,751,821 B / 6184 ms, 1024 5,742,924 B, 4096
/// 5,739,533 B / 6608 ms, and beyond 4096 nothing on that member (10 B). m5 takes
/// 4096 rather than "never" because the *time* is not flat there: with the gate
/// off entirely a 6 MiB XML member went from 625 ms to 24 s, since the gate also
/// stops inserting the positions it steps over and a dense tree makes every
/// later descent more expensive. m1-m3 keep 256 — which is why the default
/// level's bytes do not move at all — and m4 takes 1024.
pub(super) const COLLECT_MISS_THRESHOLD: [usize; 6] = [0, 256, 256, 256, 1024, 4096];

/// Test seam: force the full pricing passes even for matchless blocks, to
/// prove the matchless fast path is byte-identical.
static DISABLE_MATCHLESS_FAST_PATH: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn set_fast_path_enabled(enabled: bool) {
    DISABLE_MATCHLESS_FAST_PATH.store(!enabled, Ordering::Relaxed);
}

/// Relaxed matchless fast path: a block whose longest candidate match is at
/// most this many bytes (and that has no live repeat distance) parses to
/// all-literals, exactly like the strict matchless path. The collector only
/// ever reports matches of length >= 4 (`collect_block_matches`), so the value
/// below separates *accidental* 4-byte hash-collision matches (incompressible
/// data, where a 4-byte match can never beat four literals) from real signal.
/// Raising it would also skip genuinely useful short matches on compressible
/// data, so keep it at the collector's floor.
const RELAXED_MATCHLESS_MAX_LEN: usize = 4;
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
pub(super) const OPTIMAL_PARSE_PASSES: [usize; 6] = [0, 0, 2, 2, 3, 4];

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

/// One length's worth of a [`MatchDistancePlan`], mirroring the three shapes
/// the writer can emit.
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
fn slot_extra_bits(slot: usize) -> u32 {
    if slot < 8 { 0 } else { (slot / 4 - 1) as u32 }
}

/// Estimated bit cost of a match before any block has been priced (rars
/// `estimated_match_cost`). `None` when the match cannot be encoded.
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

    /// Bits to code a match of `length` bytes at the plan's distance.
    fn match_cost(&self, plan: &MatchDistancePlan, length: u32) -> Option<u32> {
        Some(match plan.shape(length)? {
            MatchShape::Repeat => self.repeat_cost,
            MatchShape::Cache { index, len_slot } => {
                self.cache_cost[index] + self.cache_len_cost[len_slot]
            }
            MatchShape::New { len_slot, fresh } => {
                let distance_bits = if fresh.dbits >= 4 {
                    fresh.dbits as u32 - 4 + self.ldc_cost[(fresh.dist_extra & 0xF) as usize]
                } else {
                    fresh.dbits as u32
                };
                self.new_len_cost[len_slot] + self.dc_cost[fresh.dist_slot] + distance_bits
            }
        })
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
    // Per-level dial: when the search drops to the recovery cadence.
    miss_threshold: usize,
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
    // The next gated position's first tree step (its head-resolved
    // descendant and that node's child pair), computed at the end of the
    // preceding iteration so its DRAM reads overlap the bookkeeping.
    let mut pending: Option<(usize, usize, u32, u32)> = None;
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
            // Warm the next position's head slot while this descent runs;
            // the seed computed at the tail then hits. Harmless when the
            // next position ends up unseeded/skipped.
            if pos + 4 < combined.len() {
                finder.prefetch_head_for(combined, pos + 1);
            }
            let avail = combined.len() - pos;
            let len_limit = avail.min(NICE_MATCH_LENGTH);
            scratch.clear();
            match pending.take() {
                Some((seed_pos, current, less, greater)) if seed_pos == pos => {
                    finder.matches_seeded(
                        combined,
                        pos,
                        len_limit,
                        max_distance,
                        chain_len,
                        &mut scratch,
                        current,
                        less,
                        greater,
                    );
                }
                _ => finder.matches(
                    combined,
                    pos,
                    len_limit,
                    max_distance,
                    chain_len,
                    &mut scratch,
                ),
            }
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
            if !fast_tree && tree_misses >= miss_threshold {
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
                if !lr_fast && lr_misses >= miss_threshold {
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
        // Seed the next position's first tree step. Its values are settled
        // now (every position through `pos` has inserted and this block does
        // nothing else to the tree), so reading them here is byte-identical
        // to reading them at the next turn. Mirror the gate exactly using
        // the just-updated commit/fast-tree state; the gate the next
        // iteration evaluates reads the same values.
        let npos = pos + 1;
        let n_max_distance = npos.min(window);
        let n_max_length = (block.end - npos).min(max_match);
        if npos >= committed_through
            && n_max_distance > 0
            && n_max_length >= 4
            && npos + 3 < combined.len()
            && (!fast_tree || (npos & (FAST_RECOVER_INTERVAL - 1)) == 0)
        {
            let (current, less, greater) = finder.seed_for(combined, npos);
            pending = Some((npos, current, less, greater));
        } else {
            pending = None;
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
    prices: Option<&TokenPrices>,
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
            // The distance side of every candidate in this run is the same,
            // so classify it once: the per-length step below only resolves
            // the length slot (and the repeat length's exact match).
            let plan = state.plan(distance, variant);
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
                    Some(prices) => prices.match_cost(&plan, reach),
                    None => estimated_match_cost(&plan, reach),
                };
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
/// useful matches: the tree head is still cleared, the parse inserts the
/// chunk's own positions, and within-chunk plus long-range matches are
/// unaffected — only the wasted fresh-tree seeding of a random tail is
/// skipped. (The multi-threaded workers do not reach this function; they
/// run the low-step chain tier in [`mt_slice_symbols_low_step`].)
#[allow(clippy::too_many_arguments)]
pub(super) fn find_matches_optimal(
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
    // Per-level parse dial (see [`COLLECT_MISS_THRESHOLD`]).
    miss_threshold: usize,
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
    let mut tree_finder = state.tree.get_or_insert_with(|| {
        // The finder spans the whole combined slice, so the slider's
        // rebase/trim never has to copy the son array mid-chunk.
        match_finder::TreeMatchFinder::new(tree_window)
    });
    tree_finder.grow_to(tree_window);
    // Lever-4 probe: `RAR_RS_FAR_BAND=start,cut` caps how many descent
    // steps may be spent on candidates farther than `start` bytes back, per
    // the probe on TreeMatchFinder. Experimental speed-vs-ratio switch,
    // off by default; removed once the experiment settles.
    if let Ok(v) = std::env::var("RAR_RS_FAR_BAND") {
        let mut parts = v.split(',').map(|s| s.trim().parse::<usize>());
        if let (Some(Ok(start)), Some(Ok(cut))) = (parts.next(), parts.next()) {
            tree_finder.with_far_band(start, cut);
        }
    }
    let keep = window.min(NEAR_WINDOW_MAX).min(combined.len());
    // `combined_len == 0` marks a fresh frame: the first chunk of a member
    // (the multi-threaded workers no longer reach here — they run the
    // low-step chain tier; the tree is sequential-only now). In that case
    // the head must be cleared — the tree may hold links from an earlier
    // frame — and the tail seeded. A continued frame instead rebases the
    // links by the slide amount, keeping the tail's positions valid without
    // re-seeding.
    if state.combined_len == 0 {
        tree_finder.clear_head();
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
                tree_finder.matches(&combined, pos, 4, tree_window, budget, &mut seed);
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
                miss_threshold,
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
            miss_threshold,
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
    // Per-level dial (see [`COLLECT_MISS_THRESHOLD`]).
    miss_threshold: usize,
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
        miss_threshold,
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
        let relaxed = !strict
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
        let prices = TokenPrices::new(&nc_l, &dc_l, &ldc_l, &rc_l);
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

/// Cap for grouping parsed symbols into *emitted* blocks. The RAR5 size
/// field allows blocks up to 4 GiB, so this is purely an encoder choice:
/// on distribution-stable data (repetitive text) merging many parse blocks
/// into one emitted block amortises the per-block Huffman table definitions
/// (WinRAR writes one block per whole member there); on heterogeneous data
/// the tables stay per-parse-block because the drift check keeps the parse
/// blocks small. Only the emitted grouping is larger — the parse itself is
/// unchanged, so token choices are byte-identical to the 128 KiB cap.
///
/// This is the one emitted-block policy every encode pipeline uses (plain
/// and filtered, sequential and multi-threaded); [`find_block_end_adaptive`]
/// is its splitter.
pub(super) const EMITTED_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Group symbols into emitted blocks of up to `cap` uncompressed bytes, but
/// close the block early when the symbol stream's *local* literal/match
/// distribution drifts between adjacent ~64 KiB sub-spans.
///
/// The parse-side [`BlockSplitter`] compares each sub-block against the cumulative
/// counts of the open block, which cannot see section boundaries once the
/// cumulative mix stabilises (a DLL's code+data+padding blend looks stable
/// over a 1 MiB span). Comparing each sub-span against the *previous* one
/// catches those boundaries: repetitive text stays merged (WinRAR writes
/// one block per member there), heterogeneous binaries keep small blocks
/// (WinRAR's ~64 KiB DLL blocks). The token stream itself is untouched —
/// only the emitted grouping changes, so parsers and decoders behave the
/// same.
pub(super) fn find_block_end_adaptive(
    symbols: &[Symbol],
    start: usize,
    cap: usize,
) -> (usize, usize) {
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
