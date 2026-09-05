/// RAR5 native codec — LZSS+Huffman compression/decompression.
///
/// Clean-room implementation for software conservation and educational
/// purposes. Bitstream format derived from analysis of libarchive's
/// archive_read_support_format_rar5.c by Grzegorz Antoniak (2018),
/// an independent BSD-2-Clause licensed implementation.
///
/// License: BSD-2-Clause
pub(crate) mod common;
pub(crate) mod legacy;
pub(crate) mod modern;

// The public `codec::lzss_huff` path (used by the crate re-exports and
// examples) is kept as a re-export alias.
pub use modern::lzss_huff;

pub use lzss_huff::{
    DEFAULT_CHUNK_SIZE, DecodeOptions, DecoderState, EncodeOptions, EncoderState, FilterSpec,
    MAX_FILTER_BLOCK_LENGTH, MAX_STREAMING_FILTER_BUFFER, decode, decode_raw, decode_standalone,
    decode_standalone_to_writer, decode_to_writer, encode, encode_chunked, encode_chunked_raw,
    encode_raw, encode_with_auto_x86_filter, encode_with_filters, encode_with_progress_raw,
};
