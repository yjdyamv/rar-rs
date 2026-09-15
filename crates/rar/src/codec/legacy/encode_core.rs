//! Shared tables, slot math and level-table tokens for the legacy RAR 2.x/3.x
//! codecs.
//!
//! The length-slot tables are identical across the RAR20 and RAR29 codecs
//! (the offset tables differ: RAR20 stops at 48 slots, RAR29 has 60), and the
//! slot-window search, the offset-dependent length adjustment, the
//! most-recent-offset ring and the level-table token machinery are one
//! machine. They live here once so a fix reaches both writers; the shared
//! tables and the offset ring also feed both decoders.

/// Number of length slots (shared by the RAR20/RAR29 codecs).
pub(super) const LENGTH_COUNT: usize = 28;

/// Base length of each slot.
pub(super) const LENGTH_BASES: [usize; LENGTH_COUNT] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224,
];

/// Extra bits carried by each length slot.
pub(super) const LENGTH_BITS: [u8; LENGTH_COUNT] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5,
];

/// Base distance of each short-distance slot.
pub(super) const SHORT_BASES: [usize; 8] = [0, 4, 8, 16, 32, 64, 128, 192];

/// Extra bits carried by each short-distance slot.
pub(super) const SHORT_BITS: [u8; 8] = [2, 2, 3, 4, 5, 6, 6, 6];

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
    /// The plain level token for position `pos` holding `value`: RAR29
    /// subtracts `base[pos]` (its 4-bit delta), RAR20 ignores `base` and
    /// writes the literal level.
    fn plain(pos: usize, value: u8, base: &[u8]) -> LevelToken;

    /// Append the tokens for a run of `run` (3+) repetitions of `value`.
    fn repeat_previous(value: u8, run: usize, out: &mut Vec<LevelToken>);

    /// The short zero-run token (runs of 3..=10).
    fn zero_run_short(run: usize) -> LevelToken;

    /// The long zero-run token (runs of 11+).
    fn zero_run_long(run: usize) -> LevelToken;
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
            let total = run;
            let mut remaining = run;
            while remaining != 0 {
                if remaining >= 11 {
                    // Long-form chunks never leave a 1-2 tail behind (the
                    // 7-bit form needs at least 11).
                    let mut chunk = remaining.min(138);
                    if matches!(remaining - chunk, 1 | 2) && chunk >= 14 {
                        chunk -= 3;
                    }
                    tokens.push(A::zero_run_long(chunk));
                    remaining -= chunk;
                } else if remaining >= 3 {
                    let chunk = remaining.min(10);
                    tokens.push(A::zero_run_short(chunk));
                    remaining -= chunk;
                } else {
                    // A run too short for its own symbol is written out
                    // position by position, each a plain token like any
                    // other.
                    tokens.extend((pos..pos + remaining).map(|at| A::plain(at, 0, base)));
                    break;
                }
            }
            previous = Some(0);
            pos += total;
            continue;
        }

        if previous == Some(value) && run >= 3 {
            A::repeat_previous(value, run, &mut tokens);
            pos += run;
            continue;
        }

        tokens.push(A::plain(pos, value, base));
        previous = Some(value);
        pos += 1;
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy alphabet with distinct symbol numbers per form, so assertions
    /// read directly as chunk counts.
    struct TestMap;

    impl LevelAlphabet for TestMap {
        fn plain(_pos: usize, value: u8, _base: &[u8]) -> LevelToken {
            LevelToken::plain(value as usize)
        }

        fn repeat_previous(_value: u8, run: usize, out: &mut Vec<LevelToken>) {
            out.push(LevelToken::new(16, 3, run as u8));
        }

        fn zero_run_short(run: usize) -> LevelToken {
            LevelToken::new(17, 3, run as u8)
        }

        fn zero_run_long(run: usize) -> LevelToken {
            LevelToken::new(18, 7, run as u8)
        }
    }

    fn tokens(lengths: &[u8]) -> Vec<(usize, u8, u8)> {
        encode_level_tokens::<TestMap>(lengths, &[])
            .into_iter()
            .map(|token| (token.symbol, token.extra_bits, token.extra_value))
            .collect()
    }

    /// Short zero runs are plain tokens, the 3..=10 and 11+ forms bound the
    /// longer ones, and the long form never leaves a 1-2 tail behind.
    #[test]
    fn zero_runs_split_into_plain_short_and_long_forms() {
        assert_eq!(tokens(&[0, 0]), [(0, 0, 0), (0, 0, 0)]);
        assert_eq!(tokens(&[0, 0, 0]), [(17, 3, 3)]);
        assert_eq!(tokens(&[0; 10]), [(17, 3, 10)]);
        assert_eq!(tokens(&[0; 11]), [(18, 7, 11)]);
        assert_eq!(tokens(&[0; 138]), [(18, 7, 138)]);
        assert_eq!(tokens(&[0; 139]), [(18, 7, 135), (17, 3, 4)]);
        assert_eq!(tokens(&[0; 140]), [(18, 7, 135), (17, 3, 5)]);
        assert_eq!(tokens(&[0; 141]), [(18, 7, 138), (17, 3, 3)]);
        assert_eq!(tokens(&[0; 277]), [(18, 7, 138), (18, 7, 135), (17, 3, 4)]);
        assert_eq!(
            tokens(&[9, 0, 0, 9]),
            [(9, 0, 0), (0, 0, 0), (0, 0, 0), (9, 0, 0)]
        );
    }

    /// A run of 3+ repeats is only taken against an already-seen previous
    /// level: four equal levels are one plain token plus one repeat.
    #[test]
    fn repeats_need_a_previous_level() {
        assert_eq!(tokens(&[5, 5, 5]), [(5, 0, 0), (5, 0, 0), (5, 0, 0)]);
        assert_eq!(tokens(&[5, 5, 5, 5]), [(5, 0, 0), (16, 3, 3)]);
        assert_eq!(tokens(&[9, 5, 5, 5, 5]), [(9, 0, 0), (5, 0, 0), (16, 3, 3)]);
    }
}
