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
        for (k, v) in self.keys.iter().zip(self.vals.iter()) {
            if *k != 0 {
                grown.insert(*k - 1, *v);
            }
        }
        *self = grown;
    }

    /// Ensure capacity for the current history, then rebuild the sample
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
    /// repeated keys (LZ favors the most recent candidate).
    fn insert(&mut self, key: u32, offset: i32) {
        let mut i = self.probe(key);
        let step = 1;
        loop {
            if self.keys[i] == 0 || self.keys[i] == key + 1 {
                self.keys[i] = key + 1;
                self.vals[i] = offset;
                return;
            }
            i = (i + step) & self.mask;
        }
    }

    /// Look up the most recent offset for `key`; `None` when absent.
    #[inline]
    fn get(&self, key: u32) -> Option<i32> {
        let mut i = self.probe(key);
        loop {
            match self.keys[i] {
                0 => return None,
                k if k == key + 1 => return Some(self.vals[i]),
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
        let key = self.hash4(chunk, pos);
        let cand = self.table.get(key)? as usize;
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

    /// Memory must track the actual history: a fresh long-range finder
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
