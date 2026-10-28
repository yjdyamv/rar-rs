//! RAR5 bitstream reader and writer (MSB-first bit ordering).
// ── BitReader ──────────────────────────────────────────────────────────────
/// A shifted bit buffer, the shape every fast Huffman decoder uses: bytes are
/// pulled in eight at a time into a `u64` whose high `bits` bits are the
/// unconsumed stream, so a peek is a shift and a skip is a shift. Decoding cost
/// used to go through a per-peek byte copy; this is what closed most of the
/// remaining gap to official UnRAR.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Index of the next byte to pull into `buf`.
    pos: usize,
    /// Unconsumed bits, MSB-aligned.
    buf: u64,
    /// Valid bits in `buf`, always `..=64`.
    bits: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            buf: 0,
            bits: 0,
        }
    }

    /// Top the buffer up to at least 56 bits while the stream has whole bytes
    /// left. Eight bytes are loaded at once and shifted into place, which is
    /// one unaligned load per eight symbols instead of one per peek.
    #[inline]
    fn fill(&mut self) {
        while self.bits < 56 {
            if self.pos + 8 <= self.data.len() {
                let word = u64::from_be_bytes(
                    self.data[self.pos..self.pos + 8]
                        .try_into()
                        .expect("eight bytes"),
                );
                self.buf |= word >> self.bits;
                // Whole bytes that still fit above the bits already buffered.
                let added = ((63 - self.bits) / 8) as usize;
                if added == 0 {
                    break;
                }
                self.pos += added;
                self.bits += (added as u32) * 8;
            } else if self.pos < self.data.len() {
                self.buf |= (self.data[self.pos] as u64) << (56 - self.bits);
                self.pos += 1;
                self.bits += 8;
            } else {
                break;
            }
        }
    }

    #[inline]
    pub fn bits_remaining(&self) -> usize {
        self.bits as usize + (self.data.len() - self.pos) * 8
    }

    /// Byte offset of the next unconsumed bit.
    #[inline]
    pub fn byte_position(&self) -> usize {
        self.pos - (self.bits as usize).div_ceil(8)
    }

    /// Bit offset inside [`Self::byte_position`]'s byte.
    #[inline]
    pub fn bit_position(&self) -> u8 {
        ((8 - (self.bits % 8)) % 8) as u8
    }

    /// Read `n` bits and return as a u32 (MSB-first). Max 32 bits.
    pub fn read_bits(&mut self, n: u8) -> Result<u32, &'static str> {
        if n == 0 {
            return Ok(0);
        }
        if n <= 32 {
            let value = self.peek_bits(n)?;
            self.skip_bits(n as u32);
            return Ok(value);
        }
        let mut result: u32 = 0;
        let mut left = n;
        while left > 0 {
            let take = left.min(16);
            result = (result << take) | (self.read_bits(take)? & ((1u32 << take) - 1));
            left -= take;
        }
        Ok(result)
    }

    /// Peek at `n` bits without consuming them; `n <= 32`.
    pub fn peek_bits(&mut self, n: u8) -> Result<u32, &'static str> {
        if n == 0 {
            return Ok(0);
        }
        self.fill();
        if (n as usize) > self.bits_remaining() || (n as u32) > self.bits {
            // Near the end the buffer may hold fewer than `n` bits while the
            // stream has them in unloaded bytes, but `fill` already drained
            // every whole byte, so anything left is the padded tail.
            return Err("BitReader: end of data");
        }
        Ok((self.buf >> (64 - n as u32)) as u32)
    }

    /// Read `n` bits and return as a u64 (MSB-first). Max 64 bits; kept
    /// separate from the hot `read_bits` so only the RAR7 extended-distance
    /// path pays for the wider type.
    pub fn read_bits_u64(&mut self, mut n: u8) -> Result<u64, &'static str> {
        let mut result = 0u64;
        while n > 32 {
            let take = n - 32;
            result = (result << take) | self.read_bits(take)? as u64;
            n = 32;
        }
        result = (result << n) | self.read_bits(n)? as u64;
        Ok(result)
    }

    /// Skip `n` bits, buffering them first if the caller has not peeked.
    ///
    /// Skipping past the end simply empties the buffer; a following peek then
    /// reports the end of data, which is how the decoder stops exactly where a
    /// member ends.
    #[inline]
    pub fn skip_bits(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        if (n as usize) > self.bits as usize {
            self.fill();
        }
        if n >= self.bits {
            self.buf = 0;
            self.bits = 0;
        } else {
            self.buf <<= n;
            self.bits -= n;
        }
    }

    /// Advance to the next byte boundary.
    pub fn align(&mut self) {
        let drop = self.bits % 8;
        if drop > 0 {
            self.skip_bits(drop);
        }
    }

    /// Read one aligned byte.
    pub fn read_byte(&mut self) -> Result<u8, &'static str> {
        self.align();
        self.fill();
        if self.bits < 8 {
            return Err("BitReader: end of data");
        }
        let byte = (self.buf >> 56) as u8;
        self.skip_bits(8);
        Ok(byte)
    }

    /// Read `n` aligned bytes.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        self.align();
        let start = self.byte_position();
        if start + n > self.data.len() {
            return Err("BitReader: end of data");
        }
        self.set_position(start + n, 0);
        Ok(&self.data[start..start + n])
    }

    /// Set absolute position (used for block boundary alignment).
    pub fn set_position(&mut self, byte_pos: usize, bit_pos: u8) {
        self.pos = byte_pos;
        self.buf = 0;
        self.bits = 0;
        if bit_pos > 0 {
            self.fill();
            if (bit_pos as u32) <= self.bits {
                self.skip_bits(bit_pos as u32);
            }
        }
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

    /// Hand every complete byte written so far to `writer`, keeping the
    /// partial byte (and its bit position) so the bit stream continues where
    /// it left off. A streaming encoder calls this per chunk so it never
    /// holds a whole member's packed output; the caller decides what error
    /// type an I/O failure becomes.
    pub fn drain_to<W: std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        writer.write_all(&self.buf)?;
        self.buf.clear();
        Ok(())
    }

    /// Pad and hand over the final partial byte, ending the bit stream.
    pub fn finish_to<W: std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.flush_align();
        self.drain_to(writer)
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
        let mut r = BitReader::new(&data);
        assert_eq!(r.peek_bits(3).unwrap(), 0b110);
        assert_eq!(r.peek_bits(3).unwrap(), 0b110);
    }

    /// The RAR7 extended-distance path reads up to 34-bit fields; the u64
    /// reader must not truncate like a u32 read would.
    #[test]
    fn read_bits_u64_carries_more_than_32_bits() {
        // 0x15_2345_6789: 37 significant bits, written as 40.
        let mut w = BitWriter::new();
        w.write_bits(0x15, 8);
        w.write_bits(0x2345_6789, 32);
        let data = w.into_bytes();

        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits_u64(40).unwrap(), 0x15_2345_6789);
    }

    /// The buffered reader must agree with a bit-at-a-time walk of the same
    /// bytes, whatever widths are mixed — that equivalence is what keeps the
    /// shift-register rewrite byte-transparent.
    #[test]
    fn buffered_reads_match_a_bit_by_bit_walk() {
        let data: Vec<u8> = (0..37u16)
            .map(|i| (i.wrapping_mul(37) & 0xff) as u8)
            .collect();
        let widths = [1u8, 3, 15, 7, 32, 2, 13, 5, 8, 17, 4, 25, 6];
        let mut r = BitReader::new(&data);
        let mut consumed = 0usize;
        for width in widths.iter().copied().cycle().take(200) {
            if consumed + width as usize > data.len() * 8 {
                break;
            }
            let mut expected = 0u32;
            for bit in 0..width as usize {
                let index = consumed + bit;
                let byte = data[index / 8];
                expected = (expected << 1) | ((byte >> (7 - index % 8)) & 1) as u32;
            }
            assert_eq!(r.read_bits(width).unwrap(), expected, "width {width}");
            consumed += width as usize;
            assert_eq!(r.bits_remaining(), data.len() * 8 - consumed);
            assert_eq!(r.byte_position(), consumed / 8);
            assert_eq!(r.bit_position() as usize, consumed % 8);
        }
    }

    /// Peeking past the end is an error, not a zero-padded read: the decoder
    /// relies on it to stop at the exact end of a member.
    #[test]
    fn peek_past_the_end_is_an_error() {
        let data = [0xffu8];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(8).unwrap(), 0xff);
        assert!(r.peek_bits(1).is_err());
        assert!(r.peek_bits(32).is_err());
        assert_eq!(r.bits_remaining(), 0);
    }

    /// `set_position` must seek to an arbitrary bit offset, which is how the
    /// decoder lands exactly on a block boundary it never stepped through.
    #[test]
    fn set_position_seeks_to_a_bit_offset() {
        let data: Vec<u8> = (0..64u8).collect();
        for byte in [0usize, 1, 7, 9, 33, 60] {
            for bit in 0..8u8 {
                let mut r = BitReader::new(&data);
                r.set_position(byte, bit);
                assert_eq!(r.byte_position(), byte, "byte {byte} bit {bit}");
                assert_eq!(r.bit_position(), bit, "byte {byte} bit {bit}");
                if byte + 2 < data.len() {
                    // The same 16 bits, read straight out of the byte stream.
                    let window = (u32::from(data[byte]) << 16)
                        | (u32::from(data[byte + 1]) << 8)
                        | u32::from(data[byte + 2]);
                    let expected = (window >> (8 - bit as u32)) & 0xffff;
                    assert_eq!(r.read_bits(16).unwrap(), expected, "byte {byte} bit {bit}");
                }
            }
        }
    }

    /// Aligned reads must agree with the byte stream, including when the
    /// position sits just past a buffer refill.
    #[test]
    fn aligned_reads_return_stream_bytes() {
        let data: Vec<u8> = (0..40u8).collect();
        let mut r = BitReader::new(&data);
        r.skip_bits(5);
        r.align();
        assert_eq!(r.byte_position(), 1);
        assert_eq!(r.read_byte().unwrap(), 1);
        assert_eq!(r.read_bytes(3).unwrap(), &[2, 3, 4]);
        assert_eq!(r.byte_position(), 5);
        assert_eq!(r.read_byte().unwrap(), 5);
    }

    #[test]
    fn read_bits_u64_matches_read_bits_up_to_32() {
        let mut w = BitWriter::new();
        w.write_bits(0xDEAD_BEEF, 32);
        let data = w.into_bytes();

        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits_u64(32).unwrap(), u64::from(0xDEAD_BEEFu32));
        assert_eq!(r.bits_remaining(), 0);
    }
}
