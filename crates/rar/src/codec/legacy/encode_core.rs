//! Shared tables and slot/level math for the legacy RAR 2.x/3.x codecs.
//!
//! The length-slot tables are identical across the RAR20 and RAR29 codecs
//! (the offset tables differ: RAR20 stops at 48 slots, RAR29 has 60), and the
//! slot-window search, the offset-dependent length adjustment, the
//! most-recent-offset ring and the level-table token alphabet are one
//! machine. They live here once so a fix reaches both writers, and the shared
//! bases feed both decoders.

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

/// Base distance of each short-distance slot.
pub(crate) const SHORT_BASES: [usize; 8] = [0, 4, 8, 16, 32, 64, 128, 192];

/// Extra bits carried by each short-distance slot.
pub(crate) const SHORT_BITS: [u8; 8] = [2, 2, 3, 4, 5, 6, 6, 6];

/// Shift an offset into the most-recent-first ring: `old_offsets[0]` is the
/// last match distance.
pub(super) fn push_old_offset(old_offsets: &mut [usize; 4], offset: usize) {
    old_offsets[3] = old_offsets[2];
    old_offsets[2] = old_offsets[1];
    old_offsets[1] = old_offsets[0];
    old_offsets[0] = offset;
}

/// The slot whose `[base, base + 2^bits - 1]` window contains `adjusted`,
/// plus the in-slot offset.
pub(super) fn slot_for(bases: &[usize], bits: &[u8], adjusted: usize) -> Option<(usize, usize)> {
    for (slot, &base) in bases.iter().enumerate() {
        let extra_bits = bits[slot];
        let max = base
            + if extra_bits == 0 {
                0
            } else {
                (1usize << extra_bits) - 1
            };
        if adjusted >= base && adjusted <= max {
            return Some((slot, adjusted - base));
        }
    }
    None
}

/// Length-slot lookup shared by both writers: `length` is the match length
/// (3+), the result the (slot, in-slot offset) pair.
pub(super) fn length_slot_for_match(length: usize) -> Result<(usize, usize), &'static str> {
    if length < 3 {
        return Err("match length is too short");
    }
    slot_for(&LENGTH_BASES, &LENGTH_BITS, length - 3).ok_or("match length is too long")
}

/// Offset-slot lookup shared by both writers, over the codec's offset table
/// (the slot counts differ: RAR20 48, RAR29 60).
pub(super) fn offset_slot_for(
    offset: usize,
    bases: &[usize],
    bits: &[u8],
) -> Result<(usize, usize), &'static str> {
    if offset == 0 {
        return Err("match offset is zero");
    }
    slot_for(bases, bits, offset - 1).ok_or("match offset is too large")
}

/// Extra length a match carries before its slot lookup is applied, from the
/// distance-adjusted length encodings shared by both codecs.
pub(super) fn match_length_adjustment(offset: usize) -> usize {
    usize::from(offset >= 0x2000) + usize::from(offset >= 0x40000)
}

/// Short-distance slot lookup (RAR20's 8-slot short matches).
pub(super) fn short_slot_for_match(offset: usize) -> Result<(usize, usize), &'static str> {
    if offset == 0 || offset > 256 {
        return Err("short match offset is out of range");
    }
    slot_for(&SHORT_BASES, &SHORT_BITS, offset - 1).ok_or("short match offset is out of range")
}

// ── Level-table tokens ─────────────────────────────────────────────────────

/// One token of the level-table encoding: a level-alphabet symbol plus its
/// inline extra bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LevelToken {
    pub(super) symbol: usize,
    pub(super) extra_bits: u8,
    pub(super) extra_value: u8,
}

impl LevelToken {
    pub(super) const fn plain(symbol: usize) -> Self {
        Self {
            symbol,
            extra_bits: 0,
            extra_value: 0,
        }
    }

    pub(super) const fn new(symbol: usize, extra_bits: u8, extra_value: u8) -> Self {
        Self {
            symbol,
            extra_bits,
            extra_value,
        }
    }
}

/// The level-table alphabet of a legacy codec: which symbol encodes a plain
/// level, a repeat of the previous level, and the two zero-run forms. RAR20
/// writes literal levels (19-symbol alphabet, one repeat form), RAR29 writes
/// deltas against a base table (20-symbol alphabet, short and long repeat
/// forms).
pub(super) trait LevelAlphabet {
    /// The plain level token for position `pos` holding `value`.
    fn plain(pos: usize, value: u8, base: &[u8]) -> LevelToken;

    /// Tokens for a run of `run` (3+) repetitions of `value`.
    fn repeat_previous(value: u8, run: usize) -> Vec<LevelToken>;

    /// The short zero-run token (runs of 3..=10).
    fn zero_run_short(run: usize) -> LevelToken;

    /// The long zero-run token (runs of 11+).
    fn zero_run_long(run: usize) -> LevelToken;
}

/// One piece of a zero run.
enum ZeroChunk {
    Long(usize),
    Short(usize),
    Plain(usize),
}

/// Split a zero run into long-form chunks (up to 138, never leaving a 1-2
/// tail behind), short-form chunks (3..=10) and a plain tail (1-2).
fn zero_run_chunks(mut run: usize) -> Vec<ZeroChunk> {
    let mut chunks = Vec::new();
    while run != 0 {
        if run >= 11 {
            let mut chunk = run.min(138);
            if matches!(run - chunk, 1 | 2) && chunk >= 14 {
                chunk -= 3;
            }
            chunks.push(ZeroChunk::Long(chunk));
            run -= chunk;
        } else if run >= 3 {
            let chunk = run.min(10);
            chunks.push(ZeroChunk::Short(chunk));
            run -= chunk;
        } else {
            chunks.push(ZeroChunk::Plain(run));
            break;
        }
    }
    chunks
}

/// Encode a level table (deltas against `base`, or literals when the
/// alphabet ignores it) into its token stream.
pub(super) fn encode_level_tokens<A: LevelAlphabet>(
    lengths: &[u8],
    base: &[u8],
) -> Vec<LevelToken> {
    let mut tokens = Vec::new();
    let mut pos = 0usize;
    let mut previous = None;
    while pos < lengths.len() {
        let value = lengths[pos];
        let mut run = 1usize;
        while pos + run < lengths.len() && lengths[pos + run] == value {
            run += 1;
        }

        if value == 0 {
            for chunk in zero_run_chunks(run) {
                match chunk {
                    ZeroChunk::Long(count) => tokens.push(A::zero_run_long(count)),
                    ZeroChunk::Short(count) => tokens.push(A::zero_run_short(count)),
                    // A run too short for its own symbol is written out
                    // position by position, each a plain token like any
                    // other.
                    ZeroChunk::Plain(count) => {
                        tokens.extend((pos..pos + count).map(|at| A::plain(at, 0, base)));
                    }
                }
            }
            previous = Some(0);
            pos += run;
            continue;
        }

        if previous == Some(value) && run >= 3 {
            tokens.extend(A::repeat_previous(value, run));
            pos += run;
            continue;
        }

        tokens.push(A::plain(pos, value, base));
        previous = Some(value);
        pos += 1;
    }
    tokens
}
