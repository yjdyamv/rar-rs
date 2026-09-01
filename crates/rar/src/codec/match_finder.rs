/// RAR5 LZ match finder — hash-chain match finder for LZSS compression.
use super::rar50::DIST_CACHE_SIZE;

/// Sampling step of the long-range hash table: one 4-byte sample per
/// `LONG_RANGE_STEP` bytes of history. Finer steps catch more matches at
/// higher memory cost; 16 bytes keeps the table at ~2x the history size
/// in memory (entries are 8 bytes and the table is double-hashed).
pub const LONG_RANGE_STEP: usize = 16;

/// Long-range history window: match distances beyond the near window
/// (tail + chunk, a few MiB) are found through a sampled hash table over
/// the most recent input. Match distances are inherently bounded by the
/// dictionary (the decoder window can only reach `dict_size` back —
/// WinRAR 7.23 also refuses to compress a beyond-dictionary distant copy
/// at default or `-md32m`, and only does so with a dictionary that covers
/// the distance), so the retained history never needs to exceed
/// `LONG_RANGE_MAX.min(dictionary)`. The 128 MiB cap additionally bounds
/// table memory for huge dictionaries: history + table stay under ~256
/// MiB even for multi-GiB (or RAR7) dictionaries. WinRAR's `-mcl` long
/// range search works the same way (sampled, bounded memory).
pub const LONG_RANGE_MAX: usize = 128 * 1024 * 1024;

/// Open-addressing hash table mapping a 4-byte sample hash to its most
/// recent position inside the long-range history (a relative offset).
///
/// Keys are stored as `hash + 1` so 0 marks an empty slot; values are
/// i32 because the history is bounded by [`LONG_RANGE_MAX`]. The table
/// starts small and grows geometrically as history is pushed, so memory
/// tracks the actual data size instead of the declared dictionary.
struct LongRangeTable {
    keys: Vec<u32>,
    vals: Vec<i32>,
    /// Second-newest offset per key. The multi-threaded encoder pre-builds
    /// the table over a whole window before any slice parses, so a slice's
    /// own positions are already in it: a probe at a position that is the
    /// newest occurrence of its 4-byte window would find itself (distance
    /// 0) and miss the real copy source it shadows. Keeping the previous
    /// occurrence lets the probe fall back to it. The sequential encoder
    /// pushes history incrementally (never the chunk being parsed), so its
    /// newest entry is always valid and this slot is never consulted.
    vals2: Vec<i32>,
    mask: usize,
}

/// Smallest initial capacity (entries); grows on demand up to the size
/// needed for [`LONG_RANGE_MAX`] of history.
const TABLE_MIN_CAP: usize = 1024;

impl LongRangeTable {
    /// Largest table capacity needed for `history_max` bytes of history
    /// (one sample per [`LONG_RANGE_STEP`] bytes, kept at <= 50% load).
    fn max_cap_for(history_max: usize) -> usize {
        let samples = history_max / LONG_RANGE_STEP;
        (samples * 2).max(TABLE_MIN_CAP).next_power_of_two()
    }

    /// Allocate a table with exactly `cap` entries (a power of two).
    fn with_capacity(cap: usize) -> Self {
        Self {
            keys: vec![0; cap],
            vals: vec![0; cap],
            vals2: vec![0; cap],
            mask: cap - 1,
        }
    }

    fn cap(&self) -> usize {
        self.keys.len()
    }

    /// Grow the table so it can hold `samples` samples (<= 50% load),
    /// rehashing the existing entries; never exceeds `max_cap`.
    fn grow_to(&mut self, samples: usize, max_cap: usize) {
        let want = (samples * 2)
            .max(TABLE_MIN_CAP)
            .next_power_of_two()
            .min(max_cap);
        if want <= self.cap() {
            return;
        }
        let mut grown = LongRangeTable::with_capacity(want);
        for (i, k) in self.keys.iter().enumerate() {
            if *k != 0 {
                // Insert the second-newest first so the newest ends up in
                // `vals` and the chain survives the rehash.
                let v2 = self.vals2[i];
                if v2 != 0 {
                    grown.insert(*k - 1, v2);
                }
                grown.insert(*k - 1, self.vals[i]);
            }
        }
        *self = grown;
    }
    /// table from scratch (used when the window slides).
    fn clear_and_rebuild(&mut self, hist: &[u8], max_cap: usize) {
        self.grow_to(hist.len() / LONG_RANGE_STEP, max_cap);
        self.keys.fill(0);
        let mut off = 0usize;
        while off + 4 <= hist.len() {
            let key = long_hash4(hist, off);
            self.insert(key, off as i32);
            off += LONG_RANGE_STEP;
        }
    }

    #[inline]
    fn probe(&self, key: u32) -> usize {
        (key.wrapping_mul(0x9E3779B1) as usize) & self.mask
    }

    /// Insert or refresh `(key, offset)` — the newest position wins for
    /// repeated keys (LZ favors the most recent candidate); the previous
    /// newest drops to the second slot (see [`LongRangeTable::vals2`]).
    fn insert(&mut self, key: u32, offset: i32) {
        let mut i = self.probe(key);
        let step = 1;
        loop {
            if self.keys[i] == 0 || self.keys[i] == key + 1 {
                if self.keys[i] == key + 1 {
                    self.vals2[i] = self.vals[i];
                }
                self.keys[i] = key + 1;
                self.vals[i] = offset;
                return;
            }
            i = (i + step) & self.mask;
        }
    }

    /// Most recent offset for `key` strictly before `limit` (hist-relative):
    /// the newest entry when it qualifies, else the second-newest. Used by
    /// the pre-built-table path where the newest can be the probing
    /// position itself (self-shadowing).
    #[inline]
    fn get_before(&self, key: u32, limit: i32) -> Option<i32> {
        let mut i = self.probe(key);
        loop {
            match self.keys[i] {
                0 => return None,
                k if k == key + 1 => {
                    let v = self.vals[i];
                    if v < limit {
                        return Some(v);
                    }
                    let v2 = self.vals2[i];
                    return (v2 != 0 && v2 < limit).then_some(v2);
                }
                _ => i = (i + 1) & self.mask,
            }
        }
    }

    /// Drop all entries (used when the history window slides).
    fn clear(&mut self) {
        self.keys.fill(0);
    }
}

/// 4-byte sample hash used by both the long-range table and the finder.
fn long_hash4(data: &[u8], pos: usize) -> u32 {
    let h = (data[pos] as u32)
        | ((data[pos + 1] as u32) << 8)
        | ((data[pos + 2] as u32) << 16)
        | ((data[pos + 3] as u32) << 24);
    h.wrapping_mul(0x9E3779B1)
}

/// Long-range match history: a sliding window of input bytes plus a
/// sampled hash table over it. Matches found here have distances up to
/// the history length; the history itself is bounded by both
/// [`LONG_RANGE_MAX`] and the encoder's dictionary window (candidates
/// beyond the window can never be emitted, so keeping them wastes table
/// memory and lookup time). The window slides by dropping the oldest
/// bytes and rebuilding the table when full.
pub struct LongRange {
    hist: Vec<u8>,
    table: LongRangeTable,
    /// Largest table capacity needed for `max_hist` bytes of history.
    max_cap: usize,
    /// Maximum match distance (the encoder's dictionary window).
    window: usize,
    /// History bound: `LONG_RANGE_MAX.min(window)`.
    max_hist: usize,
    /// Total bytes ever pushed (absolute stream length covered). `base()`
    /// derives the absolute offset of `hist[0]` after window slides.
    total_pushed: usize,
}

impl LongRange {
    pub fn new(window: usize) -> Self {
        let max_hist = LONG_RANGE_MAX.min(window.max(LONG_RANGE_STEP));
        Self {
            // Memory tracks the actual history: the buffer and the sample
            // table start small and grow on demand, instead of paying
            // ~2x the declared dictionary up front (64 MiB at the default
            // 32 MiB dict, even for a one-byte archive).
            hist: Vec::new(),
            table: LongRangeTable::with_capacity(TABLE_MIN_CAP),
            max_cap: LongRangeTable::max_cap_for(max_hist),
            window,
            max_hist,
            total_pushed: 0,
        }
    }

    pub fn reset(&mut self) {
        self.hist.clear();
        self.table.clear();
    }

    /// Current history length in bytes (≤ [`LONG_RANGE_MAX`]).
    pub fn hist_len(&self) -> usize {
        self.hist.len()
    }

    /// True when the long-range finder has history to search.
    pub fn is_empty(&self) -> bool {
        self.hist.is_empty()
    }

    #[inline]
    fn hash4(&self, data: &[u8], pos: usize) -> u32 {
        long_hash4(data, pos)
    }

    /// Find the longest match for `chunk[pos..]` against the history.
    /// `min_dist` skips candidates the near match finder already covers
    /// (distances up to tail + chunk); candidates beyond `self.window`
    /// are rejected (the decoder window could not reach them).
    pub fn find(
        &self,
        chunk: &[u8],
        pos: usize,
        min_dist: usize,
        max_len: usize,
    ) -> Option<(u32, usize)> {
        // The query chunk directly abuts the history end, so its anchor
        // (absolute stream position) is simply total pushed bytes.
        self.find_from(chunk, pos, self.total_pushed, min_dist, max_len)
    }

    /// Append `chunk` to the history, sliding the window and rebuilding
    /// the sample table when the bound is exceeded. Call after the chunk
    /// has been matched (so the chunk itself is searchable later).
    ///
    /// To keep the amortized cost linear, the window drops back to half
    /// its size (not just the overflow) before rebuilding the table; the
    /// long-range reachable distance then oscillates between
    /// `max_hist/2` and `max_hist`.
    pub fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.total_pushed += chunk.len();
        if chunk.len() >= self.max_hist {
            // A single chunk fills (or exceeds) the whole window: keep
            // only its tail and rebuild the table from scratch.
            let tail = &chunk[chunk.len() - self.max_hist..];
            self.hist.clear();
            self.hist.extend_from_slice(tail);
            self.rebuild_table();
            return;
        }
        if self.hist.len() + chunk.len() > self.max_hist {
            let drop = (self.hist.len() + chunk.len() - self.max_hist / 2).min(self.hist.len());
            self.hist.drain(0..drop);
            self.rebuild_table();
        }
        let base = self.hist.len();
        self.hist.extend_from_slice(chunk);
        // Grow the table for the new sample count, then index the new
        // region only (existing entries stay valid).
        self.table
            .grow_to(self.hist.len() / LONG_RANGE_STEP, self.max_cap);
        let mut off = base;
        while off + 4 <= self.hist.len() {
            let key = self.hash4(&self.hist, off);
            self.table.insert(key, off as i32);
            off += LONG_RANGE_STEP;
        }
    }

    fn rebuild_table(&mut self) {
        self.table.clear_and_rebuild(&self.hist, self.max_cap);
    }

    /// Absolute stream offset of `hist[0]` (total pushed minus what the
    /// sliding window still holds).
    pub(crate) fn hist_base(&self) -> usize {
        self.total_pushed - self.hist.len()
    }

    /// Total bytes ever pushed (the absolute stream position of the
    /// history end).
    pub(crate) fn total_pushed(&self) -> usize {
        self.total_pushed
    }

    /// Read-only view of the retained history bytes (callers seed a fresh
    /// table for parallel workers with the same content).
    #[cfg_attr(not(feature = "parallel"), allow(dead_code))]
    pub(crate) fn hist_bytes(&self) -> &[u8] {
        &self.hist
    }

    /// Like [`Self::find`], but the query chunk starts at absolute stream
    /// position `anchor` (history may cover more than just the bytes right
    /// before it, e.g. when several slices of one buffer are encoded in
    /// parallel against a shared table). Candidates at or after `anchor`
    /// are rejected; distances are measured from the true absolute
    /// positions, so emitted matches stay valid LZ.
    pub(crate) fn find_from(
        &self,
        chunk: &[u8],
        pos: usize,
        anchor: usize,
        min_dist: usize,
        max_len: usize,
    ) -> Option<(u32, usize)> {
        if pos + 4 > chunk.len() {
            return None;
        }
        // The query position must lie inside the retained window: when the
        // history was truncated (input larger than the window), positions
        // older than `hist_base` are gone and there is no candidate. This
        // happens with small dictionaries against a large chunk (and with
        // multi-window MT chains); without the guard the subtraction below
        // underflows.
        let query_abs = anchor + pos;
        if query_abs < self.hist_base() {
            return None;
        }
        let key = self.hash4(chunk, pos);
        // Strictly before the probing position: in a pre-built table the
        // newest entry for this key can be the probing position itself
        // (self-shadow), so ask for the newest qualifying entry and let
        // the table fall back to the previous occurrence.
        let cand = self
            .table
            .get_before(key, (query_abs - self.hist_base()) as i32)? as usize;
        let cand_abs = self.hist_base() + cand;
        if cand_abs >= anchor {
            return None;
        }
        let dist = anchor + pos - cand_abs;
        if dist < min_dist || dist > self.window {
            return None;
        }
        // Comparisons may read past `anchor` into later slices' plaintext:
        // those are real input bytes, and overlap semantics keep every
        // referenced source strictly behind the destination.
        let limit = max_len.min(self.hist.len() - cand).min(chunk.len() - pos);
        if limit < 2 {
            return None;
        }
        let len = long_match_len(&self.hist, chunk, cand, pos, limit);
        (len >= 2).then_some((dist as u32, len))
    }
}

/// Compare `hist[cand..]` against `chunk[pos..]`, capped at `limit`
/// (both slices' remaining lengths included), using 64-bit word compares
/// with a scalar tail.
fn long_match_len(hist: &[u8], chunk: &[u8], cand: usize, pos: usize, limit: usize) -> usize {
    let mut l = 0;
    while l + 8 <= limit {
        let a = u64::from_le_bytes(hist[cand + l..cand + l + 8].try_into().unwrap());
        let b = u64::from_le_bytes(chunk[pos + l..pos + l + 8].try_into().unwrap());
        if a != b {
            return l + ((a ^ b).trailing_zeros() / 8) as usize;
        }
        l += 8;
    }
    while l < limit && hist[cand + l] == chunk[pos + l] {
        l += 1;
    }
    l
}

pub struct MatchFinder<'a> {
    data: &'a [u8],
    size: usize,
    head: Vec<i32>,
    prev: Vec<i32>,
    min_match: usize,
    max_match: usize,
    chain_len: usize,
    window: usize,
    hash_mask: usize,
    prev_mask: usize,
}

const HASH_BITS: usize = 20;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// Extend a match at `data[cand + start..]` vs `data[pos + start..]`,
/// capped at `limit`, using 64-bit word compares with a scalar tail.
/// Returns the total matched length (from `start`).
#[inline]
fn match_length(data: &[u8], cand: usize, pos: usize, start: usize, limit: usize) -> usize {
    let mut length = start;
    while length + 8 <= limit {
        let a = u64::from_le_bytes(data[cand + length..cand + length + 8].try_into().unwrap());
        let b = u64::from_le_bytes(data[pos + length..pos + length + 8].try_into().unwrap());
        if a != b {
            length += ((a ^ b).trailing_zeros() / 8) as usize;
            return length;
        }
        length += 8;
    }
    while length < limit && data[cand + length] == data[pos + length] {
        length += 1;
    }
    length
}

impl<'a> MatchFinder<'a> {
    pub fn new(
        data: &'a [u8],
        min_match: usize,
        max_match: usize,
        chain_len: usize,
        window: usize,
    ) -> Self {
        // The prev ring only needs to cover the LZ window: older slots are
        // safely aliased because the finder is rebuilt per chunk and every
        // chained candidate is strictly older than the current position.
        // Keeping the ring at window size keeps it hot in cache.
        //
        // The ring is capped at the input length so a huge declared
        // dictionary (RAR7 `-md` up to 64 GiB) can never allocate a
        // multi-GiB `prev` array (or overflow its i32 slots) — with
        // chunked compression the match finder only ever sees one
        // chunk + tail anyway.
        let prev_size = window.min(data.len()).next_power_of_two().max(1 << 17);
        MatchFinder {
            data,
            size: data.len(),
            head: vec![-1; HASH_SIZE],
            prev: vec![-1; prev_size],
            min_match,
            max_match,
            chain_len,
            window,
            hash_mask: HASH_SIZE - 1,
            prev_mask: prev_size - 1,
        }
    }

    #[inline]
    fn hash4(&self, pos: usize) -> usize {
        let d = self.data;
        let h = (d[pos] as u32)
            | ((d[pos + 1] as u32) << 8)
            | ((d[pos + 2] as u32) << 16)
            | ((d[pos + 3] as u32) << 24);
        ((h.wrapping_mul(0x9E3779B1)) >> 14) as usize & self.hash_mask
    }

    /// Insert position into the hash chain without searching.
    pub fn insert(&mut self, pos: usize) {
        if pos + 3 >= self.size {
            return;
        }
        let h = self.hash4(pos);
        self.prev[pos & self.prev_mask] = self.head[h];
        self.head[h] = pos as i32;
    }

    /// Find the best match at `pos`. Returns (distance, length) or (0, 0).
    pub fn find_match(&mut self, pos: usize) -> (usize, usize) {
        if pos + self.min_match > self.size {
            return (0, 0);
        }
        if pos + 3 >= self.size {
            return self.find_short(pos);
        }

        let h = self.hash4(pos);
        self.prev[pos & self.prev_mask] = self.head[h];
        self.head[h] = pos as i32;

        let data = self.data;
        let mut best_len = self.min_match - 1;
        let mut best_dist = 0;
        let max_len = self.max_match.min(self.size - pos);
        let mut chain_count = self.chain_len;

        let mut candidate = self.prev[pos & self.prev_mask];
        while candidate >= 0 && chain_count > 0 {
            let cand = candidate as usize;
            let dist = pos - cand;
            if dist == 0 || dist > self.window {
                break;
            }

            if data[cand + best_len] == data[pos + best_len]
                && data[cand] == data[pos]
                && data[cand + 1] == data[pos + 1]
            {
                let limit = max_len.min(self.size - cand);
                let length = match_length(data, cand, pos, 0, limit);
                if length > best_len || (length == best_len && dist < best_dist) {
                    best_len = length;
                    best_dist = dist;
                    if best_len >= max_len {
                        break;
                    }
                }
            }

            candidate = self.prev[cand & self.prev_mask];
            chain_count -= 1;
        }

        if best_len >= self.min_match {
            (best_dist, best_len)
        } else {
            (0, 0)
        }
    }

    fn find_short(&self, pos: usize) -> (usize, usize) {
        let data = self.data;
        let mut best_len = self.min_match - 1;
        let mut best_dist = 0;
        let max_len = self.max_match.min(self.size - pos);
        let max_dist = (pos + 1).min(self.window + 1).min(256);

        for dist in 1..max_dist {
            let cand = pos - dist;
            let limit = max_len.min(self.size - cand);
            let length = match_length(data, cand, pos, 0, limit);
            if length > best_len {
                best_len = length;
                best_dist = dist;
                if best_len >= max_len {
                    break;
                }
            }
        }

        if best_len >= self.min_match {
            (best_dist, best_len)
        } else {
            (0, 0)
        }
    }

    /// Find the best match, preferring cached distances.
    pub fn find_match_cached(
        &mut self,
        pos: usize,
        dist_cache: &[u32; DIST_CACHE_SIZE],
    ) -> (usize, usize) {
        if pos + self.min_match > self.size {
            return (0, 0);
        }

        let data = self.data;
        let max_len = self.max_match.min(self.size - pos);

        // Check cached distances
        let mut best_cache_dist = 0usize;
        let mut best_cache_len = 0usize;
        for &cached_dist in dist_cache {
            let cd = cached_dist as usize;
            if cd == 0 || cd > pos {
                continue;
            }
            let cand = pos - cd;
            // 3-byte prefilter before the O(len) comparison — the cache
            // rarely matches on non-repetitive data, so skipping the full
            // comparison avoids most of the work on random/unique input.
            // Bounds-check BOTH sides before the 3-byte prefilter: `pos` can
            // be within 2 bytes of the end (min_match = 2 passes the entry
            // guard), and reading `data[pos + 2]` there is out of bounds.
            // The tail (fewer than 3 bytes left) cannot form a cached 3-byte
            // match anyway; `find_match` handles the remaining bytes safely.
            if cand + 2 >= self.size || self.size - pos < 3 || data[cand] != data[pos] {
                continue;
            }
            if data[cand + 1] != data[pos + 1] || data[cand + 2] != data[pos + 2] {
                continue;
            }
            let limit = max_len.min(self.size - cand);
            let length = match_length(data, cand, pos, 3, limit);
            if length > best_cache_len && length >= self.min_match {
                best_cache_len = length;
                best_cache_dist = cd;
            }
        }

        let (normal_dist, normal_len) = self.find_match(pos);

        if best_cache_len > 0 && best_cache_len >= normal_len {
            (best_cache_dist, best_cache_len)
        } else {
            (normal_dist, normal_len)
        }
    }
}

/// Sentinel for "no position" in `head`/`son` links.
const NO_LINK: u32 = u32::MAX;

/// Reads a truncated link back as a position, given the newest position
/// (always the one being searched for).
///
/// The final subtraction is wrapping: [`TreeMatchFinder::rebase`] drops
/// out-of-window links by letting them wrap to huge values, and a wrapped
/// link must resolve to a huge position (past every real floor) so the
/// descent's `current >= floor` guard ends it — never a subtraction
/// underflow.
fn resolve(newest: usize, link: u32) -> usize {
    if link == NO_LINK {
        return usize::MAX;
    }
    newest.wrapping_sub((newest as u32).wrapping_sub(link) as usize)
}

/// A binary-tree match finder, after LZMA's BT4 (ported from the `rars`
/// project `codec/match_finder.rs`, MIT OR Apache-2.0).
///
/// Positions sharing a hash of their first four bytes form a binary search
/// tree ordered by the bytes at them, newest position at the root. One
/// descent finds every improving match and re-hangs the tree under the new
/// position, and each node on the way down narrows a proven-prefix bound,
/// so the descent never re-reads bytes it has already matched. That is
/// what a deep chain walk cannot avoid, and why the tree serves the
/// optimal parse, which searches at every position anyway. The price is
/// that inserting *is* a descent, so history cannot be seeded cheaply, and
/// the tree costs eight bytes per byte of window against the chain's four.
pub struct TreeMatchFinder {
    head: Vec<u32>,
    /// Two links per window slot: the child whose bytes compare lesser
    /// first, then the greater-or-equal one, in LZMA's layout.
    son: Vec<u32>,
    mask: usize,
}

impl TreeMatchFinder {
    const HASH_BITS: u32 = 17;
    const MIN_MATCH: usize = 4;

    /// Builds a finder that remembers the last `window` positions, at
    /// eight bytes of links per byte of window.
    pub fn new(window: usize) -> Self {
        let window = window.max(1).next_power_of_two();
        Self {
            head: vec![NO_LINK; 1 << Self::HASH_BITS],
            // Zero rather than the sentinel: a slot is always written
            // during its position's own insertion before any link can lead
            // to it, and untouched zeroes let the allocator defer the
            // pages.
            son: vec![0; window * 2],
            mask: window - 1,
        }
    }

    /// Grow the window in place, keeping every existing link (positions
    /// below the old mask map identically under a larger one: slot
    /// `(pos & old_mask) << 1` equals `(pos & new_mask) << 1` for every
    /// live position, so the old links are copied verbatim). Grows the
    /// `son` array when the window grew; shrinking keeps the larger
    /// allocation (the mask bounds the reachable window either way). Used
    /// by the persistent-finder path, where the finder spans chunks of one
    /// member instead of being rebuilt (and the tail re-seeded) each chunk.
    pub fn grow_to(&mut self, window: usize) {
        let window = window.max(1).next_power_of_two();
        if window * 2 > self.son.len() {
            // Copy, never wipe: the head table persists across chunks and
            // points at positions whose subtree links live in this array.
            // Replacing it with fresh zeros turned every unwritten slot
            // into a link to position 0 (0 is a valid position, only
            // `NO_LINK` marks empty), and the next descent followed those
            // zeros to the member head — a corrupt match. This was the
            // multi-chunk DLL corruption (bogus matches copying the MZ
            // header over real code).
            let old = std::mem::take(&mut self.son);
            let mut grown = vec![0; window * 2];
            grown[..old.len()].copy_from_slice(&old);
            self.son = grown;
        }
        self.mask = window - 1;
    }

    /// Clear the head table, keeping the `son` array (a slot is always
    /// written during its own position's insertion before any link can
    /// lead to it, so stale links are unreachable once the head is empty).
    /// Used when a reused finder starts a fresh frame (multi-threaded
    /// worker slices) instead of rebasing a continued one.
    pub fn clear_head(&mut self) {
        self.head.fill(NO_LINK);
    }

    /// Shift every stored link back by `sub`, dropping positions below it:
    /// a link whose value underflows wraps to a huge value, which `resolve`
    /// turns into a self-terminating descent (the `current >= floor` guard)
    /// — so dropped entries cost nothing and can never resolve to a live
    /// in-window position. Used when the persistent finder's frame slides.
    ///
    /// Links are *migrated*, not just value-shifted: the son array is
    /// indexed by `(pos & mask) << 1`, so a link belonging to old position
    /// `p` must move from slot `(p & mask) << 1` to slot
    /// `((p - sub) & mask) << 1` when the frame slides by `sub`. Shifting
    /// the values in place left them at the wrong slots, and the slots the
    /// new frame read were either stale or zero — and zero is a valid link
    /// to position 0, so the descent could jump to the member head and
    /// report a corrupt match (the multi-chunk DLL corruption).
    pub fn rebase(&mut self, sub: usize) {
        if sub == 0 {
            return;
        }
        let sub = sub as u32;
        // The head table's values are positions too: shift them, dropping
        // the ones that slid out of the frame (underflow wraps to a huge
        // value, which `resolve` self-terminates — safe, but the slot
        // stays; the parse overwrites it on the next insertion of that
        // hash).
        for slot in self.head.iter_mut() {
            if *slot != NO_LINK {
                *slot = slot.wrapping_sub(sub);
            }
        }
        // The son array is indexed by `(pos & mask) << 1`, so a link that
        // belonged to old position `p` must move to the slot of `p - sub`.
        // Shifting the values in place left them at the wrong slots, and
        // the slots the new frame read were either stale or zero — and
        // zero is a valid link to position 0, so the descent could jump to
        // the member head and report a corrupt match (the multi-chunk DLL
        // corruption). Only links whose position *and* value both survive
        // the slide are migrated; the rest are dropped.
        let mut remapped = vec![NO_LINK; self.son.len()];
        for (i, &slot) in self.son.iter().enumerate() {
            let pos = i >> 1;
            if slot != NO_LINK && pos >= sub as usize && slot >= sub {
                let new_pos = (pos - sub as usize) & self.mask;
                let side = i & 1;
                remapped[(new_pos << 1) | side] = slot - sub;
            }
        }
        self.son = remapped;
    }

    #[inline]
    fn hash4(input: &[u8], pos: usize) -> usize {
        let value =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        (value.wrapping_mul(0x9E37_79B1) >> (32 - Self::HASH_BITS)) as usize
    }

    /// Finds the matches at `pos` and inserts `pos`, in one descent.
    ///
    /// Pushes `(length, distance)` pairs onto `out` with both strictly
    /// increasing, so each pair is the nearest distance found that reaches
    /// its length; nothing shorter than four bytes or further than
    /// `max_distance` is reported. Comparing stops at `len_limit`, which
    /// must leave `pos + len_limit` readable; a pair whose length equals it
    /// may really extend further, which the caller measures if it cares. A
    /// node that matches the whole limit gives its place to the new
    /// position, since the two are interchangeable prefixes and the new one
    /// is nearer everything to come. `cut` bounds the nodes visited.
    pub fn matches(
        &mut self,
        input: &[u8],
        pos: usize,
        len_limit: usize,
        max_distance: usize,
        cut: usize,
        out: &mut Vec<(u32, u32)>,
    ) {
        if pos + Self::MIN_MATCH > input.len() {
            return;
        }
        let hash = Self::hash4(input, pos);
        let mut current = resolve(pos, self.head[hash]);
        self.head[hash] = pos as u32;
        // The two attachment points still waiting for a subtree, starting
        // as the new position's own child slots. Each step down hangs the
        // node just compared on one of them and moves that side into the
        // node's matching child slot.
        let mut ptr0 = ((pos & self.mask) << 1) + 1;
        let mut ptr1 = (pos & self.mask) << 1;
        // How much of the prefix is proven to match on each side of the
        // descent. Everything below a node compares the same way the node
        // did up to its recorded length, so bytes before the smaller of the
        // two never need reading again.
        let mut len0 = 0usize;
        let mut len1 = 0usize;
        let mut longest = Self::MIN_MATCH - 1;
        let mut budget = cut;
        let mut floor = pos;
        loop {
            // A candidate that does not step back is a reused slot or the
            // sentinel; one further back than the window has fallen out of
            // it. Either ends the descent, sealing both attachment points
            // so no stale link survives below them. A spent budget ends it
            // the same way, dropping whatever subtree the budget could not
            // reach.
            if current >= floor || pos - current > self.mask || budget == 0 {
                self.son[ptr0] = NO_LINK;
                self.son[ptr1] = NO_LINK;
                return;
            }
            budget -= 1;
            floor = current;
            let pair = (current & self.mask) << 1;
            let mut len = len0.min(len1);
            if input[current + len] == input[pos + len] {
                len += 1;
                while len < len_limit && input[current + len] == input[pos + len] {
                    len += 1;
                }
                if len > longest {
                    if pos - current <= max_distance {
                        out.push((len as u32, (pos - current) as u32));
                    }
                    longest = len;
                    if len == len_limit {
                        // The node's whole comparable prefix is the new
                        // position's prefix, so the new position adopts its
                        // children and the node drops out as the farther of
                        // two interchangeable candidates.
                        self.son[ptr1] = self.son[pair];
                        self.son[ptr0] = self.son[pair + 1];
                        return;
                    }
                }
            }
            if input[current + len] < input[pos + len] {
                self.son[ptr1] = current as u32;
                ptr1 = pair + 1;
                len1 = len;
                current = resolve(pos, self.son[ptr1]);
            } else {
                self.son[ptr0] = current as u32;
                ptr0 = pair;
                len0 = len;
                current = resolve(pos, self.son[ptr0]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_bytes(seed: u64, n: usize) -> Vec<u8> {
        let mut state = seed
            .wrapping_mul(0x2545_F491_4F6C_DD1D)
            .wrapping_add(0x9E37_79B9);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            out.push((state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8);
        }
        out
    }
    /// allocates nothing for the history and only a small initial table,
    /// growing as data is pushed (was: ~2x the declared dictionary up
    /// front — 64 MiB at the default 32 MiB dict, even for 1-byte files).
    #[test]
    fn long_range_memory_tracks_pushed_data() {
        let mut lr = LongRange::new(32 * 1024 * 1024); // default-like dict
        assert_eq!(lr.hist.capacity(), 0, "no upfront history allocation");
        assert_eq!(lr.table.cap(), TABLE_MIN_CAP, "small initial table");

        let data = random_bytes(42, 4 * 1024 * 1024);
        for chunk in data.chunks(64 * 1024) {
            lr.push(chunk);
        }
        assert_eq!(
            lr.hist_len(),
            4 * 1024 * 1024,
            "history retains pushed bytes"
        );
        assert!(
            lr.table.cap() > TABLE_MIN_CAP,
            "table must grow with the data"
        );
        // Growing to the 4 MiB history, not the 32 MiB dict.
        assert!(
            lr.table.cap() < LongRangeTable::max_cap_for(32 * 1024 * 1024),
            "table must not reach the full-dictionary size"
        );
    }

    /// Distant matches are found through the sampled table, but only up
    /// to the dictionary window: a copy at exactly window distance is
    /// emitted, one beyond it is rejected (the decoder window could not
    /// reach it — matches WinRAR, which also cannot compress beyond the
    /// dictionary).
    #[test]
    fn long_range_reach_is_bounded_by_the_dictionary() {
        // 32 KiB window: the history is capped at the window.
        let mut lr = LongRange::new(32 * 1024);
        let data = random_bytes(7, 32 * 1024);
        lr.push(&data);

        // Copy from hist[24 KiB..] (an aligned sample): distance ~8 KiB,
        // within the window — must be found.
        let chunk: Vec<u8> = data[24 * 1024..24 * 1024 + 64]
            .iter()
            .copied()
            .cycle()
            .take(256)
            .collect();
        let found = lr.find(&chunk, 0, 1024, 256);
        let (dist, len) = found.expect("in-window distant match must be found");
        assert!(
            (8192..8320).contains(&(dist as usize)),
            "expected ~8 KiB distance, got {dist}"
        );
        assert!(len >= 64, "expected a long match, got {len}");

        // Copy from hist[0..64] (aligned sample at offset 0) queried at
        // pos 2048: distance = 32 KiB + 2048, beyond the window — must be
        // rejected even though the bytes exist in the history.
        let beyond: Vec<u8> = data[..64].iter().copied().cycle().take(4096).collect();
        assert!(
            lr.find(&beyond, 2048, 1024, 256).is_none(),
            "beyond-window match must be rejected"
        );
    }

    /// Sliding: once more than the window is pushed, the history drops
    /// the oldest bytes and stays bounded while still finding matches in
    /// the retained range.
    #[test]
    fn long_range_window_slides_and_stays_bounded() {
        let mut lr = LongRange::new(64 * 1024);
        for i in 0..8u64 {
            lr.push(&random_bytes(i, 32 * 1024));
        }
        assert_eq!(lr.hist_len(), 64 * 1024, "history capped at the window");
        assert_eq!(lr.total_pushed(), 256 * 1024);
        // The oldest data slid out: the retained history starts at
        // 256 KiB - 64 KiB.
        assert_eq!(lr.hist_base(), 256 * 1024 - 64 * 1024);
    }
}
