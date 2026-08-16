/// RAR5 LZ match finder — hash-chain match finder for LZSS compression.
use super::tables::DIST_CACHE_SIZE;

/// Sampling step of the long-range hash table: one 4-byte sample per
/// `LONG_RANGE_STEP` bytes of history. Finer steps catch more matches at
/// higher memory cost; 16 bytes keeps the table at ~2x the history size
/// in memory (entries are 8 bytes and the table is double-hashed).
pub const LONG_RANGE_STEP: usize = 16;

/// Long-range history window: match distances beyond the near window
/// (tail + chunk, a few MiB) are found through a sampled hash table over
/// the most recent `LONG_RANGE_MAX` bytes of input. This bounds both the
/// history copy (we never rebuild the combined buffer) and the table
/// memory (~2x history in open addressing). WinRAR's `-mcl` long range
/// search works the same way (sampled, bounded memory), though its
/// window scales with the dictionary (up to 64 GiB); we cap at 128 MiB
/// so a full 128 MiB of history fits before the window slides (distant
/// copies up to that distance compress whole), at a peak cost of ~256
/// MiB (history + table) for multi-GiB inputs.
pub const LONG_RANGE_MAX: usize = 128 * 1024 * 1024;

/// Open-addressing hash table mapping a 4-byte sample hash to its most
/// recent position inside the long-range history (a relative offset).
///
/// Keys are stored as `hash + 1` so 0 marks an empty slot; values are
/// i32 because the history is bounded by [`LONG_RANGE_MAX`].
struct LongRangeTable {
    keys: Vec<u32>,
    vals: Vec<i32>,
    mask: usize,
}

impl LongRangeTable {
    /// Allocate a table sized for `history_max` bytes.
    fn new(history_max: usize) -> Self {
        let samples = history_max / LONG_RANGE_STEP;
        let cap = (samples * 2).max(1024).next_power_of_two();
        Self {
            keys: vec![0; cap],
            vals: vec![0; cap],
            mask: cap - 1,
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
    /// Maximum match distance (the encoder's dictionary window).
    window: usize,
    /// History bound: `LONG_RANGE_MAX.min(window)`.
    max_hist: usize,
}

impl LongRange {
    pub fn new(window: usize) -> Self {
        let max_hist = LONG_RANGE_MAX.min(window.max(LONG_RANGE_STEP));
        Self {
            hist: Vec::with_capacity(max_hist),
            table: LongRangeTable::new(max_hist),
            window,
            max_hist,
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
        let h = (data[pos] as u32)
            | ((data[pos + 1] as u32) << 8)
            | ((data[pos + 2] as u32) << 16)
            | ((data[pos + 3] as u32) << 24);
        h.wrapping_mul(0x9E3779B1)
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
        if pos + 4 > chunk.len() {
            return None;
        }
        let key = self.hash4(chunk, pos);
        let cand = self.table.get(key)? as usize;
        if cand >= self.hist.len() {
            return None;
        }
        let dist = self.hist.len() + pos - cand;
        if dist < min_dist || dist > self.window {
            return None;
        }
        let limit = max_len
            .min(self.hist.len() - cand)
            .min(chunk.len() - pos);
        if limit < 2 {
            return None;
        }
        let len = long_match_len(&self.hist, chunk, cand, pos, limit);
        (len >= 2).then_some((dist as u32, len))
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
        if chunk.len() >= self.max_hist {
            // A single chunk fills (or exceeds) the whole window: keep
            // only its tail and rebuild the table from scratch.
            let tail = &chunk[chunk.len() - self.max_hist..];
            self.hist.clear();
            self.table.clear();
            self.hist.extend_from_slice(tail);
            self.rebuild_table();
            return;
        }
        if self.hist.len() + chunk.len() > self.max_hist {
            let drop = (self.hist.len() + chunk.len() - self.max_hist / 2)
                .min(self.hist.len());
            self.hist.drain(0..drop);
            self.table.clear();
            self.rebuild_table();
        }
        let base = self.hist.len();
        self.hist.extend_from_slice(chunk);
        let mut off = base;
        while off + 4 <= self.hist.len() {
            let key = self.hash4(&self.hist, off);
            self.table.insert(key, off as i32);
            off += LONG_RANGE_STEP;
        }
    }

    fn rebuild_table(&mut self) {
        let mut off = 0usize;
        while off + 4 <= self.hist.len() {
            let key = self.hash4(&self.hist, off);
            self.table.insert(key, off as i32);
            off += LONG_RANGE_STEP;
        }
    }
}

/// Compare `hist[cand..]` against `chunk[pos..]`, capped at `limit`
/// (both slices' remaining lengths included), using 64-bit word compares
/// with a scalar tail.
fn long_match_len(
    hist: &[u8],
    chunk: &[u8],
    cand: usize,
    pos: usize,
    limit: usize,
) -> usize {
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
