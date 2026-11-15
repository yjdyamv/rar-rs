//! Match collection: the candidates the parsers price.
//!
//! [`collect_block_matches`] takes one parse block through the shared tree
//! finder and records, per position, the runs the optimal parse prices, plus
//! the collector's dials: [`COLLECT_MISS_THRESHOLD`] and
//! [`FAST_RECOVER_INTERVAL`] decide when its tree walk drops to the recovery
//! cadence, and [`NICE_MATCH_LENGTH`] bounds what it measures.
//!
//! The relaxed matchless gate lives here too ([`RELAXED_MATCHLESS_MAX_LEN`],
//! [`DISABLE_MATCHLESS_FAST_PATH`] / [`set_fast_path_enabled`]): a block whose
//! longest candidate is within the collector's floor and whose repeat cache is
//! dead resolves to all-literals, so both parse drivers in [`super::optimal`]
//! skip the pricing passes byte-identically. `optimal` imports these; nothing
//! here depends on it.

use super::super::*;
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::atomic::Ordering;
/// Longest match the optimal parse commits to and steps over without
/// pricing the bytes it covers (rars `NICE_MATCH_LENGTH`).
///
/// Two roles, both experiment seams below: the finder stops comparing bytes at
/// this length (a match that reaches it is measured out to its real end
/// afterwards), and the parse stops pricing positions it covers.
pub(crate) const NICE_MATCH_LENGTH: usize = 64;

/// Minimum match length whose covered bytes the parse stops pricing. Separate
/// from [`NICE_MATCH_LENGTH`], which also caps how far the finder measures:
/// raising this one trades ratio for fewer tree descents, because a committed
/// match skips the descent of every byte it covers. `RAR_RS_COMMIT` overrides
/// it for measurement.
fn verify_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("RAR_RS_CAP")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(NICE_MATCH_LENGTH)
    })
}

/// Hash-table-first mode (`RAR_RS_HASH4`): the four-byte nearest-occurrence
/// table hands over a candidate in one load, and the tree is only descended
/// where that candidate is shorter than [`hash4_taper`]. This is the
/// official's split between its short tables and its hash chains — the table
/// answers most positions, the tree spends its steps on the rest.
fn hash4_mode() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| std::env::var_os("RAR_RS_HASH4").is_some_and(|v| v != "0"))
}

/// Length at or above which the table's match stands in for the tree descent.
/// Below it the descent still runs, because a table hit that short leaves the
/// lengths between it and the true best unpriced.
fn hash4_taper() -> usize {
    static TAPER: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *TAPER.get_or_init(|| {
        std::env::var("RAR_RS_HASH4_TAPER")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(24)
    })
}

fn commit_threshold() -> usize {
    static THRESHOLD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("RAR_RS_COMMIT")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(NICE_MATCH_LENGTH)
    })
}

/// Step cap for one descent under the stall tier.
///
/// Splitting the decision out keeps the environment plumbing away from the
/// hot loop and lets the thresholds be tested without a process-wide knob.
fn stall_cut(tier: Option<(usize, usize)>, stall: usize, chain_len: usize) -> usize {
    match tier {
        Some((threshold, stalled)) if stall >= threshold => stalled.min(chain_len),
        _ => chain_len,
    }
}

/// Stall tier: `(stall_threshold, stalled_cut)`.
///
/// `RAR_RS_STALL` = how many consecutive empty descents turn the tier on
/// (0 or absent = off, the shipped behaviour). `RAR_RS_STALL_CUT` = the step
/// cap used while it is on (default 8: about one cache line of nodes, enough
/// to keep the node the next position would have wanted).
///
/// This is the honoured part of the official matcher's counter-driven fast
/// mode: `Rar.exe` quarters its budget when its miss counters run ahead
/// (`FUN_1400468d4` reads `+0x300b8`/`+0x300bc` before picking the budget).
/// Spending *fewer steps* on a stretch that yields nothing is a different
/// lever from skipping the position outright (`fast_tree`), which throws the
/// insertion away with the search and needs a recovery cadence.
fn stall_config() -> Option<(usize, usize)> {
    static CONFIG: std::sync::OnceLock<Option<(usize, usize)>> = std::sync::OnceLock::new();
    *CONFIG.get_or_init(|| {
        let threshold = std::env::var("RAR_RS_STALL").ok()?.parse().ok()?;
        if threshold == 0 {
            return None;
        }
        let cut = std::env::var("RAR_RS_STALL_CUT")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(8);
        Some((threshold, cut))
    })
}

/// Full-search cadence inside fast mode (power of two): every this many
/// literal positions a real long-range probe runs, so the mode recovers
/// when compressible data returns (without it the first 64 KiB
/// incompressible run would lock the probe off for the whole member).
pub(super) const FAST_RECOVER_INTERVAL: usize = 128;

/// How many positions ahead the tree's head slot is warmed.
pub(super) const PREFETCH_AHEAD: usize = 16;

/// Consecutive failed tree probes before the block collector's tree walk
/// drops to the [`FAST_RECOVER_INTERVAL`] cadence, **by level**: this is the
/// one shortcut that measurably moved ratio when relaxed, and it is why the
/// m3-m5 ladder used to be flat.
///
/// A probe fails only when the tree found *no match at all* (not merely a short
/// one): on text-like data 4-15 byte matches are real signal (word prefixes) and
/// must keep the full search cadence, while on truly incompressible data a
/// 4-byte hash-collision match is ~2^-32 per position, so the miss run still
/// accumulates and the mode engages after a couple of KiB of wasted
/// cache-missing descents into the multi-MiB son array.
///
/// The gate is meant for the incompressible case, but on a **dense binary** it
/// also truncates the runs where a long match is about to appear, dropping
/// candidates at every level. Measured on the 12.5 MiB DLL at `-mt1` (dict 32m):
/// threshold 256 (today) 5,751,821 B / 6184 ms, 1024 5,742,924 B, 4096
/// 5,739,533 B / 6608 ms, and beyond 4096 nothing on that member (10 B). m5 takes
/// 4096 rather than "never" because the *time* is not flat there: with the gate
/// off entirely a 6 MiB XML member went from 625 ms to 24 s, since the gate also
/// stops inserting the positions it steps over and a dense tree makes every
/// later descent more expensive. m1-m3 keep 256 — which is why the default
/// level's bytes do not move at all — and m4 takes 1024.
pub(crate) const COLLECT_MISS_THRESHOLD: [usize; 6] = [0, 256, 256, 256, 1024, 4096];

/// Test seam: force the full pricing passes even for matchless blocks, to
/// prove the matchless fast path is byte-identical.
pub(super) static DISABLE_MATCHLESS_FAST_PATH: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn set_fast_path_enabled(enabled: bool) {
    DISABLE_MATCHLESS_FAST_PATH.store(!enabled, Ordering::Relaxed);
}

/// Relaxed matchless fast path: a block whose longest candidate match is at
/// most this many bytes (and that has no live repeat distance) parses to
/// all-literals, exactly like the strict matchless path. The collector only
/// ever reports matches of length >= 4 (`collect_block_matches`), so the value
/// below separates *accidental* 4-byte hash-collision matches (incompressible
/// data, where a 4-byte match can never beat four literals) from real signal.
/// Raising it would also skip genuinely useful short matches on compressible
/// data, so keep it at the collector's floor.
pub(super) const RELAXED_MATCHLESS_MAX_LEN: usize = 4;

/// One match finder result list for a block: every position's runs, one
/// position after another. Each run is `(length, distance)`; the sequence
/// per position has strictly increasing lengths, and the first distance to
/// reach a length is the cheapest one that can (nearest-first chains).
pub(super) struct BlockMatches {
    pub(super) runs: Vec<(u32, u32)>,
    pub(super) starts: Vec<u32>,
}

impl BlockMatches {
    pub(super) fn at(&self, index: usize) -> &[(u32, u32)] {
        &self.runs[self.starts[index] as usize..self.starts[index + 1] as usize]
    }

    /// Empty the block's runs and per-position starts, keeping the
    /// allocations for the next block — a 64 KiB block needs a 64 KiB
    /// `starts` array and up to as much again in runs, and there are two
    /// hundred of them in a 13 MiB member.
    pub(super) fn reset(&mut self, span: usize) {
        self.runs.clear();
        self.starts.clear();
        self.runs.reserve(span);
        self.starts.reserve(span + 1);
    }
}

/// Collect the matches the optimal parse will price at each position of a
/// block, taking the positions into the shared tree finder as it goes
/// (blocks must arrive in order, each exactly once).
///
/// Long-range candidates beyond the near window are folded in here: they
/// do not depend on the prices, so collecting them once lets every parse
/// pass replay the same answers. `spec.lr` is `(table, near_max, anchor)` as
/// in the lazy path, with the LR query slice being the chunk part of
/// `combined` (which starts at `spec.tail_len`); the probe only runs where the
/// tree found nothing useful (a good near match is never worse than a
/// far one).
/// Nearest-occurrence tables for the 2- and 3-byte prefixes.
///
/// The tree hashes four bytes, so a pair or triple that never grows to four
/// bytes never enters its buckets — and those are a quarter of what the
/// official encoder emits (it writes 135k two-byte and 188k three-byte
/// matches into a 13 MiB binary, while the tree found none). Both tables are
/// direct-mapped and hold one position per key: at a match this short the
/// nearest occurrence is the only one worth having, and the length bonus
/// bounds how far it can even be encoded (raw length 2 needs bonus 0, so
/// distance <= 0x100; raw length 3 needs bonus <= 1, so <= 0x2000), which is
/// why two 64k-slot tables cover the whole useful range.
#[derive(Debug)]
pub(crate) struct ShortFinder {
    head2: Vec<u32>,
    head3: Vec<u32>,
    /// Nearest occurrence of every four-byte prefix (`RAR_RS_HASH4`): the
    /// official's primary candidate source. The tree hashes four bytes too,
    /// but reaching a candidate costs a descent; this table hands one over in
    /// a single load, which is what lets the collector skip the descent where
    /// the table already found a long match.
    head4: Vec<u32>,
}

impl ShortFinder {
    const BITS: u32 = 16;
    const SIZE: usize = 1 << Self::BITS;
    /// Four-byte table width: 4 Mi slots is 16 MiB, a quarter of the tree's
    /// son array at the default window, and keeps a slot collision near the
    /// four-byte hash's own odds.
    const BITS4: u32 = 22;
    const SIZE4: usize = 1 << Self::BITS4;
    const NO_POS: u32 = u32::MAX;
    const HASH3: u32 = 0x9E37_79B1;

    pub(crate) fn new() -> Self {
        Self {
            head2: vec![Self::NO_POS; Self::SIZE],
            head3: vec![Self::NO_POS; Self::SIZE],
            // Allocated on first use: the mode is off by default and 16 MiB
            // of never-read slots would be most of the finder's footprint.
            head4: Vec::new(),
        }
    }

    /// Allocate the four-byte table for `RAR_RS_HASH4`. Idempotent, and cheap
    /// when the table is already up (one length check).
    pub(crate) fn enable_hash4(&mut self) {
        if self.head4.len() < Self::SIZE4 {
            self.head4.clear();
            self.head4.resize(Self::SIZE4, Self::NO_POS);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.head2.fill(Self::NO_POS);
        self.head3.fill(Self::NO_POS);
        self.head4.fill(Self::NO_POS);
    }

    /// Move every stored position down by `delta` (the frame slide), dropping
    /// the entries the slide pushed out of the frame.
    pub(crate) fn shift(&mut self, delta: usize) {
        let delta = delta as u32;
        for table in [&mut self.head2, &mut self.head3, &mut self.head4] {
            for slot in table.iter_mut() {
                *slot = match slot.checked_sub(delta) {
                    Some(moved) => moved,
                    None => Self::NO_POS,
                };
            }
        }
    }

    #[inline]
    pub(crate) fn insert(&mut self, data: &[u8], pos: usize) {
        let Some(triple) = data.get(pos..pos + 3) else {
            return;
        };
        let pos = pos as u32;
        self.head2[Self::key2(triple)] = pos;
        self.head3[(Self::key3(triple) >> (32 - Self::BITS)) as usize] = pos;
    }

    /// Seed the frame's carried tail so the first bytes of the chunk can match
    /// against history. Only the last `0x2000` bytes can ever matter (that is
    /// the widest distance a short match encodes at).
    pub(crate) fn seed(&mut self, data: &[u8], end: usize) {
        let start = end.saturating_sub(0x2000);
        for pos in start..end {
            self.insert(data, pos);
        }
    }

    /// Nearest occurrence of the 2-byte prefix at `pos` within
    /// `max_distance` (the caller still has to confirm the bytes).
    #[inline]
    pub(crate) fn nearest2(&self, data: &[u8], pos: usize, max_distance: usize) -> Option<u32> {
        let cand = self.head2[Self::key2(data.get(pos..pos + 3)?)];
        Self::distance(pos, cand, max_distance)
    }

    /// Nearest occurrence of the 3-byte prefix at `pos` within
    /// `max_distance`.
    #[inline]
    pub(crate) fn nearest3(&self, data: &[u8], pos: usize, max_distance: usize) -> Option<u32> {
        let cand = self.head3[(Self::key3(data.get(pos..pos + 3)?) >> (32 - Self::BITS)) as usize];
        Self::distance(pos, cand, max_distance)
    }

    /// Record this position as the nearest occurrence of its four-byte
    /// prefix. O(1) by design: the point of the table is that the positions
    /// the tree descent steps over can still be inserted.
    #[inline]
    pub(crate) fn insert4(&mut self, data: &[u8], pos: usize) {
        let Some(word) = data.get(pos..pos + 4) else {
            return;
        };
        self.head4[Self::key4(word)] = pos as u32;
    }

    /// Nearest occurrence of the four-byte prefix at `pos` within
    /// `max_distance` (the caller still has to measure the length).
    #[inline]
    pub(crate) fn nearest4(&self, data: &[u8], pos: usize, max_distance: usize) -> Option<u32> {
        let cand = self.head4[Self::key4(data.get(pos..pos + 4)?)];
        Self::distance(pos, cand, max_distance)
    }

    #[inline]
    fn key4(word: &[u8]) -> usize {
        let mixed =
            u32::from_le_bytes([word[0], word[1], word[2], word[3]]).wrapping_mul(Self::HASH3);
        (mixed >> (32 - Self::BITS4)) as usize
    }

    #[inline]
    fn distance(pos: usize, cand: u32, max_distance: usize) -> Option<u32> {
        if cand == Self::NO_POS {
            return None;
        }
        let cand = cand as usize;
        let distance = pos.checked_sub(cand)?;
        if distance == 0 || distance > max_distance {
            return None;
        }
        Some(distance as u32)
    }

    #[inline]
    fn key2(triple: &[u8]) -> usize {
        u16::from_le_bytes([triple[0], triple[1]]) as usize
    }

    #[inline]
    fn key3(triple: &[u8]) -> u32 {
        let word = u32::from_le_bytes([triple[0], triple[1], triple[2], 0]);
        word.wrapping_mul(Self::HASH3)
    }
}

impl Default for ShortFinder {
    fn default() -> Self {
        Self::new()
    }
}

/// Fast-mode state that outlives one parse block.
///
/// The collector drops to a recovery cadence once the tree has found nothing
/// for `miss_threshold` positions in a row; that run used to restart at every
/// 64 KiB block boundary, which on incompressible data meant paying a full
/// search cadence (and, now, a short-table probe) at the head of every block.
/// Carrying it across blocks keeps the mode engaged where the data is dense
/// and costs nothing where it is not.
#[derive(Debug, Default)]
pub(crate) struct SearchState {
    tree_misses: usize,
    fast_tree: bool,
    /// Consecutive positions whose descent yielded nothing at all. Drives
    /// the stall tier: a walk that keeps coming up empty in the same stretch
    /// of data is spending its steps on a part of the tree that has nothing
    /// to give, so it continues with a shorter step cap until a candidate
    /// shows up again (see [`stall_config`]).
    stall: usize,
}

impl SearchState {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

pub(super) fn collect_block_matches(
    finder: &mut match_finder::TreeMatchFinder,
    short: &mut ShortFinder,
    search: &mut SearchState,
    combined: &[u8],
    block: std::ops::Range<usize>,
    spec: super::BlockSearch<'_>,
    matches: &mut BlockMatches,
) {
    let super::BlockSearch {
        tail_len,
        chain_len,
        miss_threshold,
        limits,
        lr,
    } = spec;
    let max_match = limits.max_match;
    let window = limits.window;
    let span = block.end - block.start;
    matches.reset(span);
    let mut committed_through = block.start;
    // Scratch for the tree finder's per-position reports.
    let mut scratch: Vec<(u32, u32)> = Vec::new();
    let mut lr_fast = false;
    let mut lr_misses = 0usize;
    let tree_misses = &mut search.tree_misses;
    let stall = &mut search.stall;
    let stall_tier = stall_config();
    let hash4 = hash4_mode();
    let hash4_taper = hash4_taper();
    if hash4 {
        short.enable_hash4();
    }
    let fast_tree = &mut search.fast_tree;
    // The next gated position's first tree step (its head-resolved
    // descendant and that node's child pair), computed at the end of the
    // preceding iteration so its DRAM reads overlap the bookkeeping.
    let mut pending: Option<(usize, usize, u32, u32)> = None;
    for pos in block.clone() {
        matches.starts.push(matches.runs.len() as u32);
        let searching = pos >= committed_through;
        super::super::profile::count(super::super::profile::POS, 1);
        let max_distance = pos.min(window);
        let max_length = (block.end - pos).min(max_match);
        let before = matches.runs.len();
        // Inserting into a tree is the same descent as searching it, so a
        // position the parse steps over is stepped over here too rather
        // than inserted for nothing (its bytes are a copy of what the
        // match already points at, so the tree loses little by not holding
        // them).
        //
        // Fast mode: once the tree has found nothing for a long run of
        // positions (incompressible data), the descent stops paying — it
        // walks links through a multi-MiB son array that every probe
        // misses in. Skip the search (and with it the insertion) except
        // for a full recovery search every FAST_RECOVER_INTERVAL
        // positions, and resume when any real match (>= 16 bytes, past
        // spurious 4-byte hash coincidences) shows up.
        // Table-first candidate: one load, one length measurement, and the
        // position is inserted either way (that insert is the table's whole
        // maintenance cost).
        let mut hash4_len = 0usize;
        let mut hash4_dist = 0u32;
        if hash4 && searching && max_distance > 0 && max_length >= 4 && pos + 3 < combined.len() {
            if let Some(distance) = short.nearest4(combined, pos, max_distance) {
                let length = match_length_at(combined, pos, distance as usize, max_length);
                if length >= 4 {
                    hash4_len = length;
                    hash4_dist = distance;
                }
            }
            short.insert4(combined, pos);
        }
        if searching
            && max_distance > 0
            && max_length >= 4
            && pos + 3 < combined.len()
            && (!hash4 || hash4_len < hash4_taper)
            && (!*fast_tree || (pos & (FAST_RECOVER_INTERVAL - 1)) == 0)
        {
            // Warm a *later* position's head slot: the next position is
            // already resolved by the `seed_for` computed at the end of the
            // previous turn, so prefetching pos + 1 would only duplicate a
            // line this turn is about to read anyway. Far enough ahead that
            // the load has time to land.
            if pos + PREFETCH_AHEAD + 4 < combined.len() {
                finder.prefetch_head_for(combined, pos + PREFETCH_AHEAD);
            }
            super::super::profile::count(super::super::profile::SEARCH, 1);
            let cut = stall_cut(stall_tier, *stall, chain_len);
            let avail = combined.len() - pos;
            let len_limit = avail.min(verify_cap());
            scratch.clear();
            match pending.take() {
                Some((seed_pos, current, less, greater)) if seed_pos == pos => {
                    finder.matches_seeded(
                        match_finder::MatchQuery {
                            input: combined,
                            pos,
                            len_limit,
                            max_distance,
                            cut,
                            out: &mut scratch,
                        },
                        current,
                        less,
                        greater,
                    );
                }
                _ => finder.matches(match_finder::MatchQuery {
                    input: combined,
                    pos,
                    len_limit,
                    max_distance,
                    cut,
                    out: &mut scratch,
                }),
            }
            // The descent reports exact prefix lengths: it compares whole
            // words and seals any node whose comparison ran into the end of
            // the input, so a truncated limit can never make it adopt a
            // subtree on a prefix it could not check. Nothing here re-reads
            // the bytes the descent just compared — that used to be a full
            // `match_length_at` pass over every report, paying for the same
            // comparison twice to drop the bogus reports an unordered tree
            // produced. The seal is what made that pass redundant; the debug
            // check below keeps it honest without costing release builds.
            #[cfg(debug_assertions)]
            for &(len, dist) in scratch.iter() {
                let check = (len as usize).min(len_limit);
                debug_assert_eq!(
                    match_length_at(combined, pos, dist as usize, check),
                    check,
                    "tree reported a {len}-byte match at {pos} (distance {dist}) that does not hold"
                );
            }
            matches.runs.extend(scratch.iter().copied());
            // Measure the last report out to its real end: the tree
            // stops comparing at the limit, and a match reaching it
            // is what the parse commits to and steps over.
            if let Some(&(length, distance)) = scratch.last()
                && length as usize == len_limit
                && len_limit < avail.min(max_match)
            {
                super::super::profile::count(super::super::profile::VERIFY, 1);
                let full = match_length_at(combined, pos, distance as usize, avail.min(max_match));
                if let Some(last) = matches.runs.last_mut() {
                    last.0 = full as u32;
                }
            }
        }
        let longest = matches.runs[before..]
            .iter()
            .map(|&(len, _)| len as usize)
            .max()
            .unwrap_or(0);
        // Best length offered to the parse so far (the tree's, the short
        // tables', the long-range probe's). `longest` stays the tree's alone,
        // because the fast-mode counter below must keep accumulating while
        // only spurious 2-3 byte pairs remain.
        let mut offered = longest;
        // The table's match joins the tree's runs — appended, so the list stays
        // ascending in length and the parse prices it as the longest run.
        if hash4_len > offered {
            matches.runs.push((hash4_len as u32, hash4_dist));
            offered = hash4_len;
        }
        // The tree hashes four bytes, so a pair or triple that stops short of
        // four never enters its buckets; the official encoder writes such
        // matches by the hundreds of thousands. Ask the short tables only
        // where the tree found nothing at all: a run that does reach four
        // bytes already prices lengths 2-3 at its own (nearer) distance, and
        // the parse prices lengths in ascending order, so a short candidate
        // can only ever be appended to an empty run list.
        if longest < 4
            && searching
            && *tree_misses < 32
            && max_distance > 0
            && max_length >= 2
            && !*fast_tree
            && pos + 3 <= combined.len()
        {
            // Encodable windows, straight from the length bonus: raw length 2
            // needs bonus 0 (distance <= 0x100), raw length 3 needs bonus <= 1
            // (distance <= 0x2000).
            super::super::profile::count(super::super::profile::SHORT_PROBE, 2);
            for (limit, min_length) in [(0x100usize, 2usize), (0x2000, 3)] {
                if max_length < min_length {
                    continue;
                }
                let reach_limit = max_distance.min(limit);
                let candidate = if min_length == 2 {
                    short.nearest2(combined, pos, reach_limit)
                } else {
                    short.nearest3(combined, pos, reach_limit)
                };
                let Some(distance) = candidate else {
                    continue;
                };
                let length = match_length_at(combined, pos, distance as usize, max_length);
                if length >= min_length && length > offered {
                    matches.runs.push((length as u32, distance));
                    offered = length;
                }
            }
        }
        // Every position goes in, stepped-over ones included (the tree skips
        // them, but a nearest-occurrence table wants the most recent copy of a
        // pair or triple whatever the parse did with it), and only *after* the
        // probe above: the newest entry for this position's own key is the
        // position itself.
        if max_distance > 0 && !*fast_tree {
            short.insert(combined, pos);
        }
        // Fast mode gates on the tree finding *nothing at all* (`longest
        // == 0`), not on a short match: on text-like data 4-15 byte
        // matches are real signal (word prefixes) and must keep the full
        // search cadence, while on truly incompressible data a 4-byte
        // hash-collision match is ~2^-32 per position, so the miss run
        // still accumulates and the mode engages as quickly as ever.
        if longest == 0 {
            *tree_misses += 1;
            if !*fast_tree && *tree_misses >= miss_threshold {
                *fast_tree = true;
            }
        } else {
            *tree_misses = 0;
            *fast_tree = false;
        }
        // Long-range probe gating: the probe misses in a multi-MiB
        // random-access table, so once it has failed for a long run of
        // positions (incompressible data) it drops to the
        // FAST_RECOVER_INTERVAL cadence. Any hit resumes full probing —
        // a spurious short tree match must not reset this, only an actual
        // long-range hit pays for the probe.
        if let Some((long_range, near_max, anchor)) = lr
            && searching
            && longest < 64
            && pos + 4 <= combined.len()
            && (!lr_fast || (pos & (FAST_RECOVER_INTERVAL - 1)) == 0)
        {
            let chunk_off = pos - tail_len;
            let before = matches.runs.len();
            super::super::profile::count(super::super::profile::LR_PROBE, 1);
            if let Some((ld, ll)) = long_range.find_from(
                &combined[tail_len..],
                chunk_off,
                anchor,
                near_max + 1,
                max_length,
            ) && ll > offered
            {
                matches.runs.push((ll as u32, ld));
                offered = ll;
            }
            if matches.runs.len() > before {
                lr_fast = false;
                lr_misses = 0;
            } else {
                lr_misses += 1;
                // A 64 KiB parse block holds 64 K positions, so a
                // 64 K-probe threshold would only fire at the last
                // position of the block and never pay off; a few hundred
                // failed probes (a couple of KiB of incompressible data)
                // is already definitive and leaves room to act within
                // the block.
                if !lr_fast && lr_misses >= miss_threshold {
                    lr_fast = true;
                }
            }
        }
        // The parse can only take a match the block still has room for, so
        // the reach it will commit to is measured the way it measures it.
        let reach = offered.min(block.end - pos).min(max_match);
        // Stall bookkeeping for the next position: any candidate at all
        // clears the run, an empty position advances it.
        if stall_tier.is_some() {
            if offered >= 4 {
                *stall = 0;
            } else {
                *stall += 1;
            }
        }
        if reach >= commit_threshold() {
            committed_through = pos + reach;
        }
        // Seed the next position's first tree step. Its values are settled
        // now (every position through `pos` has inserted and this block does
        // nothing else to the tree), so reading them here is byte-identical
        // to reading them at the next turn. Mirror the gate exactly using
        // the just-updated commit/fast-tree state; the gate the next
        // iteration evaluates reads the same values.
        let npos = pos + 1;
        let n_max_distance = npos.min(window);
        let n_max_length = (block.end - npos).min(max_match);
        if npos >= committed_through
            && n_max_distance > 0
            && n_max_length >= 4
            && npos + 3 < combined.len()
            && (!*fast_tree || (npos & (FAST_RECOVER_INTERVAL - 1)) == 0)
        {
            let (current, less, greater) = finder.seed_for(combined, npos);
            pending = Some((npos, current, less, greater));
        } else {
            pending = None;
        }
    }
    matches.starts.push(matches.runs.len() as u32);
}

/// Length of the match at `pos` against `distance` bytes back, capped at
/// `max_length` (64-bit word compares with a scalar tail).
pub(super) fn match_length_at(
    data: &[u8],
    pos: usize,
    distance: usize,
    max_length: usize,
) -> usize {
    let cand = pos.wrapping_sub(distance);
    let limit = max_length.min(data.len() - pos).min(data.len() - cand);
    let mut l = 0usize;
    while l + 8 <= limit {
        let a = u64::from_le_bytes(data[cand + l..cand + l + 8].try_into().unwrap());
        let b = u64::from_le_bytes(data[pos + l..pos + l + 8].try_into().unwrap());
        if a != b {
            return l + ((a ^ b).trailing_zeros() / 8) as usize;
        }
        l += 8;
    }
    while l < limit && data[cand + l] == data[pos + l] {
        l += 1;
    }
    l
}

#[cfg(test)]
mod tests {
    use super::stall_cut;

    #[test]
    fn stall_tier_keeps_the_full_cap_below_its_threshold() {
        assert_eq!(stall_cut(None, 10_000, 96), 96);
        assert_eq!(stall_cut(Some((2, 4)), 1, 96), 96);
        assert_eq!(stall_cut(Some((2, 4)), 2, 96), 4);
        assert_eq!(stall_cut(Some((2, 4)), 2_000, 96), 4);
    }

    #[test]
    fn stall_tier_never_raises_the_tier_cap() {
        // Method 1 parses with a 4-step cap; a stalled cap above it must not
        // hand the descent more steps than the method allows.
        assert_eq!(stall_cut(Some((1, 64)), 5, 4), 4);
    }
}
