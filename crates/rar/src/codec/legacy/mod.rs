//! Legacy (RAR 1.5-3.x / RAR4) codecs: PPMd and the LZSS/Huffman
//! decoders and encoder used by pre-RAR5 members.

pub(crate) mod ppmd;
pub(crate) mod rar15;
pub(crate) mod rar20;
pub(crate) mod rar29;
pub(crate) mod rar29_encoder;
