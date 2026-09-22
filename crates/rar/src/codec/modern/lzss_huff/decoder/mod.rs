//! RAR5/RAR7 (v70) LZSS+Huffman decoder.
//!
//! Role split:
//! - [`symbols`] — the symbol-stream state machine (block framing,
//!   checksums, Huffman tables, cache/repeat resolution),
//! - [`engine`] — public entry points and the single window/output loop,
//! - [`analysis`] — symbol-stream analysis/tracing tooling over
//!   [`symbols`],
//! - [`tables`] — Huffman-table reading and length/distance/filter
//!   primitives.
//!
//! Shared vocabulary ([`DecoderState`], [`DecodeOptions`]) and the pending
//! filter record passed between parse and engine live here.
//!
//! Clean-room implementation for software conservation and educational
//! purposes.
//!
//! License: BSD-2-Clause

mod analysis;
mod engine;
mod symbols;
mod tables;
#[cfg(test)]
mod tests;

pub use analysis::{BlockStat, StreamAnalysis, TraceSymbol, analyze_stream, trace_stream};
pub use engine::{
    MAX_STREAMING_FILTER_BUFFER, decode_raw, decode_standalone, decode_standalone_to_writer,
    decode_to_writer,
};

use symbols::SymbolState;

use crate::codec::common::window::SlidingWindow;
use crate::version::ArchiveVersion;

/// A filter record parsed from the symbol stream and awaiting its region.
struct PendingFilter {
    filter_type: u8,
    block_start: u64,
    block_length: u64,
    channels: u8,
    applied: bool,
}

/// Persistent decoder state for solid archive support.
///
/// In a solid archive, the sliding window and the symbol-stream state
/// (distance cache, Huffman tables) carry over between files. The fields are
/// codec-private; the archive layer only creates and holds the state.
pub struct DecoderState {
    window: SlidingWindow,
    symbols: SymbolState,
}

impl DecoderState {
    /// Fresh decoder state with a sliding window of `dict_size` bytes.
    pub fn new(dict_size: usize) -> Self {
        DecoderState {
            window: SlidingWindow::new(dict_size),
            symbols: SymbolState::default(),
        }
    }

    /// Capacity, in bytes, of the shared window.
    pub fn window_capacity(&self) -> usize {
        self.window.capacity()
    }

    /// Grow the shared window to `new_capacity` (a larger power of two),
    /// carrying the lookbehind tail, distance cache, last-length state and
    /// Huffman tables forward. A no-op when the window is already at least
    /// that large.
    pub fn grow_window(&mut self, new_capacity: usize) {
        self.window.grow(new_capacity);
    }
}

/// Options for decoding one member.
///
/// `dict_size_log` sizes the window for standalone members only: when
/// `state` is carried (solid chains), the state owns its window and the
/// log is ignored by construction.
#[derive(Default)]
pub struct DecodeOptions<'a> {
    /// Dictionary size as log2(size/128KB), 0 = 128KB. Used when `state`
    /// is `None`.
    pub dict_size_log: u8,
    /// Actual dictionary size in bytes (RAR7, `comp_version` 1): may be
    /// non-power-of-two; the window rounds up to a power of two.
    pub dict_size_bytes: Option<u64>,
    /// RAR7 algorithm variant (extended distance codes, `v70`).
    pub variant: ArchiveVersion,
    /// Shared decoder state for solid-chain continuity (`None` for
    /// standalone members).
    pub state: Option<&'a mut DecoderState>,
}
