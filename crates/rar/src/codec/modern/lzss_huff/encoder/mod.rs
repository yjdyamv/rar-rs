//! RAR5/RAR7 (v70) LZSS+Huffman encoder.
//!
//! Role split:
//! - [`chunked`] — public raw entry points and the chunk/MT pipeline,
//! - [`filter`] — pre-compression VM filters (delta, E8, E8E9, ARM),
//! - [`parse`] — match finding and the lazy/optimal symbol parsers,
//! - [`emit`] — block serialisation (Huffman tables, symbols, checksum).
//!
//! Shared vocabulary ([`EncoderState`], [`FilterSpec`], [`Symbol`]) lives
//! here; constants shared by several role modules live with the role that
//! owns them ([`parse`] owns the parser and the emitted-block policy).
//!
//! Clean-room implementation for software conservation and educational
//! purposes.
//!
//! License: BSD-2-Clause

mod chunked;
mod emit;
mod filter;
mod parse;
#[cfg(test)]
mod tests;

pub(crate) use chunked::EncodeSpec;
#[cfg(feature = "parallel")]
pub(crate) use chunked::encode_chunked_mt_with_progress;
pub(crate) use chunked::encode_chunked_raw_with_lead;
pub use chunked::{
    DEFAULT_CHUNK_SIZE, encode_chunked_mt, encode_chunked_raw, encode_raw, encode_with_progress_raw,
};
pub use filter::{
    FILTER_PROBE_LEN, FilterProbe, MAX_FILTER_BLOCK_LENGTH, encode_with_auto_delta_filter,
    encode_with_auto_x86_filter, encode_with_filters, encode_with_filters_continue,
    encode_with_filters_mt, filter_transform_wins, forced_combined_specs, member_filter_probes,
    pick_delta_channel,
};
pub(crate) use filter::{
    delta_stream_window, forced_combined_stream_window, merge_ranges, solid_filter_specs,
    x86_stream_window,
};
#[cfg(test)]
pub(crate) use parse::set_fast_path_enabled;
#[cfg(all(test, feature = "parallel"))]
pub(crate) use parse::set_price_carry_enabled;

/// How many times the matchless fast path returned literals without running
/// the pricing passes. Test seam: a byte-identity check whose corpus never
/// triggers the path would pass vacuously, so the default-suite smoke test
/// asserts this moved (`mt_tests` covers the full corpus matrix).
#[cfg(test)]
pub(crate) static MATCHLESS_FAST_PATH_USES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

use super::DIST_CACHE_SIZE;
use crate::codec::common::match_finder;

// ── Shared parameters ──────────────────────────────────────────────────────

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

/// Cut budget for the experimental priced tier, or `None` (the default) to
/// keep the level's own parameters.
///
/// Issue 15, lever B.3 asked for a priced rung between `-m1` and `-m2`. The
/// measured answer is that the rung is not a different *pricer* — the DP
/// costs the same at every budget — it is a different *collector budget*:
/// with the long-range table doing the far work, four candidates per
/// position still carry most of `-m2`'s ratio, and the priced parse stops
/// being a 10-40x jump over `-m1`.
///
/// It changes the packed bytes by design, so it can only ever be opt-in.
/// Promote it to a documented option (or delete it) once the numbers in
/// `docs/issues/compression-perf/15-fl2-zstd-parser-tiers.md` settle.
fn price_tier_cut() -> Option<usize> {
    #[cfg(test)]
    {
        let forced = PRICE_TIER_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
        if forced != PRICE_TIER_OFF {
            return Some(forced);
        }
    }
    static CUT: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CUT.get_or_init(|| {
        std::env::var("RAR_RS_SEQ_PRICE_TIER")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|&cut| cut >= 1)
    })
}

/// [`price_tier_cut`] reads the environment once per process, which a test
/// cannot vary; this seam forces the value instead (the fast-path switch uses
/// the same shape). `PRICE_TIER_OFF` means "no override".
#[cfg(test)]
pub(crate) static PRICE_TIER_OVERRIDE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(PRICE_TIER_OFF);

#[cfg(test)]
const PRICE_TIER_OFF: usize = usize::MAX;

#[cfg(test)]
pub(crate) fn set_price_tier_cut(cut: Option<usize>) {
    PRICE_TIER_OVERRIDE.store(
        cut.unwrap_or(PRICE_TIER_OFF),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Parse tier for a level: `(chain_len, passes, miss_threshold)`.
///
/// Both parser entry points — the buffered chunked path and the filtered one
/// ([`encode_with_filters`](filter::encode_with_filters), which serves every
/// member that gets an automatic x86/delta filter) — must take these from
/// here. Keeping a second copy of the level lookup in the filtered path is how
/// a tier ends up half-applied: binaries move, text does not, and the A/B
/// quietly compares two different parsers.
fn parse_tier(method: u8) -> (usize, usize, usize) {
    let level = (method as usize).clamp(1, 5);
    let (chain_len, _, _) = LEVEL_PARAMS[level];
    let miss_threshold = parse::COLLECT_MISS_THRESHOLD[level];
    match price_tier_cut() {
        // Level 1 parses greedily and has no price table to work from, so the
        // tier cannot override it without turning it into level 2.
        Some(cut) if level >= 2 => (cut, 1, miss_threshold),
        _ => (
            chain_len,
            parse::OPTIMAL_PARSE_PASSES[level],
            miss_threshold,
        ),
    }
}

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
    /// A filter covering `[block_start, block_start + block_length)` with
    /// `channels` interleaved streams.
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
    /// Cached hash-chain finder arrays (head/prev) for the MT low-step
    /// parse, reused across slices. The `combined` frame is reallocated per
    /// slice (the finder borrows it), but the two multi-MiB `head`/`prev`
    /// arrays survive in [`match_finder::MatchFinder::reuse`]/[`match_finder::MatchFinder::into_parts`] — re-arming
    /// a warm ring beats a fresh allocation + memset per slice; the random-
    /// data A/B regression of the low-step tier is the chain's per-byte
    /// insert, not this allocation (see issue 13).
    chain_parts: Option<(Vec<i32>, Vec<i32>)>,
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
        self.chain_parts = None;
    }

    /// Start a new member on this state.
    ///
    /// The persistent tree stores links as offsets into the *frame* it was
    /// built for (`tail + chunk`), so it only survives inside one frame
    /// sequence: the next member slides that frame away, and rebasing assumes
    /// a continuation that does not hold across a member boundary (measured:
    /// the third member of a solid chain silently stopped finding matches at
    /// all). Dropping it costs one re-seed per member and nothing else — the
    /// window tail, repeat cache and long-range history still carry over.
    pub(crate) fn begin_member(&mut self) {
        self.tree = None;
        self.combined_len = 0;
    }

    /// Whether this state carries no history yet — i.e. nothing precedes
    /// the data about to be encoded. Only then may a call STORE the data
    /// wholesale: once a window exists, skipping the encode would silently
    /// drop members that could have matched into it.
    pub(crate) fn is_fresh(&self) -> bool {
        self.tail.is_empty()
            && self.combined_len == 0
            && self.tree.is_none()
            && self.dist_cache == [0; DIST_CACHE_SIZE]
            && self.last_length == 0
            && self
                .long_range
                .as_ref()
                .is_none_or(|lr| lr.total_pushed() == 0)
    }
}
