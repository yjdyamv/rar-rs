//! The emitted-block policy: how parsed symbols are grouped into the blocks
//! the writer stamps with their own Huffman tables.
//!
//! [`EMITTED_BLOCK_SIZE`] is the cap; [`find_block_end_adaptive`] closes a
//! block early when the symbol stream's local literal/match distribution
//! drifts. The parse itself is untouched by this policy, so the token stream
//! is byte-identical whatever the grouping.

use super::super::emit::apply_length_bonus;
use super::super::*;
/// Cap for grouping parsed symbols into *emitted* blocks. The RAR5 size
/// field allows blocks up to 4 GiB, so this is purely an encoder choice:
/// on distribution-stable data (repetitive text) merging many parse blocks
/// into one emitted block amortises the per-block Huffman table definitions
/// (WinRAR writes one block per whole member there); on heterogeneous data
/// the tables stay per-parse-block because the drift check keeps the parse
/// blocks small. Only the emitted grouping is larger — the parse itself is
/// unchanged, so token choices are byte-identical to the 128 KiB cap.
///
/// This is the one emitted-block policy every encode pipeline uses (plain
/// and filtered, sequential and multi-threaded); [`find_block_end_adaptive`]
/// is its splitter.
pub(crate) const EMITTED_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Group symbols into emitted blocks of up to `cap` uncompressed bytes, but
/// close the block early when the symbol stream's *local* literal/match
/// distribution drifts between adjacent ~64 KiB sub-spans.
///
/// The parse-side [`BlockSplitter`](super::optimal::BlockSplitter) compares each sub-block against the cumulative
/// counts of the open block, which cannot see section boundaries once the
/// cumulative mix stabilises (a DLL's code+data+padding blend looks stable
/// over a 1 MiB span). Comparing each sub-span against the *previous* one
/// catches those boundaries: repetitive text stays merged (WinRAR writes
/// one block per member there), heterogeneous binaries keep small blocks
/// (WinRAR's ~64 KiB DLL blocks). The token stream itself is untouched —
/// only the emitted grouping changes, so parsers and decoders behave the
/// same.
pub(crate) fn find_block_end_adaptive(
    symbols: &[Symbol],
    start: usize,
    cap: usize,
) -> (usize, usize) {
    const SUB_SPAN: usize = 64 * 1024;
    const DRIFT_DIVISOR: usize = 128;
    const LIT: usize = 256;
    const DIST: usize = 5;
    const LEN: usize = 3;
    const BUCKETS: usize = LIT + DIST + LEN;
    fn dist_bucket(d: u32) -> usize {
        if d < 4096 {
            0
        } else if d < 65536 {
            1
        } else if d < 1 << 20 {
            2
        } else if d < 4 << 20 {
            3
        } else {
            4
        }
    }
    fn len_bucket(l: u32) -> usize {
        if l < 16 {
            0
        } else if l < 64 {
            1
        } else {
            2
        }
    }
    let mut count = 0usize;
    let mut last_len = 0u32;
    let mut cur = [0u64; BUCKETS];
    let mut prev = [0u64; BUCKETS];
    let mut span_out = 0usize;
    let mut drifted = false;
    let mut have_prev = false;
    for (offset, symbol) in symbols[start..].iter().enumerate() {
        let i = start + offset;
        match symbol {
            Symbol::Literal(b) => {
                cur[*b as usize] += 1;
                count += 1;
                span_out += 1;
                last_len = 0;
            }
            Symbol::Match { distance, length } => {
                last_len = apply_length_bonus(*length, *distance);
                count += last_len as usize;
                span_out += last_len as usize;
                cur[LIT + dist_bucket(*distance)] += 1;
                cur[LIT + DIST + len_bucket(last_len)] += 1;
            }
            Symbol::CacheRef { length, .. } => {
                last_len = *length;
                count += *length as usize;
                span_out += *length as usize;
                cur[LIT + DIST + len_bucket(last_len)] += 1;
            }
            Symbol::Repeat => {
                count += last_len as usize;
                span_out += last_len as usize;
            }
            Symbol::Filter { .. } => {}
        }
        if span_out >= SUB_SPAN {
            // Full sub-span collected: local drift vs the previous sub-span.
            if have_prev && !drifted {
                let mut misplaced = 0u64;
                for (a, b) in cur.iter().zip(prev.iter()) {
                    misplaced += a.abs_diff(*b);
                }
                if misplaced > SUB_SPAN as u64 / DRIFT_DIVISOR as u64 {
                    drifted = true;
                }
            }
            have_prev = true;
            prev = cur;
            cur = [0u64; BUCKETS];
            span_out = 0;
        }
        if drifted || count >= cap {
            return (i + 1, count);
        }
    }
    (symbols.len(), count)
}
