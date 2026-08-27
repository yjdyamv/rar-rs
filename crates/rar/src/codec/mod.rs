/// RAR5 native codec — LZSS+Huffman compression/decompression.
///
/// Clean-room implementation for software conservation and educational
/// purposes. Bitstream format derived from analysis of libarchive's
/// archive_read_support_format_rar5.c by Grzegorz Antoniak (2018),
/// an independent BSD-2-Clause licensed implementation.
///
/// License: BSD-2-Clause
pub mod bitstream;
pub mod filters;
pub mod huffman;
pub mod match_finder;
pub mod rar50;
pub mod window;

pub use rar50::{
    DEFAULT_CHUNK_SIZE, DecodeOptions, DecoderState, EncoderState, FilterSpec,
    MAX_FILTER_BLOCK_LENGTH, MAX_STREAMING_FILTER_BUFFER, compress, compress_chunked,
    compress_with_progress, decode, decode_standalone, decode_standalone_to_writer,
    decode_to_writer, decompress, encode, encode_chunked, encode_with_filters,
    encode_with_progress,
};
