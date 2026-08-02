/// RAR5 LZ match finder — hash-chain match finder for LZSS compression.

use super::tables::DIST_CACHE_SIZE;

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

impl<'a> MatchFinder<'a> {
    pub fn new(
        data: &'a [u8],
        min_match: usize,
        max_match: usize,
        chain_len: usize,
        window: usize,
    ) -> Self {
        let prev_bits = window.next_power_of_two().max(1 << 17);
        let prev_size = prev_bits;
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
                let mut length = 0;
                while length < limit && data[cand + length] == data[pos + length] {
                    length += 1;
                }
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
            let mut length = 0;
            while length < limit && data[cand + length] == data[pos + length] {
                length += 1;
            }
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
            let mut length = 3;
            while length < limit && data[cand + length] == data[pos + length] {
                length += 1;
            }
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
