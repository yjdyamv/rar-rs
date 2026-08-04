/// RAR5 codec constants: symbol counts, table sizes, format constants.
/// Huffman table symbol counts.
pub const HUFF_BC: usize = 20;
pub const HUFF_NC: usize = 306;
pub const HUFF_DC: usize = 64;
pub const HUFF_LDC: usize = 16;
pub const HUFF_RC: usize = 44;

/// Maximum Huffman code bit length.
pub const MAX_CODE_LENGTH: usize = 15;

/// Quick lookup table size (2^QUICK_BITS entries).
pub const QUICK_BITS: usize = 10;
pub const QUICK_SIZE: usize = 1 << QUICK_BITS;

/// Special symbols in the NC table.
pub const SYM_FILTER: usize = 256;
pub const SYM_REPEAT: usize = 257;
pub const SYM_CACHE_BASE: usize = 258;
pub const SYM_MATCH_BASE: usize = 262;

/// Distance cache size.
pub const DIST_CACHE_SIZE: usize = 4;

/// Filter types.
pub const FILTER_DELTA: u8 = 0;
pub const FILTER_E8: u8 = 1;
pub const FILTER_E8E9: u8 = 2;
pub const FILTER_ARM: u8 = 3;

/// Block header checksum seed.
pub const BLOCK_CHECKSUM_SEED: u8 = 0x5A;

/// Nibble-based RLE escape value for Huffman table encoding.
pub const NIBBLE_ESCAPE: u8 = 15;
