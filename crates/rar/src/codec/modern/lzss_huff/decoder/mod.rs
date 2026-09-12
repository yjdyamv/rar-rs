//! RAR5/RAR7 (v70) LZSS+Huffman decoder.
//!
//! Role split:
//! - [`engine`] — public entry points and the streaming state machine,
//! - [`analysis`] — symbol-stream analysis/tracing tooling,
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
mod tables;
#[cfg(test)]
mod tests;

pub use analysis::{BlockStat, StreamAnalysis, TraceSymbol, analyze_stream, trace_stream};
pub use engine::{
    MAX_STREAMING_FILTER_BUFFER, decode_raw, decode_standalone, decode_standalone_to_writer,
    decode_to_writer,
};

use super::DIST_CACHE_SIZE;
use crate::codec::common::huffman::DecodeTable;
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
/// In a solid archive, the sliding window, distance cache, and Huffman
/// tables carry over between files. The fields are codec-private; the
/// archive layer only creates and holds the state.
pub struct DecoderState {
    window: SlidingWindow,
    dist_cache: [u64; DIST_CACHE_SIZE],
    last_length: u32,
    prev_low_dist: u32,
    table_nc: Option<DecodeTable>,
    table_dc: Option<DecodeTable>,
    table_ldc: Option<DecodeTable>,
    table_rc: Option<DecodeTable>,
}

impl DecoderState {
    pub fn new(dict_size: usize) -> Self {
        DecoderState {
            window: SlidingWindow::new(dict_size),
            dist_cache: [0; DIST_CACHE_SIZE],
            last_length: 0,
            prev_low_dist: 0,
            table_nc: None,
            table_dc: None,
            table_ldc: None,
            table_rc: None,
        }
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
