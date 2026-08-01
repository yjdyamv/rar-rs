/// RAR5 native codec — LZSS+Huffman compression/decompression.
///
/// Clean-room implementation for software conservation and educational
/// purposes. Bitstream format derived from analysis of libarchive's
/// archive_read_support_format_rar5.c by Grzegorz Antoniak (2018),
/// an independent BSD-2-Clause licensed implementation.
///
/// License: BSD-2-Clause

pub mod bitstream;
pub mod decoder;
pub mod encoder;
pub mod filters;
pub mod huffman;
pub mod lz_match;
pub mod tables;
pub mod window;

pub use decoder::{decode, decode_standalone, DecoderState};
pub use encoder::{encode, encode_with_progress};