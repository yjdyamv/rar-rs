//! Codec primitives shared by the legacy and modern generations:
//! bit readers/writers, Huffman tables, filters, match finding and the
//! sliding window.

pub(crate) mod bitstream;
pub(crate) mod filters;
pub(crate) mod huffman;
pub(crate) mod match_finder;
pub(crate) mod window;
