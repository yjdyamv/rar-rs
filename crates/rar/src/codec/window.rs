/// RAR5 sliding window (circular buffer) for LZSS decompression.
pub struct SlidingWindow {
    buf: Vec<u8>,
    mask: usize,
    pos: usize,
    total_written: u64,
}

impl SlidingWindow {
    /// Create a new window of the given size (must be a power of 2).
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
    pub fn position(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn total_written(&self) -> u64 {
        self.total_written
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

    /// Read the byte `dist` positions back from the current write position.
    #[inline]
    pub fn get_byte_at(&self, dist: usize) -> u8 {
        self.buf[(self.pos.wrapping_sub(dist)) & self.mask]
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
}
