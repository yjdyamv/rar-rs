//! RAR5 bitstream reader and writer (MSB-first bit ordering).
// ── BitReader ──────────────────────────────────────────────────────────────
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            bit_pos: 0,
        }
    }

    #[inline]
    pub fn bits_remaining(&self) -> usize {
        if self.pos >= self.data.len() {
            0
        } else {
            (self.data.len() - self.pos) * 8 - self.bit_pos as usize
        }
    }

    #[inline]
    pub fn byte_position(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn bit_position(&self) -> u8 {
        self.bit_pos
    }

    /// Read `n` bits and return as a u32 (MSB-first). Max 32 bits.
    pub fn read_bits(&mut self, mut n: u8) -> Result<u32, &'static str> {
        if n == 0 {
            return Ok(0);
        }
        let mut result: u32 = 0;
        while n > 0 {
            if self.pos >= self.data.len() {
                return Err("BitReader: end of data");
            }
            let avail = 8 - self.bit_pos;
            let take = avail.min(n);
            let shift = avail - take;
            let mask = ((1u16 << take) - 1) as u8;
            let bits = (self.data[self.pos] >> shift) & mask;
            result = (result << take) | bits as u32;
            self.bit_pos += take;
            n -= take;
            if self.bit_pos >= 8 {
                self.bit_pos = 0;
                self.pos += 1;
            }
        }
        Ok(result)
    }

    /// Peek at `n` bits without consuming them.
    pub fn peek_bits(&self, n: u8) -> Result<u32, &'static str> {
        let mut copy = BitReader {
            data: self.data,
            pos: self.pos,
            bit_pos: self.bit_pos,
        };
        copy.read_bits(n)
    }

    /// Skip `n` bits.
    pub fn skip_bits(&mut self, n: u32) {
        let total = self.bit_pos as u32 + n;
        self.pos += (total / 8) as usize;
        self.bit_pos = (total % 8) as u8;
    }

    /// Advance to the next byte boundary.
    pub fn align(&mut self) {
        if self.bit_pos > 0 {
            self.bit_pos = 0;
            self.pos += 1;
        }
    }

    /// Read one aligned byte.
    pub fn read_byte(&mut self) -> Result<u8, &'static str> {
        self.align();
        if self.pos >= self.data.len() {
            return Err("BitReader: end of data");
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// Read `n` aligned bytes.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        self.align();
        if self.pos + n > self.data.len() {
            return Err("BitReader: end of data");
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Set absolute position (used for block boundary alignment).
    pub fn set_position(&mut self, byte_pos: usize, bit_pos: u8) {
        self.pos = byte_pos;
        self.bit_pos = bit_pos;
    }
}

// ── BitWriter ──────────────────────────────────────────────────────────────

pub struct BitWriter {
    buf: Vec<u8>,
    current_byte: u8,
    bit_pos: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        BitWriter {
            buf: Vec::new(),
            current_byte: 0,
            bit_pos: 0,
        }
    }

    /// Write `n` bits of `value` (MSB-first). Max 32 bits.
    pub fn write_bits(&mut self, value: u32, n: u8) {
        if n == 0 {
            return;
        }
        let mut remaining = n;
        while remaining > 0 {
            let avail = 8 - self.bit_pos;
            let take = avail.min(remaining);
            let shift = remaining - take;
            let bits = ((value >> shift) & ((1 << take) - 1)) as u8;
            self.current_byte |= bits << (avail - take);
            self.bit_pos += take;
            remaining -= take;
            if self.bit_pos >= 8 {
                self.buf.push(self.current_byte);
                self.current_byte = 0;
                self.bit_pos = 0;
            }
        }
    }

    /// Flush any partial byte (pad with zeros).
    pub fn flush_align(&mut self) {
        if self.bit_pos > 0 {
            self.buf.push(self.current_byte);
            self.current_byte = 0;
            self.bit_pos = 0;
        }
    }

    /// Total bits written so far.
    pub fn bit_count(&self) -> usize {
        self.buf.len() * 8 + self.bit_pos as usize
    }

    /// Get the written data, flushing any partial byte.
    pub fn into_bytes(mut self) -> Vec<u8> {
        self.flush_align();
        self.buf
    }
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_roundtrip() {
        let mut w = BitWriter::new();
        w.write_bits(0b110, 3);
        w.write_bits(0b01010, 5);
        w.write_bits(0xFF, 8);
        w.write_bits(0b101, 3);

        let _total = w.bit_count();
        let data = w.into_bytes();

        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(3).unwrap(), 0b110);
        assert_eq!(r.read_bits(5).unwrap(), 0b01010);
        assert_eq!(r.read_bits(8).unwrap(), 0xFF);
        assert_eq!(r.read_bits(3).unwrap(), 0b101);
    }

    #[test]
    fn peek_does_not_consume() {
        let data = [0b11010100];
        let r = BitReader::new(&data);
        assert_eq!(r.peek_bits(3).unwrap(), 0b110);
        assert_eq!(r.peek_bits(3).unwrap(), 0b110);
    }
}
