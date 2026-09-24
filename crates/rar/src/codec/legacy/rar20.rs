//! RAR 2.0–2.9 member decompressor — the legacy `unp_ver` 20/26 codec
//! (`Rar!\x1a\x07\x00` container, RAR 2.0 through 2.9 era archives).
//!
//! Ported from the decode half of bitplane's `rars` (MIT OR Apache-2.0)
//! `codec/rar20.rs`.
//! Like RAR3/4, a member is a sequence of LZ blocks with optional Huffman
//! table refreshes; RAR 2.x adds *audio blocks* (bit 15 of the 16-bit block
//! header) whose bytes are coded by per-channel Huffman tables over an
//! adaptive delta predictor. Solid chains share one decoder instance (the
//! output window and audio predictor state persist across members).
//!
//! The MSB-first bit reader, canonical-Huffman tables and the sliding
//! history are shared with the RAR3/4 decoder (`super::lz`).

use super::encode_core::{
    LENGTH_BASES, LENGTH_BITS, LENGTH_COUNT, SHORT_BASES, SHORT_BITS, push_old_offset,
};
use super::lz::{BitReader, Error as E, History, Huffman, Res, fill_levels};
use crate::error::{RarError, RarResult};

const MAIN_COUNT: usize = 298;
const OFFSET_COUNT: usize = 48;
const LEVEL_COUNT: usize = 19;
const TABLE_COUNT: usize = MAIN_COUNT + OFFSET_COUNT + LENGTH_COUNT;
const AUDIO_COUNT: usize = 257;
const MAX_CHANNELS: usize = 4;
const OLD_LEVEL_COUNT: usize = AUDIO_COUNT * MAX_CHANNELS;

/// Retained look-behind history for solid chains.
const MAX_HISTORY: usize = 1024 * 1024;

const OFFSET_BASES: [usize; OFFSET_COUNT] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536,
    2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072, 196608,
    262144, 327680, 393216, 458752, 524288, 589824, 655360, 720896, 786432, 851968, 917504, 983040,
];
const OFFSET_BITS: [u8; OFFSET_COUNT] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14, 15, 15, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
];
fn map_err(error: E) -> RarError {
    error.into_rar("RAR 2.0")
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
    history: History,
    in_block: bool,
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
            history: History::new(MAX_HISTORY),
            in_block: false,
        }
    }

    /// Decode one member's packed bytes into a fresh Vec with that member's
    /// output. Solid callers reuse this decoder; the look-behind window
    /// (plus audio predictor state) persists across members.
    pub(crate) fn decode_member(&mut self, packed: &[u8], output_size: u64) -> RarResult<Vec<u8>> {
        let output_size = usize::try_from(output_size).map_err(|_| {
            RarError::limit_exceeded(u64::MAX, "RAR 2.0 member is too large for this platform")
        })?;
        let start = self.history.current_pos();
        let target = start
            .checked_add(output_size)
            .ok_or_else(|| RarError::format("RAR 2.0 output size overflows"))?;
        if !packed.is_empty() {
            self.bits = BitReader::new();
        }
        self.bits.append(packed);
        self.decode_until(target).map_err(map_err)?;
        self.read_last_tables().map_err(map_err)?;
        let out = self
            .history
            .raw_range(start, target)
            .map_err(map_err)?
            .to_vec();
        self.history.trim(target);
        Ok(out)
    }

    /// Decode a member streaming its output to `writer` (bounded memory:
    /// sliding window + one flush chunk). RAR 2.x has no VM-filter records,
    /// so every byte can be flushed as it decodes.
    pub(crate) fn decode_member_streaming_to(
        &mut self,
        packed: &[u8],
        output_size: u64,
        writer: &mut dyn std::io::Write,
    ) -> RarResult<()> {
        const FLUSH: usize = 1024 * 1024;
        let output_size = usize::try_from(output_size).map_err(|_| {
            RarError::limit_exceeded(u64::MAX, "RAR 2.0 member is too large for this platform")
        })?;
        let start = self.history.current_pos();
        let target = start
            .checked_add(output_size)
            .ok_or_else(|| RarError::format("RAR 2.0 output size overflows"))?;
        if !packed.is_empty() {
            self.bits = BitReader::new();
        }
        self.bits.append(packed);
        let mut flushed = start;
        while flushed < target {
            let next = (flushed + FLUSH).min(target);
            self.decode_until(next).map_err(map_err)?;
            let pos = self.history.current_pos();
            let chunk = self.history.raw_range(flushed, pos).map_err(map_err)?;
            writer.write_all(chunk).map_err(RarError::Io)?;
            flushed = pos;
            self.history.trim(pos);
        }
        self.read_last_tables().map_err(map_err)?;
        self.history.trim(target);
        Ok(())
    }

    fn decode_until(&mut self, target: usize) -> Res<()> {
        while self.history.current_pos() < target {
            self.history.drain_pending_match(target)?;
            if self.history.current_pos() >= target {
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
        // unrar updates `UnpOldTable20` in place and copies back only the
        // first `TableSize` entries, so positions past the new table keep
        // their stale values (they are the delta base when a later
        // keep-tables block uses a larger table). Start from the previous
        // table and write only the covered positions.
        let mut new_levels = self.levels;
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
                    fill_levels(&mut new_levels[..table_size], &mut pos, count, value)?;
                }
                17 => {
                    let count = 3 + self.bits.read_bits(3)? as usize;
                    fill_levels(&mut new_levels[..table_size], &mut pos, count, 0)?;
                }
                18 => {
                    let count = 11 + self.bits.read_bits(7)? as usize;
                    fill_levels(&mut new_levels[..table_size], &mut pos, count, 0)?;
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
        while self.history.current_pos() < output_size {
            if self.audio_block {
                self.decode_audio_byte()?;
                if !self.in_block {
                    return Ok(());
                }
                continue;
            }
            let symbol = self.main.decode(&mut self.bits)?;
            match symbol {
                0..=255 => self.history.push(symbol as u8),
                256 => {
                    if self.last_length != 0 {
                        let length = self.last_length;
                        let offset = self.last_offset;
                        self.push_old_offset(offset);
                        self.history.copy_match(length, offset, output_size)?;
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
                    self.history.copy_match(length, offset, output_size)?;
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
                    self.history.copy_match(2, offset, output_size)?;
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
                    self.history.copy_match(length, offset, output_size)?;
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
        self.history.push(byte);
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

    /// If the member ended right at a block boundary, consume the trailing
    /// end-of-block marker and load the next block's tables so a following
    /// solid member can continue.
    fn read_last_tables(&mut self) -> Res<()> {
        if self.bits.remaining_bytes_from_current() < 5 {
            return Ok(());
        }
        if self.audio_block {
            if self.audio_tables[self.cur_channel].is_empty() {
                return Ok(());
            }
            if self.audio_tables[self.cur_channel].decode(&mut self.bits)? == 256 {
                self.read_tables()?;
                self.in_block = true;
            }
        } else {
            if self.main.is_empty() {
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
        push_old_offset(&mut self.old_offsets, offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push `count` bits of `value` (MSB first) onto `bits`.
    fn push_bits(bits: &mut Vec<bool>, value: u32, count: u32) {
        for shift in (0..count).rev() {
            bits.push((value >> shift) & 1 != 0);
        }
    }

    fn pack_bits(bits: &[bool]) -> Vec<u8> {
        let mut out = vec![0u8; bits.len().div_ceil(8)];
        for (index, &bit) in bits.iter().enumerate() {
            if bit {
                out[index / 8] |= 1 << (7 - (index % 8));
            }
        }
        out
    }

    /// One `read_tables` input: the block header (audio/keep flags and, for
    /// audio, the channel bits), 19 level lengths all equal to 5 (a valid
    /// 5-bit code per symbol) and one `delta` symbol per table position,
    /// whose 5-bit code equals its value.
    fn level_table_stream(
        audio: bool,
        keep: bool,
        channels: usize,
        table_size: usize,
        delta: u8,
    ) -> Vec<u8> {
        let mut bits = Vec::new();
        push_bits(&mut bits, u32::from(audio), 1);
        push_bits(&mut bits, u32::from(keep), 1);
        if audio {
            push_bits(&mut bits, (channels as u32) - 1, 2);
        }
        for _ in 0..LEVEL_COUNT {
            push_bits(&mut bits, 5, 4);
        }
        // Codes are plain 0..=18 at length 5, so `delta` encodes directly.
        for _ in 0..table_size {
            push_bits(&mut bits, u32::from(delta), 5);
        }
        pack_bits(&bits)
    }

    fn read_tables_from(decoder: &mut Rar20Decoder, stream: &[u8]) {
        decoder.bits = BitReader::new();
        decoder.bits.append(stream);
        decoder.read_tables().expect("valid table");
    }

    /// A keep-tables transition to a *larger* table must keep the stale
    /// entries past the previous table size as the delta base (unrar's
    /// in-place `UnpOldTable20`). Sequence: LZ (374 entries, 9s), audio 1ch
    /// (257, keep: 0..257 = 9, 257..374 stale), LZ keep (reads the stale
    /// 257..374 base). The old zero-filled rebuild lost the stale 9s.
    #[test]
    fn keep_tables_larger_audio_to_lz_retains_stale_entries() {
        let mut decoder = Rar20Decoder::new();
        // All-9 lengths are a valid (incomplete) Huffman table for each count.
        read_tables_from(
            &mut decoder,
            &level_table_stream(false, false, 1, TABLE_COUNT, 9),
        );
        assert_eq!(&decoder.levels[..TABLE_COUNT], &[9u8; TABLE_COUNT]);

        read_tables_from(
            &mut decoder,
            &level_table_stream(true, true, 1, AUDIO_COUNT, 0),
        );
        assert_eq!(&decoder.levels[..AUDIO_COUNT], &[9u8; AUDIO_COUNT]);
        assert_eq!(
            &decoder.levels[AUDIO_COUNT..TABLE_COUNT],
            &[9u8; TABLE_COUNT - AUDIO_COUNT],
            "stale entries beyond the audio table survive the keep-tables rebuild"
        );

        read_tables_from(
            &mut decoder,
            &level_table_stream(false, true, 1, TABLE_COUNT, 0),
        );
        assert_eq!(
            &decoder.levels[..TABLE_COUNT],
            &[9u8; TABLE_COUNT],
            "the larger LZ table uses the preserved stale delta base"
        );
    }
}
