//! Official-shaped matcher, phase P2 (ADR 0011): **distance classes**.
//!
//! The window is cut into `classes` contiguous blocks of `block_len` positions
//! (the official encoder's "distance classes"). Each block owns one BT4-shaped
//! binary tree: positions are inserted into the tree of the block that holds
//! them and searched from there, so a descent only ever touches candidates that
//! were inserted in one block — the shape of the reference implementation's
//! `+0x48`/`+0x68` class lists, with a tree per class instead of a list.
//!
//! The classes are searched **nearest first** with the nearest one getting the
//! full descent budget and the far ones `budget >> 2` (official `+0x184` /
//! `+0x188`), which is the half of the official design our BT4 does not have:
//! BT4 puts every candidate of the window into one tree and its distance is
//! whatever the content ordering happens to yield.
//!
//! # Why P1's block-local chain was dropped
//!
//! P1 shaped the tables by one parse block (64 KiB) and reached candidates
//! through position-indexed links inside it. Measured on 1 MiB of text with
//! 64 KiB blocks (`hashmatcher::tests::reports_candidate_density_and_cost`):
//! 109 852 candidates over 983 040 positions (0.11/pos, 4.5% of them reaching
//! 16 bytes) at 39 ns/position, against BT4's ~1.1 reports/position — an order
//! of magnitude sparser. Truncating the window is what cost the density, so P2
//! keeps the **whole window** covered and takes the official encoder's three
//! properties that BT4 shares except for the distance ordering:
//!
//! - full-window coverage (a candidate at `dict` distance is reachable),
//! - position-indexed children (`son` is indexed by position, not by content),
//! - one byte compare per descent step.
//!
//! # Coordinates
//!
//! Links store **frame positions** (`combined` indices, as the tree does).
//! A class's block is `[base[c], base[c] + block_len)` and its son slot is
//! `pos - base[c]`, not `pos & mask`: the child array stays a permutation of the
//! block under a frame slide, so `rebase` is O(classes) instead of a full pass
//! over `son` (`TreeMatchFinder::rebase` rewrites the whole child array).
//!
//! # Off by default
//!
//! `RAR_RS_CLASSES` selects the class count (absent or `0` keeps BT4), and
//! `RAR_RS_CLASS_FAR` the far-budget shift (default `2`). The ADR flips the
//! default only once the P2 measurements clear the acceptance bar.

use super::super::profile;
use super::collect::BlockFinder;
use crate::codec::common::match_finder::MatchQuery;

/// Empty child link / empty head slot.
const NO_LINK: u32 = u32::MAX;
/// Upper bound on distance classes (one `u8` band table, the official 8).
pub(crate) const MAX_CLASSES: usize = 8;
/// Log2 of the total head slots, split evenly across the classes (4 MiB total
/// at 8 classes, the same order as BT4's `1 << 20`).
const HEAD_BITS_TOTAL: u32 = 20;
/// Salt of the 4-byte hash, identical to `TreeMatchFinder`'s so that a ratio
/// difference is attributable to the class structure and not to the key.
const HASH_MULTIPLIER: u32 = 0x9E37_79B1;
/// A class that holds no block.
const EMPTY_BASE: usize = usize::MAX;

/// [`class_config`] from the environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClassConfig {
    /// Number of distance classes, `1..=MAX_CLASSES`.
    pub classes: usize,
    /// `budget >> far_shift` for every class but the nearest.
    pub far_shift: u32,
}

/// `None` keeps BT4; `Some` selects [`ClassMatchFinder`] instead.
pub(crate) fn class_config() -> Option<ClassConfig> {
    static CONFIG: std::sync::OnceLock<Option<ClassConfig>> = std::sync::OnceLock::new();
    *CONFIG.get_or_init(|| {
        let classes = std::env::var("RAR_RS_CLASSES")
            .ok()?
            .parse::<usize>()
            .ok()?
            .clamp(1, MAX_CLASSES);
        let far_shift = std::env::var("RAR_RS_CLASS_FAR")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(2);
        Some(ClassConfig { classes, far_shift })
    })
}

/// Split the window into `classes` power-of-two blocks and search the nearest
/// block with the full budget, the older ones with `budget >> far_shift`.
pub(crate) struct ClassMatchFinder {
    classes: usize,
    far_shift: u32,
    /// Positions per class block (power of two).
    block_len: usize,
    /// Log2 of [`Self::head_len`].
    head_bits: u32,
    /// Head slots per class.
    head_len: usize,
    /// `classes * head_len`: newest position per key, per class.
    head: Vec<u32>,
    /// `classes * block_len * 2` child slots, indexed by
    /// `((class * block_len) + (pos - base[class])) * 2`: slot `+0` is the
    /// smaller-byte child, `+1` the greater, exactly like BT4's `son`.
    son: Vec<u32>,
    /// First frame position of the block each class holds.
    base: Vec<usize>,
    /// Frame position where the next block starts; blocks are contiguous.
    next_base: usize,
    /// Class the next block is assigned to.
    next_class: usize,
    /// Class holding the block that contains the newest inserted position.
    cur_class: usize,
}

impl ClassMatchFinder {
    pub(crate) fn new(classes: usize, window: usize, far_shift: u32) -> Self {
        let classes = classes.clamp(1, MAX_CLASSES);
        let block_len = Self::block_len_for(window, classes);
        let head_bits = HEAD_BITS_TOTAL;
        let head_len = 1usize << head_bits;
        Self {
            classes,
            far_shift,
            block_len,
            head_bits,
            head_len,
            head: vec![NO_LINK; classes * head_len],
            son: vec![NO_LINK; classes * block_len * 2],
            base: vec![EMPTY_BASE; classes],
            next_base: 0,
            next_class: 0,
            cur_class: 0,
        }
    }

    fn block_len_for(window: usize, classes: usize) -> usize {
        (window.max(classes) / classes).max(1).next_power_of_two()
    }

    /// Positions covered by the classes: a little more than the requested
    /// window when `window / classes` is not a power of two.
    #[inline]
    fn coverage(&self) -> usize {
        self.classes * self.block_len
    }

    /// Drop every block; heads are cleared lazily when a class is re-assigned.
    pub(crate) fn clear(&mut self) {
        self.base.fill(EMPTY_BASE);
        self.next_base = 0;
        self.next_class = 0;
        self.cur_class = 0;
    }

    /// Resize to `window`, rebuilding the blocks when the block length changes.
    /// Returns `true` when the caller must reseed the tail.
    pub(crate) fn grow_to(&mut self, window: usize) -> bool {
        let block_len = Self::block_len_for(window, self.classes);
        if block_len == self.block_len {
            return false;
        }
        self.block_len = block_len;
        self.son = vec![NO_LINK; self.classes * block_len * 2];
        self.clear();
        true
    }

    /// The frame slid by `sub`: every block start moves down with it. Blocks
    /// that fall out of the frame end up empty, and their heads are cleared
    /// when the class is re-assigned.
    pub(crate) fn rebase(&mut self, sub: usize) {
        if sub == 0 {
            return;
        }
        self.next_base = self.next_base.saturating_sub(sub);
        for base in &mut self.base {
            if *base != EMPTY_BASE {
                *base = base.checked_sub(sub).unwrap_or(EMPTY_BASE);
            }
        }
    }

    /// Assign a block to `pos` when the newest block no longer covers it.
    ///
    /// Blocks are contiguous, so the band of a class is exactly
    /// `(pos - base[c]) >> shift`. A jump past the whole coverage means no
    /// stored candidate can be in window any more, and the grid restarts at
    /// `pos` (heads follow through the per-class rotation).
    #[inline]
    fn ensure_block(&mut self, pos: usize) {
        if pos >= self.next_base + self.coverage() {
            self.clear();
            self.next_base = pos;
        }
        while pos >= self.next_base {
            let class = self.next_class;
            self.next_class = if class + 1 == self.classes {
                0
            } else {
                class + 1
            };
            let low = class * self.head_len;
            self.head[low..low + self.head_len].fill(NO_LINK);
            self.base[class] = self.next_base;
            self.next_base += self.block_len;
            self.cur_class = class;
        }
    }

    #[inline]
    fn hash_slot(&self, input: &[u8], pos: usize) -> usize {
        let key = u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        (key.wrapping_mul(HASH_MULTIPLIER) >> (32 - self.head_bits)) as usize
    }

    /// Insert `pos` into its class and search every class, nearest first.
    pub(crate) fn search(&mut self, q: MatchQuery<'_>) {
        let MatchQuery {
            input,
            pos,
            len_limit,
            max_distance,
            cut,
            out,
        } = q;
        if pos + 4 > input.len() || len_limit < 4 {
            return;
        }
        // The limit is *not* shortened to the input end: a node inserted with a
        // truncated limit would adopt a subtree on a prefix that proves nothing
        // about the bytes past it (see `descend`), which poisons the tree. The
        // descent bounds its own reads instead, and seals the walk where the
        // comparison would run off the end.
        self.ensure_block(pos);
        let class = self.cur_class;
        let base = self.base[class];
        let head_slot = class * self.head_len;
        let son_base = class * self.block_len * 2;

        let hash = self.hash_slot(input, pos);

        let mut longest = 3usize;
        let head = head_slot + hash;
        self.descend::<true>(
            base,
            son_base,
            input,
            pos,
            len_limit,
            max_distance,
            cut,
            &mut longest,
            out,
            self.head[head] as usize,
        );
        self.head[head] = (pos - base) as u32;
        // Keep walking until the limit is reached: the greedy collector's early
        // exit at the nice length would drop candidates the price-driven DP still
        // wants (it measured 0.5 pp of ratio on every corpus).
        let stop = len_limit;
        if longest >= stop {
            return;
        }

        // The other classes, nearest block first. Blocks are assigned in
        // increasing position order around the ring, so walking it backwards
        // from `class` visits the nearest block first — the order that keeps the
        // reported distances increasing, lets a near class stop a far one from
        // reporting a shorter match, and decides which classes the cumulative
        // far budget reaches. A class is *not* a distance band: two blocks can
        // sit at the same band (the ring is `classes * block_len` wide, the
        // window can be narrower), so every class is walked on its own instead
        // of being indexed by band number.
        let mut far_budget = (cut >> self.far_shift).max(1);
        for back in 1..self.classes {
            if far_budget == 0 {
                return;
            }
            let other = (class + self.classes - back) % self.classes;
            let other_base = self.base[other];
            if other_base == EMPTY_BASE || other_base > pos {
                continue;
            }
            let offset = pos - other_base;
            if offset >= self.coverage() || offset >= max_distance {
                continue;
            }
            profile::count(profile::BAND, 1);
            far_budget = self.descend::<false>(
                other_base,
                other * self.block_len * 2,
                input,
                pos,
                len_limit,
                max_distance,
                far_budget,
                &mut longest,
                out,
                self.head[other * self.head_len + hash] as usize,
            );
            if longest >= stop {
                return;
            }
        }
    }

    /// One tree walk, from `current_rel` (a block-relative node) down to a
    /// leaf.
    ///
    /// Nodes are stored *relative to their class block* (`pos - base[class]`),
    /// not as frame positions: a block-relative node stays valid when the
    /// frame slides, so `rebase` only shifts `base` and never touches the
    /// child array (BT4 has to rewrite every slot because its slots are
    /// `pos & mask`).
    ///
    /// `INSERT` links `pos` into the tree as it walks (and adopts the children
    /// of a node whose whole prefix matched); a search-only walk leaves the
    /// tree alone. `longest` is shared by every class of one position, so the
    /// reported `(length, distance)` pairs keep strictly increasing, and a
    /// report that cannot reach `max_distance` still raises it — otherwise a
    /// far class could not stop a nearer one from reporting a shorter match.
    #[inline]
    #[allow(clippy::too_many_arguments)] // the walk state BT4 keeps in locals
    fn descend<const INSERT: bool>(
        &mut self,
        base: usize,
        slot: usize,
        input: &[u8],
        pos: usize,
        len_limit: usize,
        max_distance: usize,
        mut budget: usize,
        longest: &mut usize,
        out: &mut Vec<(u32, u32)>,
        mut current_rel: usize,
    ) -> usize {
        let here = slot + (pos - base) * 2;
        let mut ptr_less = here;
        let mut ptr_greater = here + 1;
        let mut len_less = 0usize;
        let mut len_greater = 0usize;
        let mut floor = pos - base;
        // Bytes past the end of `input` cannot be compared, so a position within
        // `len_limit` of the end inserts with a truncated prefix. Such a node
        // must never adopt a subtree: adoption claims the node's whole
        // comparable prefix matched, and a truncated prefix proves nothing about
        // the bytes beyond it, so the tree would stop being ordered and later
        // walks would overstate their lengths.
        let bound = len_limit.min(input.len() - pos);
        loop {
            if current_rel == NO_LINK as usize || current_rel >= floor || budget == 0 {
                if INSERT {
                    self.son[ptr_less] = NO_LINK;
                    self.son[ptr_greater] = NO_LINK;
                }
                return budget;
            }
            budget -= 1;
            floor = current_rel;
            let pair = slot + current_rel * 2;
            let child_less = self.son[pair] as usize;
            let child_greater = self.son[pair + 1] as usize;
            let current = base + current_rel;
            let mut len = len_less.min(len_greater);
            if len < bound && input[current + len] == input[pos + len] {
                len += 1;
                while len + 8 <= bound {
                    let a = u64::from_le_bytes(
                        input[current + len..current + len + 8].try_into().unwrap(),
                    );
                    let b = u64::from_le_bytes(input[pos + len..pos + len + 8].try_into().unwrap());
                    if a != b {
                        len += ((a ^ b).trailing_zeros() / 8) as usize;
                        break;
                    }
                    len += 8;
                }
                while len < bound && input[current + len] == input[pos + len] {
                    len += 1;
                }
                if len > *longest {
                    if pos - current <= max_distance {
                        out.push((len as u32, (pos - current) as u32));
                    }
                    *longest = len;
                }
            }
            if len == len_limit {
                // The node's whole comparable prefix is the new position's
                // prefix, so the new position adopts its children and the node
                // drops out as the farther of two interchangeable candidates.
                if INSERT {
                    self.son[ptr_less] = child_less as u32;
                    self.son[ptr_greater] = child_greater as u32;
                }
                return budget;
            }
            if len >= bound {
                // The comparison ran into the end of the input, so the order of
                // this subtree relative to `pos` is unknown: seal the attach
                // points instead of guessing a side.
                if INSERT {
                    self.son[ptr_less] = NO_LINK;
                    self.son[ptr_greater] = NO_LINK;
                }
                return budget;
            }
            if input[current + len] < input[pos + len] {
                if INSERT {
                    self.son[ptr_less] = current_rel as u32;
                }
                ptr_less = pair + 1;
                len_less = len;
                current_rel = child_greater;
            } else {
                if INSERT {
                    self.son[ptr_greater] = current_rel as u32;
                }
                ptr_greater = pair;
                len_greater = len;
                current_rel = child_less;
            }
        }
    }
}

impl BlockFinder for ClassMatchFinder {
    const PIPELINED: bool = false;

    fn matches(&mut self, q: MatchQuery<'_>) {
        self.search(q);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::common::match_finder::TreeMatchFinder;

    fn query<'a>(
        input: &'a [u8],
        pos: usize,
        len_limit: usize,
        max_distance: usize,
        cut: usize,
        out: &'a mut Vec<(u32, u32)>,
    ) -> MatchQuery<'a> {
        MatchQuery {
            input,
            pos,
            len_limit,
            max_distance,
            cut,
            out,
        }
    }

    /// A text-like corpus: repeated words with enough variety that the same
    /// 4-byte key recurs at many distances.
    fn corpus(len: usize) -> Vec<u8> {
        let words = [
            "alpha ", "beta ", "gamma ", "delta ", "epsilon ", "zeta ", "theta ", "kappa ",
        ];
        let mut out = Vec::with_capacity(len + 16);
        let mut state = 0x1234_5678u32;
        while out.len() < len {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            out.extend_from_slice(words[(state >> 16) as usize % words.len()].as_bytes());
        }
        out
    }

    /// The length both sequences really share, as the collector measures it.
    fn common_prefix(data: &[u8], pos: usize, distance: usize, cap: usize) -> usize {
        let mut len = 0;
        while len < cap && data[pos + len] == data[pos - distance + len] {
            len += 1;
        }
        len
    }

    /// What the collector does with a report: truncate to the real length and
    /// drop anything below the minimum match.
    fn verify(data: &[u8], pos: usize, out: &[(u32, u32)], cap: usize) -> Vec<(usize, usize)> {
        out.iter()
            .map(|&(len, distance)| {
                let (len, distance) = (len as usize, distance as usize);
                (common_prefix(data, pos, distance, len.min(cap)), distance)
            })
            .filter(|&(len, _)| len >= 4)
            .collect()
    }

    /// Claimed lengths strictly increase (that is what lets the collector stop
    /// at the nice length), distances stay inside the window, and — in the
    /// regime the encoder runs, one full-budget descent per position — no
    /// report ever overstates the match.
    #[test]
    fn reports_are_real_and_lengthen() {
        let data = corpus(200_000);
        let mut finder = ClassMatchFinder::new(8, 64 * 1024, 2);
        let mut out = Vec::new();
        for pos in 0..data.len() - 16 {
            out.clear();
            finder.search(query(&data, pos, 64, 64 * 1024, 64, &mut out));
            let mut previous = 3;
            for &(len, distance) in out.iter() {
                let (len, distance) = (len as usize, distance as usize);
                assert!(len > previous, "pos {pos}: {out:?}");
                previous = len;
                assert!(distance >= 1 && distance <= pos, "pos {pos}: {distance}");
                assert!(
                    distance <= 64 * 1024,
                    "pos {pos}: {distance} past the window"
                );
                assert_eq!(
                    common_prefix(&data, pos, distance, len),
                    len,
                    "pos {pos}: reported length {len} at {distance} is not a match"
                );
            }
        }
    }

    /// The nearest class is searched first, so a repeated phrase is reported at
    /// its nearest occurrence.
    #[test]
    fn nearest_class_reported_first() {
        const PHRASE: &[u8] = b"needle-in-the-haystack-0123456789";
        let mut data = Vec::new();
        data.extend_from_slice(b"PADDING PADDING PADDING PADDING ");
        data.extend_from_slice(PHRASE);
        data.resize(40_000, b'.');
        data.extend_from_slice(PHRASE);
        data.resize(44_000, b'.');
        data.extend_from_slice(PHRASE);
        data.extend_from_slice(b" tail tail tail");

        let pos = 44_000;
        let mut finder = ClassMatchFinder::new(4, 16 * 1024, 2);
        for p in 0..pos {
            finder.search(query(&data, p, 64, 16 * 1024, 64, &mut Vec::new()));
        }
        let mut out = Vec::new();
        finder.search(query(&data, pos, 64, 16 * 1024, 64, &mut out));
        assert!(!out.is_empty(), "the repeated phrase must be found");
        let (len, distance) = *out.last().expect("a report");
        assert_eq!(len as usize, PHRASE.len(), "whole phrase matches");
        assert_eq!(distance as usize, 4_000, "nearest copy is 4000 back");
    }

    /// `rebase` shifts the frame origin; blocks that slide out are dropped and
    /// the surviving ones keep reporting real, correctly distanced matches.
    #[test]
    fn rebase_keeps_matches_reachable() {
        let data = corpus(120_000);
        // Phase 1 parses `data[..n]` in its own coordinates; the frame then
        // slides by `sub`, so the kept tail is `keep = n - sub` bytes of the
        // new frame `data[sub..]`, whose positions continue at `keep`.
        let (n, sub) = (60_000usize, 10_000usize);
        let keep = n - sub;
        let mut finder = ClassMatchFinder::new(4, 32 * 1024, 2);
        for pos in 0..n {
            finder.search(query(&data[..n], pos, 64, 32 * 1024, 64, &mut Vec::new()));
        }
        finder.rebase(sub);
        let frame = &data[sub..];
        let mut out = Vec::new();
        let mut found = 0usize;
        for pos in keep..frame.len() {
            out.clear();
            finder.search(query(frame, pos, 64, 32 * 1024, 64, &mut out));
            for &(len, distance) in out.iter() {
                let (len, distance) = (len as usize, distance as usize);
                assert!(distance >= 1 && distance <= pos, "pos {pos}: {distance}");
                assert_eq!(
                    common_prefix(frame, pos, distance, len),
                    len,
                    "pos {pos}: reported length {len} at {distance} is not a match"
                );
            }
            found += verify(frame, pos, &out, 64).len();
        }
        assert!(found > 0, "the slid frame must still report matches");
    }

    /// Reports per position and filled length against BT4 on the same corpus:
    /// the class matcher must not be an order of magnitude sparser (the P1
    /// failure mode was 0.11 candidates per position against BT4's 1.1).
    ///
    /// The far budget is wide open (`far_shift` 0) on purpose: this checks that
    /// splitting the tree keeps the candidates reachable, not the far-budget
    /// policy (`far_budget_bounds_far_reports`). The policy is what costs ratio
    /// in the encoder — see the P2 section of ADR 0011.
    #[test]
    fn density_is_in_the_tree_ballpark() {
        let data = corpus(300_000);
        let mut class = ClassMatchFinder::new(8, 128 * 1024, 0);
        let mut tree = TreeMatchFinder::new(128 * 1024);
        let mut out = Vec::new();
        let (mut class_reports, mut class_len) = (0usize, 0usize);
        let (mut tree_reports, mut tree_len) = (0usize, 0usize);
        for pos in 0..data.len() - 64 {
            out.clear();
            class.search(query(&data, pos, 64, 128 * 1024, 64, &mut out));
            class_reports += out.len();
            class_len += out.last().map_or(0, |&(len, _)| len as usize);

            out.clear();
            tree.matches(query(&data, pos, 64, 128 * 1024, 64, &mut out));
            tree_reports += out.len();
            tree_len += out.last().map_or(0, |&(len, _)| len as usize);
        }
        assert!(
            class_reports * 2 >= tree_reports,
            "class matcher too sparse: {class_reports} vs {tree_reports}"
        );
        assert!(
            class_len * 10 >= tree_len * 9,
            "class matcher lost filled length: {class_len} vs {tree_len}"
        );
    }

    /// Far classes are searched with `cut >> far_shift`, so raising the shift
    /// can only shrink their reach.
    #[test]
    fn far_budget_bounds_far_reports() {
        let data = corpus(200_000);
        let far = |shift: u32| {
            let mut finder = ClassMatchFinder::new(8, 128 * 1024, shift);
            let mut out = Vec::new();
            let mut far_reports = 0usize;
            for pos in 0..60_000 {
                out.clear();
                finder.search(query(&data, pos, 64, 128 * 1024, 64, &mut out));
                far_reports += out
                    .iter()
                    .filter(|&&(_, distance)| distance as usize > 16 * 1024)
                    .count();
            }
            far_reports
        };
        let quarter = far(2);
        let eighth = far(3);
        assert!(
            eighth <= quarter,
            "shift 3 ({eighth}) must not beat shift 2 ({quarter})"
        );
    }

    /// The last `len_limit` bytes of a frame have no bytes left to compare
    /// beyond them, so those nodes insert with a truncated prefix. A truncated
    /// prefix must not adopt a subtree (it proves nothing about the bytes past
    /// it); if it did, the tree would stop being ordered and every later walk
    /// through it could overstate a length.
    #[test]
    fn truncated_limit_does_not_poison_the_tree() {
        let data = corpus(120_000);
        let (n, window) = (60_000usize, 32 * 1024usize);
        let mut finder = ClassMatchFinder::new(4, window, 2);
        // Phase 1 sees only `data[..n]`, so its last positions compare against
        // nothing past the end (truncated prefix).
        for pos in 0..n {
            finder.search(query(&data[..n], pos, 64, window, 64, &mut Vec::new()));
        }
        // Phase 2 continues in the same coordinates with the whole input, so
        // every position after `n` has a full limit and must report exactly.
        let mut out = Vec::new();
        for pos in n..data.len() - 8 {
            out.clear();
            finder.search(query(&data, pos, 64, window, 64, &mut out));
            for &(len, distance) in out.iter() {
                let (len, distance) = (len as usize, distance as usize);
                assert!(distance >= 1 && distance <= pos, "pos {pos}: {distance}");
                assert_eq!(
                    common_prefix(&data, pos, distance, len),
                    len,
                    "pos {pos}: reported length {len} at {distance} is not a match"
                );
            }
            assert!(
                !verify(&data, pos, &out, 64).is_empty() || out.is_empty(),
                "pos {pos}: a report that verifies to nothing wastes a candidate"
            );
        }
    }

    /// When one class block covers the whole frame the class finder *is* BT4:
    /// same head, same children, same visit order. Anything else would mean the
    /// ring layout or the descent bookkeeping has drifted from the reference,
    /// and the split would then be measured against the wrong baseline.
    #[test]
    fn stream_equals_bt4_when_one_class_covers_the_window() {
        use crate::codec::common::match_finder::TreeMatchFinder;

        // 60 KiB of data in a 128 KiB window: one class block (64 KiB) still
        // covers every position, so none is ever walked as a far class.
        let data = corpus(60 * 1024);
        let window = 128 * 1024usize;
        let (cut, limit) = (32usize, 64usize);
        let mut tree = TreeMatchFinder::new(window);
        let mut finders: Vec<ClassMatchFinder> = [1usize, 2]
            .into_iter()
            .map(|classes| ClassMatchFinder::new(classes, window, 2))
            .collect();
        let (mut reports, mut mismatches) = (vec![Vec::new(); 2], vec![0usize; 2]);
        for pos in 0..data.len() - limit {
            let mut expected = Vec::new();
            tree.matches(MatchQuery {
                input: &data,
                pos,
                len_limit: limit,
                max_distance: window,
                cut,
                out: &mut expected,
            });
            for (index, finder) in finders.iter_mut().enumerate() {
                reports[index].clear();
                finder.search(MatchQuery {
                    input: &data,
                    pos,
                    len_limit: limit,
                    max_distance: window,
                    cut,
                    out: &mut reports[index],
                });
                if reports[index] != expected {
                    mismatches[index] += 1;
                }
            }
        }
        assert_eq!(mismatches, vec![0, 0], "candidate stream left BT4's");
    }
}
