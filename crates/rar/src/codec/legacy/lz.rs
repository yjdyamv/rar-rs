//! Shared LZSS/Huffman decoder core for the legacy RAR 2.x and 3.x/4.x
//! members.
//!
//! The two decoders differ in their table shapes, audio prediction, PPMd and
//! VM-filter handling, but the MSB-first bit reader, the canonical Huffman
//! tables, the level-run filler and the sliding history (match copy
//! included) are one machine. It lives here once so an edge-case fix cannot
//! land in only one of the two files.

use super::ppmd;

use crate::error::RarError;

/// Internal stream error shared by the legacy decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    /// The packed stream ended before the decoder needed more bits.
    Truncated,
    /// Structurally invalid stream data.
    Bad(&'static str),
}

impl Error {
    /// Map to the caller-facing error, labelled with the family's stream
    /// name (`"RAR 2.0"`, `"RAR 2.9"`).
    pub(super) fn into_rar(self, stream: &'static str) -> RarError {
        match self {
            Error::Bad(message) => RarError::Format(format!("{stream} stream: {message}")),
            Error::Truncated => RarError::Format(format!("{stream} bitstream is truncated")),
        }
    }
}

pub(super) type Res<T> = Result<T, Error>;

// ── Bit reader (MSB-first) ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(super) struct BitReader {
    input: Vec<u8>,
    bit_pos: usize,
}

impl BitReader {
    pub(super) fn new() -> Self {
        Self {
            input: Vec::new(),
            bit_pos: 0,
        }
    }

    pub(super) fn append(&mut self, input: &[u8]) {
        self.compact();
        self.input.extend_from_slice(input);
    }

    fn compact(&mut self) {
        let bytes = self.bit_pos / 8;
        if bytes == 0 {
            return;
        }
        self.input.drain(..bytes);
        self.bit_pos -= bytes * 8;
    }

    pub(super) fn align_byte(&mut self) {
        self.bit_pos = (self.bit_pos + 7) & !7;
    }

    pub(super) fn peek_bit(&self) -> Res<u8> {
        self.peek_bits(1).map(|value| value as u8)
    }

    pub(super) fn read_bit(&mut self) -> Res<u8> {
        self.read_bits(1).map(|value| value as u8)
    }

    pub(super) fn read_bits(&mut self, count: u8) -> Res<u32> {
        let value = self.peek_bits(count)?;
        self.bit_pos += count as usize;
        Ok(value)
    }

    pub(super) fn peek_bits(&self, count: u8) -> Res<u32> {
        if count > 24 {
            return Err(Error::Bad("bit read is too wide"));
        }
        let mut value = 0u32;
        for i in 0..count as usize {
            let bit_index = self.bit_pos + i;
            let byte = *self.input.get(bit_index / 8).ok_or(Error::Truncated)?;
            let bit = (byte >> (7 - (bit_index % 8))) & 1;
            value = (value << 1) | bit as u32;
        }
        Ok(value)
    }

    pub(super) fn remaining_bytes_from_current(&self) -> usize {
        self.input.len().saturating_sub(self.bit_pos / 8)
    }

    /// A reader over a standalone byte slice (VM filter record bodies).
    pub(super) fn from_bytes(input: &[u8]) -> Self {
        Self {
            input: input.to_vec(),
            bit_pos: 0,
        }
    }

    /// RARVM variable-length integer (2-bit tag + payload).
    pub(super) fn read_encoded_u32(&mut self) -> Res<u32> {
        match self.read_bits(2)? {
            0 => self.read_bits(4),
            1 => {
                let high = self.read_bits(8)?;
                if high >= 16 {
                    Ok(high)
                } else {
                    Ok(0xffff_ff00 | (high << 4) | self.read_bits(4)?)
                }
            }
            2 => self.read_bits(16),
            _ => Ok((self.read_bits(16)? << 16) | self.read_bits(16)?),
        }
    }
}

impl ppmd::PpmdByteReader for BitReader {
    fn read_ppmd_byte(&mut self) -> ppmd::Result<u8> {
        self.read_bits(8)
            .map(|value| value as u8)
            .map_err(|error| match error {
                Error::Truncated => ppmd::Error::NeedMoreInput,
                Error::Bad(message) => ppmd::Error::InvalidData(message),
            })
    }
}

// ── Canonical Huffman tables ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct HuffmanSymbol {
    code: u16,
    len: u8,
    symbol: usize,
}

#[derive(Debug, Clone)]
pub(super) struct Huffman {
    symbols: Vec<HuffmanSymbol>,
    first_code: [u16; 16],
    first_index: [usize; 16],
    counts: [u16; 16],
}

impl Huffman {
    pub(super) fn empty() -> Self {
        Self {
            symbols: Vec::new(),
            first_code: [0; 16],
            first_index: [0; 16],
            counts: [0; 16],
        }
    }

    pub(super) fn from_lengths(lengths: &[u8]) -> Res<Self> {
        let mut count = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return Err(Error::Bad("Huffman length is too large"));
            }
            if len != 0 {
                count[len as usize] += 1;
            }
        }
        if count.iter().all(|&value| value == 0) {
            return Ok(Self::empty());
        }
        validate_huffman_counts(&count)?;

        let mut first_code = [0u16; 16];
        let mut next_code = [0u16; 16];
        let mut code = 0u16;
        for len in 1..=15 {
            code = (code + count[len - 1]) << 1;
            first_code[len] = code;
            next_code[len] = code;
        }

        let mut first_index = [0usize; 16];
        let mut index = 0usize;
        for len in 1..=15 {
            first_index[len] = index;
            index += usize::from(count[len]);
        }

        let mut symbols = Vec::new();
        for (symbol, &len) in lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let code = next_code[len as usize];
            next_code[len as usize] += 1;
            symbols.push(HuffmanSymbol { code, len, symbol });
        }
        symbols.sort_by_key(|item| (item.len, item.code, item.symbol));
        Ok(Self {
            symbols,
            first_code,
            first_index,
            counts: count,
        })
    }

    pub(super) fn decode(&self, bits: &mut BitReader) -> Res<usize> {
        let mut code = 0u16;
        if self.symbols.is_empty() {
            return Err(Error::Bad("empty Huffman table"));
        }
        for len in 1..=15 {
            code = (code << 1) | u16::from(bits.read_bit()?);
            let count = self.counts[len];
            if count != 0 {
                let first = self.first_code[len];
                let offset = code.wrapping_sub(first);
                if offset < count {
                    let index = self.first_index[len] + usize::from(offset);
                    return Ok(self.symbols[index].symbol);
                }
            }
        }
        Err(Error::Bad("invalid Huffman code"))
    }

    /// Whether no table was built yet (`empty()`): RAR 2.x skips its
    /// trailing end-of-block probe in that case.
    pub(super) fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
}

fn validate_huffman_counts(count: &[u16; 16]) -> Res<()> {
    let mut available = 1i32;
    for &len_count in count.iter().skip(1) {
        available = (available << 1) - i32::from(len_count);
        if available < 0 {
            return Err(Error::Bad("oversubscribed Huffman table"));
        }
    }
    Ok(())
}

/// Fill a level run, clamping at the table end (unrar tolerates runs that
/// overshoot the smaller audio table).
pub(super) fn fill_levels(levels: &mut [u8], pos: &mut usize, count: usize, value: u8) -> Res<()> {
    let end = pos
        .checked_add(count)
        .ok_or(Error::Bad("table run overflows"))?;
    let end = end.min(levels.len());
    for item in &mut levels[*pos..end] {
        *item = value;
    }
    *pos = end;
    Ok(())
}

/// Shift an offset into the most-recent-first ring: `old_offsets[0]` is the
/// last match distance.
pub(super) fn push_old_offset(old_offsets: &mut [usize; 4], offset: usize) {
    old_offsets[3] = old_offsets[2];
    old_offsets[2] = old_offsets[1];
    old_offsets[1] = old_offsets[0];
    old_offsets[0] = offset;
}

// ── Sliding history ────────────────────────────────────────────────────────

/// The decoded output window shared by the legacy decoders: bytes since the
/// last trim (up to `limit` of look-behind), plus a match parked across a
/// flush boundary.
#[derive(Debug)]
pub(super) struct History {
    output: Vec<u8>,
    base_offset: usize,
    limit: usize,
    pending_match: Option<(usize, usize)>,
}

impl History {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            output: Vec::new(),
            base_offset: 0,
            limit,
            pending_match: None,
        }
    }

    pub(super) fn current_pos(&self) -> usize {
        self.base_offset + self.output.len()
    }

    fn raw_byte(&self, position: usize) -> Option<&u8> {
        self.output.get(position.checked_sub(self.base_offset)?)
    }

    pub(super) fn raw_range(&self, start: usize, end: usize) -> Res<&[u8]> {
        if start < self.base_offset || end < start {
            return Err(Error::Bad("retained history is unavailable"));
        }
        let rel_start = start - self.base_offset;
        let rel_end = end - self.base_offset;
        self.output
            .get(rel_start..rel_end)
            .ok_or(Error::Bad("retained history is unavailable"))
    }

    pub(super) fn push(&mut self, byte: u8) {
        self.output.push(byte);
    }

    /// Drop decoded history beyond the sliding window, never past the last
    /// flush. Returns the position the window now starts at, so a family can
    /// drop state anchored before it.
    pub(super) fn trim(&mut self, flushed_pos: usize) -> usize {
        let keep_from = self
            .current_pos()
            .saturating_sub(self.limit)
            .min(flushed_pos);
        if keep_from > self.base_offset {
            let drain = keep_from - self.base_offset;
            self.output.drain(..drain);
            self.base_offset = keep_from;
        }
        self.base_offset
    }

    /// Copy `length` bytes from `offset` back in the window.
    ///
    /// The bitstream normally encodes match distances as offset+1, so zero is
    /// not emitted for fresh matches; the legacy decoder boundary is tolerant
    /// here and treats a zero internal offset as distance one. A match
    /// reaching past the start of the stream writes zeroes rather than
    /// failing: WinRAR never clears its window and guards the copy with a
    /// first-wrap flag instead, so those bytes read as zero there, and an
    /// archive that leans on it stays readable here. The decision is taken
    /// once for the whole match, as it is there: a copy does not start on
    /// zeroes and cross into real bytes partway. A copy that reaches
    /// `output_size` parks its tail in `pending_match` for the next flush.
    pub(super) fn copy_match(
        &mut self,
        length: usize,
        offset: usize,
        output_size: usize,
    ) -> Res<()> {
        let offset = if offset == 0 { 1 } else { offset };
        let before_window = offset > self.current_pos();
        for index in 0..length {
            if self.current_pos() >= output_size {
                self.pending_match = Some((length - index, offset));
                break;
            }
            let byte = if before_window {
                0
            } else {
                let src = self.current_pos() - offset;
                *self
                    .raw_byte(src)
                    .ok_or(Error::Bad("match distance is out of range"))?
            };
            self.push(byte);
        }
        Ok(())
    }

    pub(super) fn drain_pending_match(&mut self, output_size: usize) -> Res<()> {
        let Some((length, offset)) = self.pending_match.take() else {
            return Ok(());
        };
        self.copy_match(length, offset, output_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_huffman_table_decodes_uniform_codes() {
        // Uniform 2-bit code over four symbols: canonical codes 00/01/10/11.
        let lengths = [2u8, 2, 2, 2];
        let table = Huffman::from_lengths(&lengths).expect("build");
        assert_eq!(table.counts[2], 4);
        let mut bits = BitReader::new();
        bits.append(&[0b0001_1011]); // 00 01 10 11
        assert_eq!(table.decode(&mut bits).unwrap(), 0);
        assert_eq!(table.decode(&mut bits).unwrap(), 1);
        assert_eq!(table.decode(&mut bits).unwrap(), 2);
        assert_eq!(table.decode(&mut bits).unwrap(), 3);
    }

    #[test]
    fn all_zero_lengths_build_an_empty_table() {
        let table = Huffman::from_lengths(&[0u8; 4]).expect("build");
        assert!(table.is_empty());
        assert!(matches!(
            table.decode(&mut BitReader::new()),
            Err(Error::Bad("empty Huffman table"))
        ));
    }

    #[test]
    fn oversubscribed_table_rejected() {
        let mut count = [0u16; 16];
        count[1] = 3; // three 1-bit codes can never fit
        assert!(validate_huffman_counts(&count).is_err());
    }

    #[test]
    fn bit_reader_reports_truncation_and_tracks_remaining_bytes() {
        let mut bits = BitReader::new();
        bits.append(&[0b1000_0000]);
        assert_eq!(bits.read_bit().unwrap(), 1);
        assert_eq!(bits.remaining_bytes_from_current(), 1);
        assert_eq!(bits.read_bits(7).unwrap(), 0);
        assert_eq!(bits.remaining_bytes_from_current(), 0);
        assert!(matches!(bits.read_bit(), Err(Error::Truncated)));
    }

    #[test]
    fn history_copy_match_zero_fills_a_match_before_the_window() {
        let mut history = History::new(1024);
        history.push(1);
        history.copy_match(3, 5, 1024).unwrap();
        assert_eq!(history.raw_range(0, 4).unwrap(), &[1, 0, 0, 0]);
    }

    #[test]
    fn history_copy_match_parks_the_tail_for_the_next_flush() {
        let mut history = History::new(1024);
        history.push(7);
        history.push(8);
        history.copy_match(4, 2, 3).unwrap();
        assert_eq!(history.current_pos(), 3);
        history.drain_pending_match(6).unwrap();
        assert_eq!(history.current_pos(), 6);
        assert_eq!(history.raw_range(0, 6).unwrap(), &[7, 8, 7, 8, 7, 8]);
    }

    #[test]
    fn history_trim_keeps_only_the_window() {
        let mut history = History::new(4);
        for byte in 0u8..10 {
            history.push(byte);
        }
        history.trim(10);
        assert_eq!(history.current_pos(), 10);
        assert_eq!(history.raw_range(6, 10).unwrap(), &[6, 7, 8, 9]);
        assert!(history.raw_byte(5).is_none());
    }
}
