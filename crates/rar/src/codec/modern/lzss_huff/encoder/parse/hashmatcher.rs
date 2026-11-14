//! Official-shaped matcher, phase P1 (ADR 0011).
//!
//! Every table is **block-local**: sized by one parse block (64 KiB), so a
//! descent only reads the few hundred KiB it just wrote. Measurements behind
//! that constraint (`crates/rar/examples/hashprobe.rs` on ntoskrnl.exe):
//!
//! - laying the hash stream down costs ~11 ms per 13.9 M positions (0.2% of an
//!   encode), so it is done eagerly per block;
//! - a lookup inside a block slice costs 1.2 ns against 6.2 ns for the whole
//!   member's table — a 5× difference, and 100 candidates/position at 1.2 ns is
//!   the same order as the official encoder's 105 ns/position.
//!
//! Candidates are therefore reached through position-indexed links inside the
//! block (`prev[i]` = previous position in the block with the same key), never
//! by touching an older block: matches that start before the block are
//! `LongRange`'s and `ShortFinder`'s job.
//!
//! P1 is not wired into the parser yet — it exists to pin the shape and its
//! cost while the BT4 path keeps serving every encode.

#![allow(dead_code)]

/// Block-local indices are `u32`; `NONE` marks an empty link.
pub(crate) const NONE: u32 = u32::MAX;

const HEAD_BITS: usize = 14;
const HEAD_SIZE: usize = 1 << HEAD_BITS;

#[derive(Default)]
pub(crate) struct HashMatcher {
    /// Head of each key chain, block-local index.
    head: Vec<u32>,
    /// Previous block position carrying the same key.
    prev: Vec<u32>,
    /// Absolute offset of the prepared block.
    base: usize,
    /// Positions covered by the prepared block.
    len: usize,
}

impl HashMatcher {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reserve the block at `start..start + len` without linking anything.
    ///
    /// `insert` only accepts positions inside the reserved range, so the parser
    /// reserves the block it is about to walk and then links positions as it
    /// covers them — the tables never grow mid-block, which is what keeps them
    /// block-local and L2-resident.
    pub(crate) fn reserve(&mut self, start: usize, len: usize) {
        self.base = start;
        self.len = len;
        self.head.clear();
        self.head.resize(HEAD_SIZE, NONE);
        self.prev.clear();
        self.prev.resize(len, NONE);
    }

    /// Reserve the block at `start..start + len` and link every position in it
    /// in one streaming pass (the P1 bulk shape: ~11 ms per 13.9 M positions).
    pub(crate) fn prepare(&mut self, data: &[u8], start: usize, len: usize) {
        let end = (start + len).min(data.len().saturating_sub(8));
        self.reserve(start, end.saturating_sub(start));
        for pos in start..end {
            self.link(data, pos);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn base(&self) -> usize {
        self.base
    }

    /// Insert absolute `pos`, which must lie inside the prepared block.
    #[inline]
    pub(crate) fn insert(&mut self, data: &[u8], pos: usize) {
        if pos >= self.base && pos - self.base < self.len {
            self.link(data, pos);
        }
    }

    #[inline]
    fn link(&mut self, data: &[u8], pos: usize) {
        let at = pos - self.base;
        if at >= self.prev.len() {
            return;
        }
        let slot = hash8(load_u64(data, pos));
        self.prev[at] = self.head[slot];
        self.head[slot] = at as u32;
    }

    /// Collect the block's matches for absolute `pos`, nearest first, keeping
    /// only runs that lengthen (`(length, distance)` pairs, both `>= 4`).
    pub(crate) fn search(
        &self,
        data: &[u8],
        pos: usize,
        max_distance: usize,
        budget: usize,
        hard_limit: usize,
        out: &mut Vec<(u32, u32)>,
    ) {
        out.clear();
        if pos + 8 > data.len() || pos < self.base {
            return;
        }
        let mut cand = self.head[hash8(load_u64(data, pos))];
        let mut walked = 0usize;
        let limit = hard_limit.min(data.len() - pos);
        while cand != NONE && walked < budget {
            let abs = self.base + cand as usize;
            walked += 1;
            if abs >= pos {
                cand = self.prev[cand as usize];
                continue;
            }
            let distance = pos - abs;
            if distance > max_distance {
                break;
            }
            let length = match_length_at(data, pos, distance, limit);
            if length >= 4 && out.last().is_none_or(|&(best, _)| length as u32 > best) {
                out.push((length as u32, distance as u32));
            }
            cand = self.prev[cand as usize];
        }
    }
}

#[inline]
fn load_u64(data: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap())
}

#[inline]
fn hash8(word: u64) -> usize {
    (word.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - HEAD_BITS as u32)) as usize
}

/// Measure how far `data[pos..]` matches `data[pos - distance..]`.
pub(crate) fn match_length_at(
    data: &[u8],
    pos: usize,
    distance: usize,
    max_length: usize,
) -> usize {
    let mut length = 0usize;
    let mut cursor = pos;
    let limit = max_length.min(data.len() - pos).min(distance);
    while length + 8 <= limit {
        let left = load_u64(data, cursor);
        let right = load_u64(data, cursor - distance);
        if left != right {
            return length + ((left ^ right).trailing_zeros() as usize >> 3);
        }
        length += 8;
        cursor += 8;
    }
    while length < limit && data[cursor] == data[cursor - distance] {
        length += 1;
        cursor += 1;
    }
    length
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(len: usize) -> Vec<u8> {
        let words = [
            &b"the quick "[..],
            &b"brown fox "[..],
            &b"jumps over "[..],
            &b"the lazy dog "[..],
        ];
        let mut out = Vec::with_capacity(len);
        let mut seed = 0x9E37_79B9u32;
        while out.len() < len {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            out.extend_from_slice(words[(seed >> 24) as usize % words.len()]);
        }
        out.truncate(len);
        out
    }

    #[test]
    fn block_local_links_yield_real_ordered_matches() {
        let data = text_block(1 << 16);
        let mut matcher = HashMatcher::new();
        let mut out = Vec::new();

        // Walk the block the way the parser will: reserve the range, then
        // search every position and insert it after searching.
        let start = 0;
        matcher.reserve(start, data.len());
        let mut inserted = start;
        let mut offered = 0usize;
        for pos in start..data.len().saturating_sub(8) {
            while inserted < pos {
                matcher.insert(&data, inserted);
                inserted += 1;
            }
            matcher.search(&data, pos, pos.min(4096), 32, 64, &mut out);
            offered += out.len();
            let mut last = 0u32;
            for &(length, distance) in &out {
                assert!(length >= 4, "short candidate at {pos}");
                assert!(length > last, "candidates must lengthen at {pos}");
                last = length;
                assert!(distance as usize <= pos, "candidate points forward");
                let measured = match_length_at(&data, pos, distance as usize, 64);
                assert!(
                    measured >= length as usize || measured == 64,
                    "candidate {length}@{distance} is stale at {pos}"
                );
            }
        }
        assert!(offered > 1024, "a repetitive block must offer candidates");
        assert!(out.len() <= 32, "search respects its budget");
    }

    #[test]
    fn matches_stay_inside_the_block() {
        let data = text_block(1 << 16);
        let mut matcher = HashMatcher::new();
        matcher.reserve(0, data.len());
        let mut out = Vec::new();
        // Nothing is inserted, so the block has no candidates at all: the
        // matcher must not fall back to positions it never linked.
        matcher.search(&data, 4096, 4096, 32, 64, &mut out);
        assert!(out.is_empty(), "an unprepared block offers nothing");
    }
}
