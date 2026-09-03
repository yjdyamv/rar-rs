//! RAR 2.0–2.9 member decompressor — the legacy `unp_ver` 20/26 codec
//! (`Rar!\x1a\x07\x00` container, RAR 2.0 through 2.9 era archives).
//!
//! Ported from the decode half of bitplane's `rars` (WTFPL) `codec/rar20.rs`.
//! Like RAR3/4, a member is a sequence of LZ blocks with optional Huffman
//! table refreshes; RAR 2.x adds *audio blocks* (bit 15 of the 16-bit block
//! header) whose bytes are coded by per-channel Huffman tables over an
//! adaptive delta predictor. Solid chains share one decoder instance (the
//! output window and audio predictor state persist across members).
//!
//! The self-contained MSB-first bit reader and canonical-Huffman tables are
//! duplicated here on purpose, mirroring rars (each legacy codec file keeps
//! its own primitives).

use crate::error::{RarError, RarResult};

const MAIN_COUNT: usize = 298;
const OFFSET_COUNT: usize = 48;
const LENGTH_COUNT: usize = 28;
const LEVEL_COUNT: usize = 19;
const TABLE_COUNT: usize = MAIN_COUNT + OFFSET_COUNT + LENGTH_COUNT;
const AUDIO_COUNT: usize = 257;
const MAX_CHANNELS: usize = 4;
const OLD_LEVEL_COUNT: usize = AUDIO_COUNT * MAX_CHANNELS;

/// Retained look-behind history for solid chains.
const MAX_HISTORY: usize = 1024 * 1024;

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
];
const OFFSET_BITS: [u8; OFFSET_COUNT] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14, 15, 15, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
];
const SHORT_BASES: [usize; 8] = [0, 4, 8, 16, 32, 64, 128, 192];
const SHORT_BITS: [u8; 8] = [2, 2, 3, 4, 5, 6, 6, 6];

// ── Internal error ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum E {
    Truncated,
    Bad(&'static str),
}

fn map_err(error: E) -> RarError {
    match error {
        E::Bad(message) => RarError::Format(format!("RAR 2.0 stream: {message}")),
        E::Truncated => RarError::Format("RAR 2.0 bitstream is truncated".into()),
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

    fn remaining_bytes_from_current(&self) -> usize {
        self.input.len().saturating_sub(self.bit_pos / 8)
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

// ── Audio predictor state ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
struct AudioState {
    k: [i32; 5],
    d1: i32,
    d2: i32,
    d3: i32,
    d4: i32,
    last_delta: i32,
    last_char: i32,
    byte_count: u32,
    dif: [u32; 11],
}

// ── Decoder ────────────────────────────────────────────────────────────────

/// Persistent RAR 2.x (unp_ver 20/26) LZSS+Huffman decoder. Keep one
/// instance across solid chain members; use a fresh instance for a
/// standalone member.
#[derive(Debug)]
pub(crate) struct Rar20Decoder {
    bits: BitReader,
    levels: [u8; OLD_LEVEL_COUNT],
    main: Huffman,
    offsets: Huffman,
    lengths: Huffman,
    audio_tables: [Huffman; MAX_CHANNELS],
    audio_block: bool,
    channels: usize,
    cur_channel: usize,
    audio: [AudioState; MAX_CHANNELS],
    channel_delta: i32,
    old_offsets: [usize; 4],
    last_offset: usize,
    last_length: usize,
    pending_match: Option<(usize, usize)>,
    in_block: bool,
    output: Vec<u8>,
    base_offset: usize,
}

impl Default for Rar20Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Rar20Decoder {
    pub(crate) fn new() -> Self {
        Self {
            bits: BitReader::new(),
            levels: [0; OLD_LEVEL_COUNT],
            main: Huffman::empty(),
            offsets: Huffman::empty(),
            lengths: Huffman::empty(),
            audio_tables: std::array::from_fn(|_| Huffman::empty()),
            audio_block: false,
            channels: 1,
            cur_channel: 0,
            audio: [AudioState::default(); MAX_CHANNELS],
            channel_delta: 0,
            old_offsets: [0; 4],
            last_offset: 0,
            last_length: 0,
            pending_match: None,
            in_block: false,
            output: Vec::new(),
            base_offset: 0,
        }
    }

    /// Decode one member's packed bytes into a fresh Vec with that member's
    /// output. Solid callers reuse this decoder; the look-behind window
    /// (plus audio predictor state) persists across members.
    pub(crate) fn decode_member(&mut self, packed: &[u8], output_size: u64) -> RarResult<Vec<u8>> {
        let output_size = usize::try_from(output_size).map_err(|_| RarError::LimitExceeded {
            limit: u64::MAX,
            context: "RAR 2.0 member is too large for this platform".into(),
        })?;
        let start = self.current_pos();
        let target = start
            .checked_add(output_size)
            .ok_or_else(|| RarError::Format("RAR 2.0 output size overflows".into()))?;
        if !packed.is_empty() {
            self.bits = BitReader::new();
        }
        self.bits.append(packed);
        self.decode_until(target).map_err(map_err)?;
        self.read_last_tables().map_err(map_err)?;
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
            if !self.in_block {
                self.read_tables()?;
                self.in_block = true;
            }
            self.decode_lz(target)?;
        }
        Ok(())
    }

    fn read_tables(&mut self) -> Res<()> {
        let bit_field = self.bits.peek_bits(16)?;
        self.audio_block = bit_field & 0x8000 != 0;
        let keep_tables = bit_field & 0x4000 != 0;
        self.bits.read_bits(2)?;
        if !keep_tables {
            self.levels = [0; OLD_LEVEL_COUNT];
        }

        let table_size = if self.audio_block {
            self.channels = ((bit_field >> 12) as usize & 3) + 1;
            if self.cur_channel >= self.channels {
                self.cur_channel = 0;
            }
            self.bits.read_bits(2)?;
            AUDIO_COUNT * self.channels
        } else {
            TABLE_COUNT
        };

        let level_lengths = Self::read_level_lengths(&mut self.bits)?;
        let level_decoder = Huffman::from_lengths(&level_lengths)?;
        let mut new_levels = [0u8; OLD_LEVEL_COUNT];
        let mut pos = 0usize;
        while pos < table_size {
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
                    let count = 3 + self.bits.read_bits(2)? as usize;
                    let value = new_levels[pos - 1];
                    fill_levels(&mut new_levels, &mut pos, count, value)?;
                }
                17 => {
                    let count = 3 + self.bits.read_bits(3)? as usize;
                    fill_levels(&mut new_levels, &mut pos, count, 0)?;
                }
                18 => {
                    let count = 11 + self.bits.read_bits(7)? as usize;
                    fill_levels(&mut new_levels, &mut pos, count, 0)?;
                }
                _ => return Err(E::Bad("invalid level symbol")),
            }
        }

        self.levels = new_levels;
        if self.audio_block {
            for channel in 0..self.channels {
                let start = channel * AUDIO_COUNT;
                self.audio_tables[channel] =
                    Huffman::from_lengths(&self.levels[start..start + AUDIO_COUNT])?;
            }
        } else {
            self.main = Huffman::from_lengths(&self.levels[..MAIN_COUNT])?;
            self.offsets =
                Huffman::from_lengths(&self.levels[MAIN_COUNT..MAIN_COUNT + OFFSET_COUNT])?;
            self.lengths =
                Huffman::from_lengths(&self.levels[MAIN_COUNT + OFFSET_COUNT..TABLE_COUNT])?;
        }
        Ok(())
    }

    /// RAR 2.x level lengths are twenty plain 4-bit fields (no escapes).
    fn read_level_lengths(bits: &mut BitReader) -> Res<[u8; LEVEL_COUNT]> {
        let mut lengths = [0u8; LEVEL_COUNT];
        for length in &mut lengths {
            *length = bits.read_bits(4)? as u8;
        }
        Ok(lengths)
    }

    fn decode_lz(&mut self, output_size: usize) -> Res<()> {
        while self.current_pos() < output_size {
            if self.audio_block {
                self.decode_audio_byte()?;
                if !self.in_block {
                    return Ok(());
                }
                continue;
            }
            let symbol = self.main.decode(&mut self.bits)?;
            match symbol {
                0..=255 => self.output.push(symbol as u8),
                256 => {
                    if self.last_length != 0 {
                        let length = self.last_length;
                        let offset = self.last_offset;
                        self.push_old_offset(offset);
                        self.copy_match(length, offset, output_size)?;
                    }
                }
                257..=260 => {
                    let index = symbol - 257;
                    let offset = self.old_offsets[index];
                    let length_slot = self.lengths.decode(&mut self.bits)?;
                    if length_slot >= LENGTH_COUNT {
                        return Err(E::Bad("invalid repeat length slot"));
                    }
                    let mut length = LENGTH_BASES[length_slot] + 2;
                    if LENGTH_BITS[length_slot] != 0 {
                        length += self.bits.read_bits(LENGTH_BITS[length_slot])? as usize;
                    }
                    if offset >= 0x101 {
                        length += 1;
                    }
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
                261..=268 => {
                    let index = symbol - 261;
                    let mut offset = SHORT_BASES[index] + 1;
                    if SHORT_BITS[index] != 0 {
                        offset += self.bits.read_bits(SHORT_BITS[index])? as usize;
                    }
                    self.push_old_offset(offset);
                    self.last_offset = offset;
                    self.last_length = 2;
                    self.copy_match(2, offset, output_size)?;
                }
                269 => {
                    // End of LZ block; the next block header may follow in
                    // this member or the next.
                    self.in_block = false;
                    return Ok(());
                }
                270..=297 => {
                    let length_slot = symbol - 270;
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

    fn decode_audio_byte(&mut self) -> Res<()> {
        let symbol = self.audio_tables[self.cur_channel].decode(&mut self.bits)?;
        if symbol == 256 {
            self.in_block = false;
            return Ok(());
        }
        if symbol > 256 {
            return Err(E::Bad("invalid audio symbol"));
        }
        let byte = self.decode_audio(symbol as u8);
        self.output.push(byte);
        self.cur_channel += 1;
        if self.cur_channel == self.channels {
            self.cur_channel = 0;
        }
        Ok(())
    }

    fn decode_audio(&mut self, delta: u8) -> u8 {
        let state = &mut self.audio[self.cur_channel];
        state.byte_count = state.byte_count.wrapping_add(1);
        state.d4 = state.d3;
        state.d3 = state.d2;
        state.d2 = state.last_delta - state.d1;
        state.d1 = state.last_delta;

        let predicted = 8 * state.last_char
            + state.k[0] * state.d1
            + state.k[1] * state.d2
            + state.k[2] * state.d3
            + state.k[3] * state.d4
            + state.k[4] * self.channel_delta;
        let predicted = (predicted >> 3) & 0xff;
        let byte = predicted.wrapping_sub(delta as i32) as u8;

        let d = (delta as i8 as i32) << 3;
        state.dif[0] = state.dif[0].wrapping_add(d.unsigned_abs());
        state.dif[1] = state.dif[1].wrapping_add((d - state.d1).unsigned_abs());
        state.dif[2] = state.dif[2].wrapping_add((d + state.d1).unsigned_abs());
        state.dif[3] = state.dif[3].wrapping_add((d - state.d2).unsigned_abs());
        state.dif[4] = state.dif[4].wrapping_add((d + state.d2).unsigned_abs());
        state.dif[5] = state.dif[5].wrapping_add((d - state.d3).unsigned_abs());
        state.dif[6] = state.dif[6].wrapping_add((d + state.d3).unsigned_abs());
        state.dif[7] = state.dif[7].wrapping_add((d - state.d4).unsigned_abs());
        state.dif[8] = state.dif[8].wrapping_add((d + state.d4).unsigned_abs());
        state.dif[9] = state.dif[9].wrapping_add((d - self.channel_delta).unsigned_abs());
        state.dif[10] = state.dif[10].wrapping_add((d + self.channel_delta).unsigned_abs());

        self.channel_delta = (byte.wrapping_sub(state.last_char as u8)) as i8 as i32;
        state.last_delta = self.channel_delta;
        state.last_char = byte as i32;

        if state.byte_count & 0x1f == 0 {
            let mut min_dif = state.dif[0];
            let mut num_min_dif = 0usize;
            state.dif[0] = 0;
            for index in 1..state.dif.len() {
                if state.dif[index] < min_dif {
                    min_dif = state.dif[index];
                    num_min_dif = index;
                }
                state.dif[index] = 0;
            }
            match num_min_dif {
                1 if state.k[0] >= -16 => state.k[0] -= 1,
                2 if state.k[0] < 16 => state.k[0] += 1,
                3 if state.k[1] >= -16 => state.k[1] -= 1,
                4 if state.k[1] < 16 => state.k[1] += 1,
                5 if state.k[2] >= -16 => state.k[2] -= 1,
                6 if state.k[2] < 16 => state.k[2] += 1,
                7 if state.k[3] >= -16 => state.k[3] -= 1,
                8 if state.k[3] < 16 => state.k[3] += 1,
                9 if state.k[4] >= -16 => state.k[4] -= 1,
                10 if state.k[4] < 16 => state.k[4] += 1,
                _ => {}
            }
        }

        byte
    }

    fn read_offset(&mut self) -> Res<usize> {
        let slot = self.offsets.decode(&mut self.bits)?;
        if slot >= OFFSET_COUNT {
            return Err(E::Bad("invalid offset slot"));
        }
        let mut offset = OFFSET_BASES[slot] + 1;
        if OFFSET_BITS[slot] != 0 {
            offset += self.bits.read_bits(OFFSET_BITS[slot])? as usize;
        }
        Ok(offset)
    }

    fn copy_match(&mut self, length: usize, offset: usize, output_size: usize) -> Res<()> {
        let offset = if offset == 0 { 1 } else { offset };
        // A match reaching past the start of the stream writes zeroes rather
        // than failing (WinRAR never clears its window; see rar29.rs).
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

    /// If the member ended right at a block boundary, consume the trailing
    /// end-of-block marker and load the next block's tables so a following
    /// solid member can continue.
    fn read_last_tables(&mut self) -> Res<()> {
        if self.bits.remaining_bytes_from_current() < 5 {
            return Ok(());
        }
        if self.audio_block {
            if self.audio_tables[self.cur_channel].symbols.is_empty() {
                return Ok(());
            }
            if self.audio_tables[self.cur_channel].decode(&mut self.bits)? == 256 {
                self.read_tables()?;
                self.in_block = true;
            }
        } else {
            if self.main.symbols.is_empty() {
                return Ok(());
            }
            if self.main.decode(&mut self.bits)? == 269 {
                self.read_tables()?;
                self.in_block = true;
            }
        }
        Ok(())
    }

    fn push_old_offset(&mut self, offset: usize) {
        self.old_offsets[3] = self.old_offsets[2];
        self.old_offsets[2] = self.old_offsets[1];
        self.old_offsets[1] = self.old_offsets[0];
        self.old_offsets[0] = offset;
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
        let keep_from = current_pos.saturating_sub(MAX_HISTORY).min(flushed_pos);
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
        let lengths = [2u8, 2, 2, 2];
        let table = Huffman::from_lengths(&lengths).expect("build");
        let mut bits = BitReader::new();
        bits.append(&[0b0001_1011]); // 00 01 10 11
        assert_eq!(table.decode(&mut bits).unwrap(), 0);
        assert_eq!(table.decode(&mut bits).unwrap(), 1);
        assert_eq!(table.decode(&mut bits).unwrap(), 2);
        assert_eq!(table.decode(&mut bits).unwrap(), 3);
    }
}
