//! RAR 3.x/4.x member decompressor — the legacy `unp_ver >= 29` LZSS+Huffman
//! codec used by every RAR 3.0–4.x archive (`Rar!\x1a\x07\x00` container).
//!
//! Ported from the decode half of bitplane's `rars` (WTFPL) `codec/rar29.rs`,
//! which is validated against genuine WinRAR archives in its own fixture
//! suite. The decoder is self-contained (its own MSB-first bit reader and
//! canonical-Huffman tables) and keeps the RAR5 codec untouched.
//!
//! A member is a sequence of *blocks*. Each block begins (byte-aligned) with
//! either a PPMd marker or an LZSS header that optionally re-reads the four
//! Huffman tables (main/offset/low-offset/length). Solid chains share one
//! decoder instance: the output window, `old_offsets`, last-length/last-offset
//! and (per block header) the tables persist across members, while each
//! member's packed bytes start a fresh bit reader at a block boundary.
//!
//! Only the LZSS+Huffman path is implemented today; members whose first block
//! header selects PPMd, or whose LZ stream carries a VM-filter record
//! (symbol 257), fail with a clear [`RarError::Unsupported`]. Both are
//! separate follow-up milestones.

use crate::error::{RarError, RarResult};

// ── Table geometry ─────────────────────────────────────────────────────────

const MAIN_COUNT: usize = 299;
const OFFSET_COUNT: usize = 60;
const LOW_OFFSET_COUNT: usize = 17;
const LENGTH_COUNT: usize = 28;
const LEVEL_COUNT: usize = 20;
const TABLE_COUNT: usize = MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT + LENGTH_COUNT;

/// Retained look-behind history for solid chains (4 MiB, the RAR3/4 window).
const MAX_HISTORY: usize = 4 * 1024 * 1024;

const LENGTH_BASES: [usize; LENGTH_COUNT] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224,
];
const LENGTH_BITS: [u8; LENGTH_COUNT] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5,
];
const OFFSET_BASES: [usize; OFFSET_COUNT] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536,
    2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072, 196608,
    262144, 327680, 393216, 458752, 524288, 589824, 655360, 720896, 786432, 851968, 917504, 983040,
    1048576, 1310720, 1572864, 1835008, 2097152, 2359296, 2621440, 2883584, 3145728, 3407872,
    3670016, 3932160,
];
const OFFSET_BITS: [u8; OFFSET_COUNT] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14, 15, 15, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 18, 18, 18, 18, 18,
    18, 18, 18, 18, 18, 18, 18,
];
const SHORT_BASES: [usize; 8] = [0, 4, 8, 16, 32, 64, 128, 192];
const SHORT_BITS: [u8; 8] = [2, 2, 3, 4, 5, 6, 6, 6];

// ── Internal error ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum E {
    /// The packed stream ended before the decoder needed more bits.
    Truncated,
    /// Structurally invalid stream data.
    Bad(&'static str),
    /// Valid RAR3/4 feature this codec does not implement yet.
    Unsupported(&'static str),
}

fn map_err(error: E) -> RarError {
    match error {
        E::Bad(message) => RarError::Format(format!("RAR 2.9 stream: {message}")),
        E::Truncated => RarError::Format("RAR 2.9 bitstream is truncated".into()),
        E::Unsupported(message) => RarError::Unsupported(message.to_string()),
    }
}

type Res<T> = Result<T, E>;

// ── Bit reader (MSB-first) ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BitReader {
    input: Vec<u8>,
    bit_pos: usize,
}

impl BitReader {
    fn new() -> Self {
        Self {
            input: Vec::new(),
            bit_pos: 0,
        }
    }

    fn append(&mut self, input: &[u8]) {
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

    fn align_byte(&mut self) {
        self.bit_pos = (self.bit_pos + 7) & !7;
    }

    fn peek_bit(&self) -> Res<u8> {
        self.peek_bits(1).map(|value| value as u8)
    }

    fn read_bit(&mut self) -> Res<u8> {
        self.read_bits(1).map(|value| value as u8)
    }

    fn read_bits(&mut self, count: u8) -> Res<u32> {
        let value = self.peek_bits(count)?;
        self.bit_pos += count as usize;
        Ok(value)
    }

    fn peek_bits(&self, count: u8) -> Res<u32> {
        if count > 24 {
            return Err(E::Bad("bit read is too wide"));
        }
        let mut value = 0u32;
        for i in 0..count as usize {
            let bit_index = self.bit_pos + i;
            let byte = *self.input.get(bit_index / 8).ok_or(E::Truncated)?;
            let bit = (byte >> (7 - (bit_index % 8))) & 1;
            value = (value << 1) | bit as u32;
        }
        Ok(value)
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
struct Huffman {
    symbols: Vec<HuffmanSymbol>,
    first_code: [u16; 16],
    first_index: [usize; 16],
    counts: [u16; 16],
}

impl Huffman {
    fn empty() -> Self {
        Self {
            symbols: Vec::new(),
            first_code: [0; 16],
            first_index: [0; 16],
            counts: [0; 16],
        }
    }

    fn from_lengths(lengths: &[u8]) -> Res<Self> {
        let mut count = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return Err(E::Bad("Huffman length is too large"));
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

    fn decode(&self, bits: &mut BitReader) -> Res<usize> {
        let mut code = 0u16;
        if self.symbols.is_empty() {
            return Err(E::Bad("empty Huffman table"));
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
        Err(E::Bad("invalid Huffman code"))
    }
}

fn validate_huffman_counts(count: &[u16; 16]) -> Res<()> {
    let mut available = 1i32;
    for &len_count in count.iter().skip(1) {
        available = (available << 1) - i32::from(len_count);
        if available < 0 {
            return Err(E::Bad("oversubscribed Huffman table"));
        }
    }
    Ok(())
}

// ── Decoder ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LzBlockEnd {
    SameFileNewTable,
    NewFileKeepTables,
    NewFileNewTables,
}

/// Persistent RAR3/4 LZSS+Huffman decoder. Keep one instance across solid
/// chain members; use a fresh instance for a standalone member.
#[derive(Debug)]
pub(crate) struct Rar29Decoder {
    bits: BitReader,
    levels: [u8; TABLE_COUNT],
    main: Huffman,
    offsets: Huffman,
    low_offsets: Huffman,
    lengths: Huffman,
    old_offsets: [usize; 4],
    last_offset: usize,
    last_length: usize,
    last_low_offset: usize,
    low_offset_repeats: usize,
    pending_match: Option<(usize, usize)>,
    in_lz_block: bool,
    /// Absolute position of `output[0]`; history older than this is trimmed.
    base_offset: usize,
    /// All decoded bytes since the last trim, in stream order.
    output: Vec<u8>,
    last_block_end: Option<LzBlockEnd>,
}

impl Default for Rar29Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Rar29Decoder {
    pub(crate) fn new() -> Self {
        Self {
            bits: BitReader::new(),
            levels: [0; TABLE_COUNT],
            main: Huffman::empty(),
            offsets: Huffman::empty(),
            low_offsets: Huffman::empty(),
            lengths: Huffman::empty(),
            old_offsets: [0; 4],
            last_offset: 0,
            last_length: 0,
            last_low_offset: 0,
            low_offset_repeats: 0,
            pending_match: None,
            in_lz_block: false,
            base_offset: 0,
            output: Vec::new(),
            last_block_end: None,
        }
    }

    /// Decode one member's packed bytes into a fresh Vec with that member's
    /// output. In a solid chain the caller reuses this decoder; the returned
    /// Vec holds only the member's own bytes, while the window inside this
    /// decoder retains up to [`MAX_HISTORY`] bytes of look-behind.
    pub(crate) fn decode_member(&mut self, packed: &[u8], output_size: u64) -> RarResult<Vec<u8>> {
        let output_size = usize::try_from(output_size).map_err(|_| RarError::LimitExceeded {
            limit: u64::MAX,
            context: "RAR 2.9 member is too large for this platform".into(),
        })?;
        let start = self.current_pos();
        let target = start
            .checked_add(output_size)
            .ok_or_else(|| RarError::Format("RAR 2.9 output size overflows".into()))?;
        if !packed.is_empty() {
            self.bits = BitReader::new();
        }
        self.bits.append(packed);
        self.decode_until(target).map_err(map_err)?;
        self.finish_member().map_err(map_err)?;
        let out = self.raw_range(start, target).map_err(map_err)?.to_vec();
        self.trim_history(target, target);
        Ok(out)
    }

    fn decode_until(&mut self, target: usize) -> Res<()> {
        while self.current_pos() < target {
            self.drain_pending_match(target)?;
            if self.current_pos() >= target {
                break;
            }
            if !self.in_lz_block {
                // A block that said "new file" while the member still owes
                // output is an encoder bug, not a format feature. Reading its
                // tables anyway is the tolerance that lets rars ship members
                // split across blocks that unrar refused.
                self.read_tables()?;
                self.in_lz_block = true;
            }
            self.decode_lz(target)?;
        }
        Ok(())
    }

    fn read_tables(&mut self) -> Res<()> {
        self.bits.align_byte();
        if self.bits.peek_bit()? != 0 {
            let first_byte = self.bits.read_bits(8)? as u8;
            let _ = first_byte;
            return Err(E::Unsupported(
                "RAR 3.x/4.x PPMd-compressed members are not yet supported (decode only LZSS for now)",
            ));
        }
        self.bits.read_bit()?;
        let keep_tables = self.bits.read_bit()? != 0;
        self.last_low_offset = 0;
        self.low_offset_repeats = 0;
        if !keep_tables {
            self.levels = [0; TABLE_COUNT];
        }

        let level_lengths = Self::read_level_lengths(&mut self.bits)?;
        let level_decoder = Huffman::from_lengths(&level_lengths)?;
        let mut new_levels = [0u8; TABLE_COUNT];
        let mut pos = 0usize;
        while pos < TABLE_COUNT {
            let symbol = level_decoder.decode(&mut self.bits)?;
            match symbol {
                0..=15 => {
                    new_levels[pos] = (self.levels[pos].wrapping_add(symbol as u8)) & 0x0f;
                    pos += 1;
                }
                16 => {
                    if pos == 0 {
                        return Err(E::Bad("table repeat at start"));
                    }
                    let count = 3 + self.bits.read_bits(3)? as usize;
                    let value = new_levels[pos - 1];
                    fill_levels(&mut new_levels, &mut pos, count, value)?;
                }
                17 => {
                    if pos == 0 {
                        return Err(E::Bad("long table repeat at start"));
                    }
                    let count = 11 + self.bits.read_bits(7)? as usize;
                    let value = new_levels[pos - 1];
                    fill_levels(&mut new_levels, &mut pos, count, value)?;
                }
                18 => {
                    let count = 3 + self.bits.read_bits(3)? as usize;
                    fill_levels(&mut new_levels, &mut pos, count, 0)?;
                }
                19 => {
                    let count = 11 + self.bits.read_bits(7)? as usize;
                    fill_levels(&mut new_levels, &mut pos, count, 0)?;
                }
                _ => return Err(E::Bad("invalid level symbol")),
            }
        }

        self.levels = new_levels;
        self.main = Huffman::from_lengths(&self.levels[..MAIN_COUNT])?;
        self.offsets = Huffman::from_lengths(&self.levels[MAIN_COUNT..MAIN_COUNT + OFFSET_COUNT])?;
        self.low_offsets = Huffman::from_lengths(
            &self.levels[MAIN_COUNT + OFFSET_COUNT..MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT],
        )?;
        self.lengths =
            Huffman::from_lengths(&self.levels[MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT..])?;
        Ok(())
    }

    fn read_level_lengths(bits: &mut BitReader) -> Res<[u8; LEVEL_COUNT]> {
        let mut lengths = [0u8; LEVEL_COUNT];
        let mut pos = 0usize;
        while pos < LEVEL_COUNT {
            let value = bits.read_bits(4)? as u8;
            if value == 15 {
                let zero_count = bits.read_bits(4)? as usize;
                if zero_count == 0 {
                    lengths[pos] = 15;
                    pos += 1;
                } else {
                    pos = pos.saturating_add(zero_count + 2).min(LEVEL_COUNT);
                }
            } else {
                lengths[pos] = value;
                pos += 1;
            }
        }
        Ok(lengths)
    }

    fn decode_lz(&mut self, output_size: usize) -> Res<()> {
        while self.current_pos() < output_size {
            let symbol = self.main.decode(&mut self.bits)?;
            match symbol {
                0..=255 => self.output.push(symbol as u8),
                256 => {
                    self.read_end_of_block()?;
                    return Ok(());
                }
                257 => {
                    // VM filter record (first byte already buffered by the
                    // decoder's caller in rars; here we read the record the
                    // same way so unsupported members fail before producing
                    // wrong output).
                    let _ = self.bits.read_bits(8)?;
                    return Err(E::Unsupported(
                        "RAR 3.x/4.x VM-filtered members are not yet supported (filters milestone pending)",
                    ));
                }
                258 => {
                    if self.last_length != 0 {
                        self.copy_match(self.last_length, self.last_offset, output_size)?;
                    }
                }
                259..=262 => {
                    let index = symbol - 259;
                    let offset = self.old_offsets[index];
                    let length_slot = self.lengths.decode(&mut self.bits)?;
                    if length_slot >= LENGTH_COUNT {
                        return Err(E::Bad("invalid repeat length slot"));
                    }
                    let mut length = LENGTH_BASES[length_slot] + 2;
                    if LENGTH_BITS[length_slot] != 0 {
                        length += self.bits.read_bits(LENGTH_BITS[length_slot])? as usize;
                    }
                    self.rotate_old_offset(index);
                    self.last_offset = offset;
                    self.last_length = length;
                    self.copy_match(length, offset, output_size)?;
                }
                263..=270 => {
                    let index = symbol - 263;
                    let mut offset = SHORT_BASES[index] + 1;
                    if SHORT_BITS[index] != 0 {
                        offset += self.bits.read_bits(SHORT_BITS[index])? as usize;
                    }
                    self.push_old_offset(offset);
                    self.last_offset = offset;
                    self.last_length = 2;
                    self.copy_match(2, offset, output_size)?;
                }
                271..=298 => {
                    let length_slot = symbol - 271;
                    let mut length = LENGTH_BASES[length_slot] + 3;
                    if LENGTH_BITS[length_slot] != 0 {
                        length += self.bits.read_bits(LENGTH_BITS[length_slot])? as usize;
                    }
                    let offset = self.read_offset()?;
                    if offset >= 0x2000 {
                        length += 1;
                    }
                    if offset >= 0x40000 {
                        length += 1;
                    }
                    self.push_old_offset(offset);
                    self.last_offset = offset;
                    self.last_length = length;
                    self.copy_match(length, offset, output_size)?;
                }
                _ => return Err(E::Bad("invalid main symbol")),
            }
        }
        Ok(())
    }

    fn read_offset(&mut self) -> Res<usize> {
        let slot = self.offsets.decode(&mut self.bits)?;
        if slot >= OFFSET_COUNT {
            return Err(E::Bad("invalid offset slot"));
        }
        let mut offset = OFFSET_BASES[slot] + 1;
        let extra_bits = OFFSET_BITS[slot];
        if extra_bits != 0 {
            if slot > 9 {
                if extra_bits > 4 {
                    offset += (self.bits.read_bits(extra_bits - 4)? as usize) << 4;
                }
                if self.low_offset_repeats > 0 {
                    self.low_offset_repeats -= 1;
                    offset += self.last_low_offset;
                } else {
                    let low = self.low_offsets.decode(&mut self.bits)?;
                    if low == 16 {
                        self.low_offset_repeats = 15;
                        offset += self.last_low_offset;
                    } else if low < 16 {
                        self.last_low_offset = low;
                        offset += low;
                    } else {
                        return Err(E::Bad("invalid low offset symbol"));
                    }
                }
            } else {
                offset += self.bits.read_bits(extra_bits)? as usize;
            }
        }
        Ok(offset)
    }

    fn read_end_of_block(&mut self) -> Res<LzBlockEnd> {
        let end = self.read_end_of_block_inner()?;
        self.last_block_end = Some(end);
        Ok(end)
    }

    fn read_end_of_block_inner(&mut self) -> Res<LzBlockEnd> {
        if self.bits.read_bit()? != 0 {
            self.in_lz_block = false;
            return Ok(LzBlockEnd::SameFileNewTable);
        }
        if self.bits.read_bit()? != 0 {
            self.in_lz_block = false;
            Ok(LzBlockEnd::NewFileNewTables)
        } else {
            self.in_lz_block = true;
            Ok(LzBlockEnd::NewFileKeepTables)
        }
    }

    fn copy_match(&mut self, length: usize, offset: usize, output_size: usize) -> Res<()> {
        // The bitstream normally encodes match distances as offset+1, so zero
        // is not emitted for fresh matches. Keep the legacy decoder boundary
        // tolerant here: a zero internal offset behaves as distance one.
        let offset = if offset == 0 { 1 } else { offset };
        // A match reaching past the start of the stream writes zeroes rather
        // than failing. WinRAR never clears its window and guards the copy
        // with a first-wrap flag instead, so those bytes read as zero there,
        // and an archive that leans on it stays readable here. The decision
        // is taken once for the whole match, as it is there: a copy does not
        // start on zeroes and cross into real bytes partway.
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
                    .ok_or(E::Bad("match distance is out of range"))?
            };
            self.output.push(byte);
        }
        Ok(())
    }

    fn drain_pending_match(&mut self, output_size: usize) -> Res<()> {
        let Some((length, offset)) = self.pending_match.take() else {
            return Ok(());
        };
        self.copy_match(length, offset, output_size)
    }

    fn finish_member(&mut self) -> Res<()> {
        self.finish_lz_member()
    }

    fn finish_lz_member(&mut self) -> Res<()> {
        loop {
            if !self.in_lz_block {
                return Ok(());
            }
            let symbol = self.main.decode(&mut self.bits)?;
            if symbol != 256 {
                return Err(E::Bad("LZ member has trailing data"));
            }
            match self.read_end_of_block()? {
                LzBlockEnd::SameFileNewTable => {
                    if let Err(E::Truncated) = self.read_tables() {
                        return Ok(());
                    }
                    self.in_lz_block = true;
                }
                LzBlockEnd::NewFileKeepTables | LzBlockEnd::NewFileNewTables => return Ok(()),
            }
        }
    }

    fn push_old_offset(&mut self, offset: usize) {
        self.old_offsets[3] = self.old_offsets[2];
        self.old_offsets[2] = self.old_offsets[1];
        self.old_offsets[1] = self.old_offsets[0];
        self.old_offsets[0] = offset;
    }

    fn rotate_old_offset(&mut self, index: usize) {
        let value = self.old_offsets[index];
        for i in (1..=index).rev() {
            self.old_offsets[i] = self.old_offsets[i - 1];
        }
        self.old_offsets[0] = value;
    }

    fn current_pos(&self) -> usize {
        self.base_offset + self.output.len()
    }

    fn raw_byte(&self, position: usize) -> Option<&u8> {
        self.output.get(position.checked_sub(self.base_offset)?)
    }

    fn raw_range(&self, start: usize, end: usize) -> Res<&[u8]> {
        if start < self.base_offset || end < start {
            return Err(E::Bad("retained history is unavailable"));
        }
        let rel_start = start - self.base_offset;
        let rel_end = end - self.base_offset;
        self.output
            .get(rel_start..rel_end)
            .ok_or(E::Bad("retained history is unavailable"))
    }

    fn trim_history(&mut self, flushed_pos: usize, current_pos: usize) {
        let keep_from = current_pos.saturating_sub(MAX_HISTORY);
        let keep_from = keep_from.min(flushed_pos);
        if keep_from <= self.base_offset {
            return;
        }
        let drain = keep_from - self.base_offset;
        self.output.drain(..drain);
        self.base_offset = keep_from;
    }
}

fn fill_levels(levels: &mut [u8], pos: &mut usize, count: usize, value: u8) -> Res<()> {
    let end = pos
        .checked_add(count)
        .ok_or(E::Bad("table run overflows"))?;
    let end = end.min(levels.len());
    for item in &mut levels[*pos..end] {
        *item = value;
    }
    *pos = end;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huffman_roundtrip_table_build() {
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
    fn oversubscribed_table_rejected() {
        let mut count = [0u16; 16];
        count[1] = 3; // three 1-bit codes can never fit
        assert!(validate_huffman_counts(&count).is_err());
    }
}
