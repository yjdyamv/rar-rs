//! Match finding and symbol parsing.
//!
//! Two parsers share the same match finders (hash chain, BT4 tree, sampled
//! long range): the fast/lazy walker used by the low levels and the
//! price-driven optimal parser for levels 2-5. Both emit the
//! [`Symbol`](super::Symbol) stream consumed by [`super::emit`]; blocks are
//! closed early when the local literal/match distribution drifts so each
//! emitted block gets its own Huffman tables.
//!
//! Role split:
//! - [`collect`] — match collection, the collector's probe dials and the
//!   relaxed matchless gate,
//! - [`optimal`] — the lazy walker and the price-driven optimal parse,
//! - [`block`] — the emitted-block policy.
//!
//! The family pipelines name this module's items through the re-exports below
//! (`encoder::chunked` / `encoder::filter` / `encoder::mod`), so the split is
//! invisible outside `parse/`.

mod block;
mod collect;
mod optimal;

use crate::codec::common::match_finder;
use crate::version::ArchiveVersion;

/// The limits every parse prices against: how long a match may be, how far
/// the decoder window reaches, and which distance table the stream is written
/// against.
///
/// Carried by the block parser, the pricer and the MT slice parsers; they all
/// need exactly this triple, and it is what `LEVEL_PARAMS[level]` supplies
/// one third of.
#[derive(Clone, Copy)]
pub(super) struct MatchLimits {
    /// Longest match the encoder may emit.
    pub max_match: usize,
    /// Distance the decoder window can reach.
    pub window: usize,
    /// RAR5 or RAR7 (v70) distance table.
    pub variant: ArchiveVersion,
}

/// One block search's specification: everything the match collector and the
/// block parser need beyond the buffer and the block range. These travel
/// together from the level table through the chunk loop, which is why they are
/// one value rather than six positional arguments at each call.
#[derive(Clone, Copy)]
pub(super) struct BlockSearch<'a> {
    /// Bytes of history at the front of `combined`; the block starts at
    /// `tail_len`.
    pub tail_len: usize,
    /// Hash-chain budget for the near finder.
    pub chain_len: usize,
    /// Per-level dial: when the near search drops to the recovery cadence.
    pub miss_threshold: usize,
    /// Match and distance limits.
    pub limits: MatchLimits,
    /// Long-range probe `(table, near_max, anchor)` over the sampled history;
    /// `None` when the level disables long-range search.
    pub lr: Option<(&'a match_finder::LongRange, usize, usize)>,
}

/// The sequential chunk driver's specification: the level's search dial, the
/// limits, and the long-range wiring (a read-only shared table for the
/// multi-threaded workers, the encoder state's own table otherwise).
#[derive(Clone, Copy)]
pub(super) struct SequentialSearch<'a> {
    /// Match and distance limits.
    pub limits: MatchLimits,
    /// Hash-chain budget for the near finder.
    pub chain_len: usize,
    /// Pricing passes (levels 2-5).
    pub passes: usize,
    /// Per-level dial: when the near search drops to the recovery cadence.
    pub miss_threshold: usize,
    /// Whether the level runs the long-range search at all.
    pub long_range: bool,
    /// Read-only shared long-range table (multi-threaded workers); `None`
    /// uses and extends the state's own table.
    pub lr_shared: Option<&'a match_finder::LongRange>,
    /// Absolute stream position of `chunk`'s first byte (the long-range
    /// anchor for a worker slice).
    pub lr_anchor: usize,
    /// Seed the finder with the carried tail (a fresh frame).
    pub seed_tail: bool,
}

pub(super) use block::{EMITTED_BLOCK_SIZE, find_block_end_adaptive};
pub(super) use collect::COLLECT_MISS_THRESHOLD;
#[cfg(test)]
pub(crate) use collect::set_fast_path_enabled;
#[cfg(feature = "parallel")]
pub(super) use optimal::{
    MT_DP_BLOCK_SIZE, RowIndex, find_matches_in_range, windowed_priced_parse,
};
pub(super) use optimal::{OPTIMAL_PARSE_PASSES, find_matches_optimal, find_matches_with_tail};
