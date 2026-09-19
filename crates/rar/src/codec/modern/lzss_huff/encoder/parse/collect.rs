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

/// Full-search cadence inside fast mode (power of two): every this many
/// literal positions a real long-range probe runs, so the mode recovers
/// when compressible data returns (without it the first 64 KiB
/// incompressible run would lock the probe off for the whole member).
pub(super) const FAST_RECOVER_INTERVAL: usize = 128;

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
}

/// Collect the matches the optimal parse will price at each position of a
/// block, taking the positions into the shared tree finder as it goes
/// (blocks must arrive in order, each exactly once).
///
/// Long-range candidates beyond the near window are folded in here: they
/// do not depend on the prices, so collecting them once lets every parse
/// pass replay the same answers. `lr` is `(table, near_max, anchor)` as in
/// the lazy path, with the LR query slice being the chunk part of
/// `combined` (which starts at `tail_len`); the probe only runs where the
/// tree found nothing useful (a good near match is never worse than a
/// far one).
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_block_matches(
    finder: &mut match_finder::TreeMatchFinder,
    combined: &[u8],
    block: std::ops::Range<usize>,
    tail_len: usize,
    chain_len: usize,
    max_match: usize,
    window: usize,
    lr: Option<(&match_finder::LongRange, usize, usize)>,
    // Per-level dial: when the search drops to the recovery cadence.
    miss_threshold: usize,
) -> BlockMatches {
    let span = block.end - block.start;
    let mut matches = BlockMatches {
        runs: Vec::with_capacity(span),
        starts: Vec::with_capacity(span + 1),
    };
    let mut committed_through = block.start;
    // Scratch for the tree finder's per-position reports.
    let mut scratch: Vec<(u32, u32)> = Vec::new();
    let mut lr_fast = false;
    let mut lr_misses = 0usize;
    let mut tree_misses = 0usize;
    let mut fast_tree = false;
    // The next gated position's first tree step (its head-resolved
    // descendant and that node's child pair), computed at the end of the
    // preceding iteration so its DRAM reads overlap the bookkeeping.
    let mut pending: Option<(usize, usize, u32, u32)> = None;
    for pos in block.clone() {
        matches.starts.push(matches.runs.len() as u32);
        let searching = pos >= committed_through;
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
        if searching
            && max_distance > 0
            && max_length >= 4
            && pos + 3 < combined.len()
            && (!fast_tree || (pos & (FAST_RECOVER_INTERVAL - 1)) == 0)
        {
            // Warm the next position's head slot while this descent runs;
            // the seed computed at the tail then hits. Harmless when the
            // next position ends up unseeded/skipped.
            if pos + 4 < combined.len() {
                finder.prefetch_head_for(combined, pos + 1);
            }
            let avail = combined.len() - pos;
            let len_limit = avail.min(NICE_MATCH_LENGTH);
            scratch.clear();
            match pending.take() {
                Some((seed_pos, current, less, greater)) if seed_pos == pos => {
                    finder.matches_seeded(
                        combined,
                        pos,
                        len_limit,
                        max_distance,
                        chain_len,
                        &mut scratch,
                        current,
                        less,
                        greater,
                    );
                }
                _ => finder.matches(
                    combined,
                    pos,
                    len_limit,
                    max_distance,
                    chain_len,
                    &mut scratch,
                ),
            }
            // The tree's internal ordering invariants can break when it is
            // reused across chunks (budget-limited descents against a dense
            // persistent tree — the DLL reproduction hit this: a bogus
            // match copied the MZ header over real code, and the corrupt
            // member was silently written). Verify every report byte-exactly
            // before the parse can price it; a report whose bytes do not
            // actually match is dropped, a short over-report is truncated to
            // the true length. Cheap: the descent already compared these
            // bytes, and matches are sparse relative to positions.
            let mut w = 0usize;
            for r in 0..scratch.len() {
                let (_len, dist) = scratch[r];
                let actual = match_length_at(combined, pos, dist as usize, len_limit);
                if actual >= 4 {
                    scratch[w] = (actual as u32, dist);
                    w += 1;
                }
            }
            scratch.truncate(w);
            matches.runs.extend(scratch.iter().copied());
            // Measure the last report out to its real end: the tree
            // stops comparing at the limit, and a match reaching it
            // is what the parse commits to and steps over.
            if let Some(&(length, distance)) = scratch.last()
                && length as usize == len_limit
                && len_limit < avail.min(max_match)
            {
                let full = match_length_at(combined, pos, distance as usize, avail.min(max_match));
                if let Some(last) = matches.runs.last_mut() {
                    last.0 = full as u32;
                }
            }
        }
        let mut longest = matches.runs[before..]
            .iter()
            .map(|&(len, _)| len as usize)
            .max()
            .unwrap_or(0);
        // Fast mode gates on the tree finding *nothing at all* (`longest
        // == 0`), not on a short match: on text-like data 4-15 byte
        // matches are real signal (word prefixes) and must keep the full
        // search cadence, while on truly incompressible data a 4-byte
        // hash-collision match is ~2^-32 per position, so the miss run
        // still accumulates and the mode engages as quickly as ever.
        if longest == 0 {
            tree_misses += 1;
            if !fast_tree && tree_misses >= miss_threshold {
                fast_tree = true;
            }
        } else {
            tree_misses = 0;
            fast_tree = false;
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
            if let Some((ld, ll)) = long_range.find_from(
                &combined[tail_len..],
                chunk_off,
                anchor,
                near_max + 1,
                max_length,
            ) && ll > longest
            {
                matches.runs.push((ll as u32, ld));
                longest = ll;
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
        let reach = longest.min(block.end - pos).min(max_match);
        if reach >= NICE_MATCH_LENGTH {
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
            && (!fast_tree || (npos & (FAST_RECOVER_INTERVAL - 1)) == 0)
        {
            let (current, less, greater) = finder.seed_for(combined, npos);
            pending = Some((npos, current, less, greater));
        } else {
            pending = None;
        }
    }
    matches.starts.push(matches.runs.len() as u32);
    matches
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
