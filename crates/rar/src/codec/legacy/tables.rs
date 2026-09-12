//! Lookup tables shared by the legacy RAR 2.x/3.x encoder implementations.
//!
//! The length-slot tables are identical across the RAR20 and RAR29 codecs
//! (the offset tables differ: RAR20 stops at 48 slots, RAR29 has 60), so the
//! shared prefix lives here instead of being copied per encoder file.

/// Number of length slots (shared by the RAR20/RAR29 codecs).
pub(crate) const LENGTH_COUNT: usize = 28;

/// Base length of each slot.
pub(crate) const LENGTH_BASES: [usize; LENGTH_COUNT] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224,
];

/// Extra bits carried by each length slot.
pub(crate) const LENGTH_BITS: [u8; LENGTH_COUNT] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5,
];
