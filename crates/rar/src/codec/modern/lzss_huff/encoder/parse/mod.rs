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

pub(super) use block::{EMITTED_BLOCK_SIZE, find_block_end_adaptive};
pub(super) use collect::COLLECT_MISS_THRESHOLD;
#[cfg(test)]
pub(crate) use collect::set_fast_path_enabled;
#[cfg(feature = "parallel")]
pub(super) use optimal::{
    MT_DP_BLOCK_SIZE, RowIndex, find_matches_in_range, windowed_priced_parse,
};
pub(super) use optimal::{OPTIMAL_PARSE_PASSES, find_matches_optimal, find_matches_with_tail};
