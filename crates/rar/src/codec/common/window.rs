//! RAR5 sliding window shared by the modern decoder and encoder.

/// RAR5 sliding window (circular buffer) for LZSS decompression.
pub struct SlidingWindow {
    buf: Vec<u8>,
    mask: usize,
    pos: usize,
    total_written: u64,
}

impl SlidingWindow {
    /// Create a new window of the given size.
    ///
    /// `size` must be a non-zero power of two. That invariant is asserted in
    /// debug builds only: every caller derives the size from a dictionary
    /// size, which the RAR5/RAR7 format restricts to powers of two.
    pub fn new(size: usize) -> Self {
        debug_assert!(size.is_power_of_two());
        SlidingWindow {
            buf: vec![0u8; size],
            mask: size - 1,
            pos: 0,
            total_written: 0,
        }
    }

    #[inline]
    pub fn total_written(&self) -> u64 {
        self.total_written
    }

    /// Number of bytes the ring can hold. `get_output` can only return a
    /// region no longer than this, so a caller materializing more than the
    /// ring holds must bound the request.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Grow the ring to `new_size` (a larger power of two), carrying the
    /// lookbehind tail forward: the last `min(old capacity, total_written)`
    /// bytes stay addressable at their stream offsets, the write cursor and
    /// `total_written` are preserved. Smaller or equal sizes are a no-op.
    ///
    /// `new_size` must be a power of two (asserted in debug builds; callers
    /// are internal and derive it from a dictionary size).
    ///
    /// A solid-chain continuation may declare a dictionary larger than the
    /// chain head's; growing (rather than rejecting) matches the reference
    /// reader, whose window is sized by the largest dictionary it has seen.
    pub fn grow(&mut self, new_size: usize) {
        if new_size <= self.buf.len() {
            return;
        }
        debug_assert!(new_size.is_power_of_two());
        let preserve = usize::try_from(self.total_written)
            .unwrap_or(usize::MAX)
            .min(self.buf.len());
        let tail = self.get_output(self.total_written - preserve as u64, preserve);
        let new_pos = (self.total_written % new_size as u64) as usize;
        let mut buf = vec![0u8; new_size];
        for (i, &b) in tail.iter().enumerate() {
            buf[(new_pos + new_size - preserve + i) % new_size] = b;
        }
        self.buf = buf;
        self.mask = new_size - 1;
        self.pos = new_pos;
    }

    /// Write a single literal byte.
    #[inline]
    pub fn put_byte(&mut self, b: u8) {
        self.buf[self.pos] = b;
        self.pos = (self.pos + 1) & self.mask;
        self.total_written += 1;
    }

    /// Copy `length` bytes from `dist` bytes back.
    /// Handles overlapping copies correctly.
    pub fn copy_match(&mut self, dist: usize, length: usize) {
        let mut src = (self.pos.wrapping_sub(dist)) & self.mask;
        let mut dst = self.pos;
        for _ in 0..length {
            self.buf[dst] = self.buf[src];
            src = (src + 1) & self.mask;
            dst = (dst + 1) & self.mask;
        }
        self.pos = dst;
        self.total_written += length as u64;
    }

    /// Extract `length` bytes starting from a total-written offset.
    pub fn get_output(&self, start_total: u64, length: usize) -> Vec<u8> {
        let buf_size = self.buf.len();
        assert!(length <= buf_size, "requested output exceeds window size");
        let offset = (self.total_written - start_total) as usize;
        let start_pos = (self.pos.wrapping_sub(offset)) & self.mask;
        if start_pos + length <= buf_size {
            self.buf[start_pos..start_pos + length].to_vec()
        } else {
            let first = buf_size - start_pos;
            let mut out = Vec::with_capacity(length);
            out.extend_from_slice(&self.buf[start_pos..]);
            out.extend_from_slice(&self.buf[..length - first]);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_output() {
        let mut w = SlidingWindow::new(256);
        for b in b"hello" {
            w.put_byte(*b);
        }
        assert_eq!(w.total_written(), 5);
        assert_eq!(&w.get_output(0, 5), b"hello");
    }

    #[test]
    fn copy_match_non_overlapping() {
        let mut w = SlidingWindow::new(256);
        for b in b"abcd" {
            w.put_byte(*b);
        }
        w.copy_match(4, 4); // copy "abcd" again
        assert_eq!(&w.get_output(0, 8), b"abcdabcd");
    }

    #[test]
    fn copy_match_overlapping() {
        let mut w = SlidingWindow::new(256);
        w.put_byte(b'a');
        w.copy_match(1, 5); // repeat 'a' 5 times
        assert_eq!(&w.get_output(0, 6), b"aaaaaa");
    }

    /// Fill past capacity (so the write cursor wraps and the ring is full of
    /// wrapped data), then grow and check the whole obtainable tail against a
    /// plain `Vec` model of the same stream.
    #[test]
    fn grow_preserves_the_wrapped_tail_and_cursor() {
        let mut window = SlidingWindow::new(64);
        let mut model: Vec<u8> = Vec::new();
        let mut state = 0x1234_5678u32;
        for _ in 0..300 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let byte = (state >> 24) as u8;
            window.put_byte(byte);
            model.push(byte);
        }
        // A `copy_match` advancing the ring too, so both write paths touch
        // the wrapped region.
        window.copy_match(5, 40);
        let src = model.len() - 5;
        for i in 0..40 {
            let byte = model[src + i];
            model.push(byte);
        }

        let total = model.len() as u64;
        assert_eq!(window.total_written(), total);
        let old_capacity = window.capacity();
        for len in 1..=old_capacity {
            assert_eq!(
                window.get_output(total - len as u64, len),
                model[model.len() - len..],
                "pre-grow tail of {len} bytes"
            );
        }

        window.grow(256);
        assert_eq!(window.capacity(), 256);
        assert_eq!(window.total_written(), total, "total_written must survive");
        // Every byte the old ring could serve is still at the same stream
        // offset.
        for len in 1..=old_capacity {
            assert_eq!(
                window.get_output(total - len as u64, len),
                model[model.len() - len..],
                "post-grow tail of {len} bytes"
            );
        }

        // The new headroom becomes addressable as new data is written: after
        // more than `new_size` bytes the 250-byte tail crosses the ring's
        // wrap and must still match the model.
        for _ in 0..300 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let byte = (state >> 24) as u8;
            window.put_byte(byte);
            model.push(byte);
        }
        let total = model.len() as u64;
        assert_eq!(window.total_written(), total);
        assert_eq!(
            window.get_output(total - 250, 250),
            model[model.len() - 250..]
        );

        // The cursor continues where the stream left off: new writes land at
        // `total` and read back through `get_output`.
        for byte in [0xAA, 0x55, 0x00, 0xFF] {
            window.put_byte(byte);
            model.push(byte);
        }
        assert_eq!(window.total_written(), model.len() as u64);
        assert_eq!(window.get_output(total, 4), [0xAA, 0x55, 0x00, 0xFF]);
        assert_eq!(window.get_output(total - 50, 54), model[model.len() - 54..]);
    }

    /// Growing to the same or a smaller size is a no-op: capacity and the
    /// cursor stay put.
    #[test]
    fn grow_to_smaller_size_is_a_no_op() {
        let mut window = SlidingWindow::new(64);
        for byte in 0..100u8 {
            window.put_byte(byte);
        }
        let total = window.total_written();
        window.grow(64);
        window.grow(32);
        assert_eq!(window.capacity(), 64);
        assert_eq!(window.total_written(), total);
        window.put_byte(0xEE);
        assert_eq!(window.get_output(total, 1), [0xEE]);
    }
}
