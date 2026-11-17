//! Codec implementations: the modern RAR5 LZSS+Huffman family and the
//! legacy RAR 1.5–4.x LZSS/Huffman/PPMd family.
//!
//! The RAR5 codec is a clean-room implementation for software conservation
//! and educational purposes. Bitstream format derived from analysis of
//! libarchive's `archive_read_support_format_rar5.c` by Grzegorz Antoniak
//! (2018), an independent BSD-2-Clause licensed implementation.
//!
//! License: BSD-2-Clause
pub(crate) mod common;
pub(crate) mod legacy;
pub(crate) mod modern;

/// Serializes every test that toggles a process-global codec switch: the
/// matchless fast path, the price-table carry, the price tier, and the
/// incompressibility screen. They are all process-global, so two tests flipping
/// different ones still race — a smoke test comparing its "off" arm against its
/// "on" arm would pick up another test's toggle and report a divergence that
/// does not exist. The counter deltas (`MATCHLESS_FAST_PATH_USES`) need the same
/// exclusion for their before/after pairs.
#[cfg(test)]
pub(crate) fn toggle_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

// The public `codec::lzss_huff` path (used by the crate re-exports and
// examples) is kept as a re-export alias.
pub use modern::lzss_huff;

pub use lzss_huff::{
    DEFAULT_CHUNK_SIZE, DecodeOptions, DecoderState, EncodeOptions, EncoderState, FilterSpec,
    MT_MIN_MEMBER_SIZE,
    MAX_FILTER_BLOCK_LENGTH, MAX_STREAMING_FILTER_BUFFER, decode, decode_raw, decode_standalone,
    decode_standalone_to_writer, decode_to_writer, encode, encode_chunked, encode_chunked_raw,
    encode_raw, encode_with_auto_x86_filter, encode_with_filters, encode_with_progress_raw,
};
