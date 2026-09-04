//! RAR 3.x/4.x member decompressor — the legacy `unp_ver >= 29` codec used by
//! every RAR 3.0–4.x archive (`Rar!\x1a\x07\x00` container): LZSS+Huffman and
//! PPMd variant H blocks.
//!
//! Ported from the decode half of bitplane's `rars` (WTFPL) `codec/rar29.rs`
//! and `codec/ppmd.rs`, which are validated against genuine WinRAR archives
//! in their own fixture suites. The decoder is self-contained (its own
//! MSB-first bit reader and canonical-Huffman tables) and keeps the RAR5
//! codec untouched.
//!
//! A member is a sequence of *blocks*. Each block begins (byte-aligned) with
//! either a PPMd marker (bit 1 + init byte) or an LZSS header that optionally
//! re-reads the four Huffman tables (main/offset/low-offset/length). Solid
//! chains share one decoder instance: the output window, `old_offsets`,
//! last-length/last-offset and (per block header) the tables persist across
//! members, while each member's packed bytes start a fresh bit reader at a
//! block boundary.
//!
//! Only the LZSS and PPMd paths are implemented today; members whose stream
//! carries a VM-filter record (LZ symbol 257, or PPMd escape code 3) fail
//! with a clear [`RarError::Unsupported`] — the filters milestone.

use super::ppmd::{self, PpmdByteReader, PpmdDecoder};
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

// RAR3 VM filter record limits (mirror rars).
const MAX_VM_GLOBAL_DATA: usize = 0x2000;
const MAX_VM_CODE_SIZE: usize = 64 * 1024;
const MAX_VM_PROGRAMS: usize = 8192;
const MAX_VM_FILTERS: usize = 8192;

/// Channel ceiling for DELTA/AUDIO decode (the RAR 2.9 VM takes the channel
/// count from register R[0], so it can exceed RAR 5's 32).
const MAX_DELTA_CHANNELS: usize = 1024;

/// E8/E8E9 transforms assume this fixed 16 MiB file size, as in the
/// reference decoders.
const E8_FILESIZE: u32 = 0x0100_0000;

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

impl From<ppmd::Error> for E {
    fn from(error: ppmd::Error) -> E {
        match error {
            ppmd::Error::InvalidData(message) => E::Bad(message),
            ppmd::Error::NeedMoreInput => E::Truncated,
        }
    }
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

    /// A reader over a standalone byte slice (VM filter record bodies).
    fn from_bytes(input: &[u8]) -> Self {
        Self {
            input: input.to_vec(),
            bit_pos: 0,
        }
    }

    /// RARVM variable-length integer (2-bit tag + payload).
    fn read_encoded_u32(&mut self) -> Res<u32> {
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

impl PpmdByteReader for BitReader {
    fn read_ppmd_byte(&mut self) -> ppmd::Result<u8> {
        self.read_bits(8)
            .map(|value| value as u8)
            .map_err(|error| match error {
                E::Truncated => ppmd::Error::NeedMoreInput,
                E::Bad(message) => ppmd::Error::InvalidData(message),
                E::Unsupported(message) => ppmd::Error::InvalidData(message),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockMode {
    Lz,
    Ppmd,
}

/// One of the five standard RAR3 VM filters, recognized by bytecode
/// fingerprint (length + CRC32, XOR checksum zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StandardFilter {
    E8,
    E8E9,
    Itanium,
    Delta,
    Rgb,
    Audio,
}

/// A pending filter application: transform `size` bytes of decoded output
/// starting at absolute stream position `start`.
#[derive(Debug, Clone)]
struct VmFilter {
    program: usize,
    start: usize,
    size: usize,
    regs: [u32; 7],
}

/// A recognized filter program (standard filters only).
#[derive(Debug, Clone)]
struct VmProgram {
    kind: StandardFilter,
    block_size: usize,
    exec_count: u32,
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
    block_mode: BlockMode,
    /// Boxed: [`PpmdDecoder`] carries ~30 KB of fixed-size model tables and
    /// must not inline into [`crate::archive::ReadState`] (kept by value in
    /// `RarArchive`, whose frames would otherwise balloon past the 1 MiB
    /// main-thread stack on Windows).
    ppmd: Box<PpmdDecoder>,
    ppmd_esc: u8,
    /// Pending VM filter applications, in record order.
    filters: Vec<VmFilter>,
    /// Recognized filter programs.
    programs: Vec<VmProgram>,
    /// Filter number of the last filter record (`0` = default reuse).
    last_filter: usize,
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
            block_mode: BlockMode::Lz,
            ppmd: Box::new(PpmdDecoder::new()),
            ppmd_esc: 2,
            filters: Vec::new(),
            programs: Vec::new(),
            last_filter: 0,
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
        let out = self.filtered_range(start, target, start).map_err(map_err)?;
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
            match self.block_mode {
                BlockMode::Lz => self.decode_lz(target)?,
                BlockMode::Ppmd => self.decode_ppmd(target)?,
            }
        }
        Ok(())
    }

    fn read_tables(&mut self) -> Res<()> {
        self.bits.align_byte();
        if self.bits.peek_bit()? != 0 {
            // PPMd block: the marker bit is followed by an 8-bit init byte
            // (reset/max-order/esc flags), then the range-coder state.
            let first_byte = self.bits.read_bits(8)? as u8;
            self.ppmd
                .decode_init(first_byte, &mut self.bits, &mut self.ppmd_esc)?;
            self.block_mode = BlockMode::Ppmd;
            return Ok(());
        }
        self.bits.read_bit()?;
        self.block_mode = BlockMode::Lz;
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
                    // VM filter record (LZ stream): read + parse, then keep
                    // decoding; the filter applies later to decoded output.
                    self.read_vm_code()?;
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

    /// Read a VM filter record from the LZ bitstream (main symbol 257).
    fn read_vm_code(&mut self) -> Res<()> {
        let first_byte = self.bits.read_bits(8)?;
        let mut len = (first_byte & 7) + 1;
        if len == 7 {
            len = self.bits.read_bits(8)? + 7;
        } else if len == 8 {
            len = self.bits.read_bits(16)?;
        }
        let mut data = Vec::with_capacity(len as usize);
        for _ in 0..len {
            data.push(self.bits.read_bits(8)? as u8);
        }
        self.parse_vm_code(first_byte, data)
    }

    /// Read a VM filter record whose bytes come from the PPMd symbol stream
    /// (escape code 3).
    fn read_vm_code_ppmd(&mut self) -> Res<()> {
        let first_byte = u32::from(self.read_ppmd_required_byte()?);
        let mut len = (first_byte & 7) + 1;
        if len == 7 {
            len = u32::from(self.read_ppmd_required_byte()?) + 7;
        } else if len == 8 {
            len = (u32::from(self.read_ppmd_required_byte()?) << 8)
                | u32::from(self.read_ppmd_required_byte()?);
        }
        let mut data = Vec::with_capacity(len as usize);
        for _ in 0..len {
            data.push(self.read_ppmd_required_byte()?);
        }
        self.parse_vm_code(first_byte, data)
    }

    /// Parse a VM filter record body (rars `parse_vm_code`): locate or load
    /// the program (standard filters only), and append a pending filter.
    fn parse_vm_code(&mut self, first_byte: u32, data: Vec<u8>) -> Res<()> {
        let mut vm = BitReader::from_bytes(&data);
        let program_index = if first_byte & 0x80 != 0 {
            let value = vm.read_encoded_u32()?;
            if value == 0 {
                self.filters.clear();
                self.programs.clear();
                0
            } else {
                usize::try_from(value - 1).map_err(|_| E::Bad("VM program index overflows"))?
            }
        } else {
            self.last_filter
        };
        if program_index > self.programs.len() {
            return Err(E::Bad("VM program index is invalid"));
        }
        self.last_filter = program_index;
        let new_program = program_index == self.programs.len();

        let mut block_start = vm.read_encoded_u32()? as usize;
        if first_byte & 0x40 != 0 {
            block_start += 258;
        }
        block_start = self
            .current_pos()
            .checked_add(block_start)
            .ok_or(E::Bad("VM block start overflows"))?;

        let mut block_size = self
            .programs
            .get(program_index)
            .map(|program| program.block_size)
            .unwrap_or(0);
        if first_byte & 0x20 != 0 {
            block_size = vm.read_encoded_u32()? as usize;
        }

        let mut regs = [0u32; 7];
        regs[3] = 0x3c000;
        regs[4] = block_size as u32;
        if let Some(program) = self.programs.get(program_index) {
            regs[5] = program.exec_count;
        }
        if first_byte & 0x10 != 0 {
            let mask = vm.read_bits(7)?;
            for (index, reg) in regs.iter_mut().enumerate() {
                if mask & (1 << index) != 0 {
                    *reg = vm.read_encoded_u32()?;
                }
            }
        }

        if new_program {
            if self.programs.len() >= MAX_VM_PROGRAMS {
                return Err(E::Bad("VM program limit exceeded"));
            }
            let code_size = vm.read_encoded_u32()? as usize;
            if code_size == 0 {
                return Err(E::Bad("VM code is empty"));
            }
            if code_size > MAX_VM_CODE_SIZE {
                return Err(E::Bad("VM code is too large"));
            }
            let mut code = Vec::with_capacity(code_size);
            for _ in 0..code_size {
                code.push(vm.read_bits(8)? as u8);
            }
            let Some(kind) = identify_standard_filter(&code) else {
                return Err(E::Unsupported(
                    "RAR 3.x/4.x member uses a non-standard VM program, which this decoder does not implement",
                ));
            };
            self.programs.push(VmProgram {
                kind,
                block_size,
                exec_count: 0,
            });
        } else if let Some(program) = self.programs.get_mut(program_index) {
            program.exec_count = program.exec_count.wrapping_add(1);
            program.block_size = block_size;
        }

        let mut global_data = Vec::new();
        if first_byte & 0x08 != 0 {
            // Global data only feeds generic (non-standard) VM programs,
            // which this decoder does not run; still consume the bytes so
            // the record parses and the stream position stays correct.
            let data_size = vm.read_encoded_u32()? as usize;
            global_data.reserve(data_size.min(MAX_VM_GLOBAL_DATA));
            for _ in 0..data_size {
                let byte = vm.read_bits(8)? as u8;
                if global_data.len() < MAX_VM_GLOBAL_DATA {
                    global_data.push(byte);
                }
            }
        }
        let _ = global_data;

        if self.filters.len() >= MAX_VM_FILTERS {
            return Err(E::Bad("VM filter limit exceeded"));
        }
        self.filters.push(VmFilter {
            program: program_index,
            start: block_start,
            size: block_size,
            regs,
        });
        Ok(())
    }

    /// Build the decoded byte range `[start, end)`, inverse-transforming
    /// every fully-contained standard filter block (rars `filtered_range`).
    fn filtered_range(&mut self, start: usize, end: usize, member_start: usize) -> Res<Vec<u8>> {
        let mut out = Vec::with_capacity(end - start);
        let mut pos = start;
        let filters: Vec<_> = self
            .filters
            .iter()
            .enumerate()
            .filter_map(|(index, filter)| {
                (filter.start >= start && filter.start + filter.size <= end).then_some(index)
            })
            .collect();
        for filter_index in filters {
            let (program_index, filter_start, filter_size, regs) = {
                let filter = self
                    .filters
                    .get(filter_index)
                    .ok_or(E::Bad("VM filter is missing"))?;
                (filter.program, filter.start, filter.size, filter.regs)
            };
            if filter_start < pos {
                continue;
            }
            out.extend_from_slice(self.raw_range(pos, filter_start)?);
            let mut block = self
                .raw_range(filter_start, filter_start + filter_size)?
                .to_vec();
            let file_offset = filter_start
                .checked_sub(member_start)
                .ok_or(E::Bad("VM filter starts before file"))?
                as u32;
            let kind = self
                .programs
                .get(program_index)
                .ok_or(E::Bad("VM program is missing"))?
                .kind;
            apply_standard_filter(kind, &mut block, file_offset, &regs)?;
            out.extend_from_slice(&block);
            pos = filter_start + filter_size;
        }
        out.extend_from_slice(self.raw_range(pos, end)?);
        Ok(out)
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
        match self.block_mode {
            BlockMode::Lz => self.finish_lz_member(),
            BlockMode::Ppmd => self.finish_ppmd_member(),
        }
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

    fn decode_ppmd(&mut self, output_size: usize) -> Res<()> {
        while self.current_pos() < output_size {
            let Some(symbol) = self.ppmd.decode_symbol(&mut self.bits)? else {
                return Ok(());
            };
            if symbol != self.ppmd_esc {
                self.output.push(symbol);
                continue;
            }

            let Some(next) = self.ppmd.decode_symbol(&mut self.bits)? else {
                return Ok(());
            };
            match next {
                0 => {
                    self.in_lz_block = false;
                    return Ok(());
                }
                1 | 6..=u8::MAX => self.output.push(self.ppmd_esc),
                2 => {
                    self.in_lz_block = false;
                    return Ok(());
                }
                3 => {
                    // PPMd-embedded VM filter record (RAR3.0+ filters).
                    self.read_vm_code_ppmd()?;
                }
                4 => {
                    let mut offset = 0usize;
                    for _ in 0..3 {
                        offset = (offset << 8) | self.read_ppmd_required_byte()? as usize;
                    }
                    offset += 2;
                    let length = self.read_ppmd_required_byte()? as usize + 32;
                    self.copy_match(length, offset, output_size)?;
                }
                5 => {
                    let length = self.read_ppmd_required_byte()? as usize + 4;
                    self.copy_match(length, 1, output_size)?;
                }
            }
        }
        Ok(())
    }

    fn read_ppmd_required_byte(&mut self) -> Res<u8> {
        self.ppmd
            .decode_symbol(&mut self.bits)?
            .ok_or(E::Bad("PPMd stream ended early"))
    }

    fn finish_ppmd_member(&mut self) -> Res<()> {
        if self.block_mode != BlockMode::Ppmd {
            return Ok(());
        }
        let Some(symbol) = self.ppmd.decode_symbol(&mut self.bits)? else {
            return Ok(());
        };
        if symbol != self.ppmd_esc {
            return Err(E::Bad("PPMd member has trailing data"));
        }
        let Some(next) = self.ppmd.decode_symbol(&mut self.bits)? else {
            return Ok(());
        };
        match next {
            2 | 0 => {
                self.in_lz_block = false;
                Ok(())
            }
            _ => Err(E::Bad("PPMd member has trailing data")),
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
        self.filters
            .retain(|filter| filter.start.saturating_add(filter.size) > self.base_offset);
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

// ── Standard VM filters ────────────────────────────────────────────────────
//
// The five standard RAR3 filters are stored as RARVM bytecode in the stream;
// the decoder recognises them by fingerprint (XOR checksum zero + (length,
// CRC32)) and applies the native inverse transform instead of running a VM.

fn identify_standard_filter(code: &[u8]) -> Option<StandardFilter> {
    if code.iter().fold(0u8, |acc, &byte| acc ^ byte) != 0 {
        return None;
    }
    match (code.len(), crate::crc32::crc32(code)) {
        (53, 0xad57_6887) => Some(StandardFilter::E8),
        (57, 0x3cd7_e57e) => Some(StandardFilter::E8E9),
        (120, 0x3769_893f) => Some(StandardFilter::Itanium),
        (29, 0x0e06_077d) => Some(StandardFilter::Delta),
        (149, 0x1c2c_5dc8) => Some(StandardFilter::Rgb),
        (216, 0xbc85_e701) => Some(StandardFilter::Audio),
        _ => None,
    }
}

fn apply_standard_filter(
    filter: StandardFilter,
    data: &mut Vec<u8>,
    file_offset: u32,
    regs: &[u32; 7],
) -> Res<()> {
    match filter {
        StandardFilter::E8 => e8e9_decode(data, file_offset, false),
        StandardFilter::E8E9 => e8e9_decode(data, file_offset, true),
        StandardFilter::Itanium => itanium_decode(data, file_offset),
        StandardFilter::Delta => {
            let channels = regs[0] as usize;
            if channels == 0 || channels > MAX_DELTA_CHANNELS {
                return Err(E::Bad("DELTA filter channel count is invalid"));
            }
            *data = delta_decode(data, channels)?;
            Ok(())
        }
        StandardFilter::Rgb => {
            if regs[0] < 3 || regs[1] > 2 {
                return Err(E::Bad("RGB filter parameters are invalid"));
            }
            let width = regs[0] as usize - 3;
            let pos_r = regs[1] as usize;
            *data = rgb_decode(data, width, pos_r)?;
            Ok(())
        }
        StandardFilter::Audio => {
            let channels = regs[0] as usize;
            if channels == 0 || channels > MAX_DELTA_CHANNELS {
                return Err(E::Bad("AUDIO filter channel count is invalid"));
            }
            *data = audio_decode(data, channels)?;
            Ok(())
        }
    }
}

/// Inverse x86 E8/E8E9 transform (relative -> absolute call/jump targets).
fn e8e9_decode(data: &mut [u8], file_offset: u32, include_e9: bool) -> Res<()> {
    if data.len() <= 4 {
        return Ok(());
    }
    let cmp_mask = if include_e9 { 0xfe } else { 0xff };
    let opcode_limit = data.len() - 4;
    let mut opcode_pos = 0usize;
    while let Some(pos) = next_x86_opcode(data, opcode_pos, opcode_limit, cmp_mask) {
        let cur_pos = pos + 1;
        let offset = file_offset.wrapping_add(cur_pos as u32);
        let addr = u32::from_le_bytes([
            data[cur_pos],
            data[cur_pos + 1],
            data[cur_pos + 2],
            data[cur_pos + 3],
        ]);
        let new_addr = if addr < E8_FILESIZE {
            Some(addr.wrapping_sub(offset))
        } else if addr & 0x8000_0000 != 0 && addr.wrapping_add(offset) & 0x8000_0000 == 0 {
            Some(addr.wrapping_add(E8_FILESIZE))
        } else {
            None
        };
        if let Some(value) = new_addr {
            data[cur_pos..cur_pos + 4].copy_from_slice(&value.to_le_bytes());
        }
        opcode_pos = pos + 5;
    }
    Ok(())
}

/// First position >= `start` (exclusive of `end_exclusive`) whose byte
/// matches the E8/E8E9 opcode mask.
fn next_x86_opcode(data: &[u8], start: usize, end_exclusive: usize, cmp_mask: u8) -> Option<usize> {
    data.get(start..end_exclusive.min(data.len()))?
        .iter()
        .position(|&byte| byte & cmp_mask == 0xe8)
        .map(|offset| start + offset)
}

/// Inverse DELTA transform: de-interleave channels, then undo byte deltas.
fn delta_decode(data: &[u8], channels: usize) -> Res<Vec<u8>> {
    if channels == 0 {
        return Err(E::Bad("DELTA filter has zero channels"));
    }
    if channels > MAX_DELTA_CHANNELS {
        return Err(E::Bad("DELTA filter channel count is invalid"));
    }
    let mut out = vec![0u8; data.len()];
    let mut src = 0usize;
    for channel in 0..channels {
        let mut prev = 0u8;
        let mut dest = channel;
        while dest < out.len() {
            let byte = *data
                .get(src)
                .ok_or(E::Bad("DELTA filter source is truncated"))?;
            prev = prev.wrapping_sub(byte);
            out[dest] = prev;
            src += 1;
            dest += channels;
        }
    }
    Ok(out)
}

fn itanium_decode(data: &mut [u8], file_offset: u32) -> Res<()> {
    if data.len() <= 21 {
        return Ok(());
    }
    let base_offset = file_offset >> 4;
    // Each 16-byte Itanium bundle can inspect a 4-byte instruction field
    // that starts up to 13 bytes into the bundle. Keeping a 21-byte tail
    // prevents decoding a partial final bundle.
    let block_count = (data.len() - 21).div_ceil(16);
    for block in 0..block_count {
        let pos = block * 16;
        let file_offset = base_offset.wrapping_add(block as u32);
        let mut mask = (0x334b_0000u32 >> (data[pos] & 0x1e)) & 3;
        if mask != 0 {
            mask += 1;
            while mask <= 4 {
                let p = pos + (mask as usize * 5 - 8);
                if ((data[p + 3] >> mask) & 15) == 5 {
                    let raw = u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
                    let mut value = raw >> mask;
                    value = value.wrapping_sub(file_offset) & 0x000f_ffff;
                    let raw = (raw & !(0x000f_ffff << mask)) | (value << mask);
                    data[p..p + 4].copy_from_slice(&raw.to_le_bytes());
                }
                mask += 1;
            }
        }
    }
    Ok(())
}

fn rgb_decode(data: &[u8], width: usize, pos_r: usize) -> Res<Vec<u8>> {
    if data.len() < 3 || width == 0 || !width.is_multiple_of(3) || width > data.len() || pos_r > 2 {
        return Err(E::Bad("RGB filter parameters are invalid"));
    }
    let mut out = vec![0u8; data.len()];
    let mut src = 0usize;
    for channel in 0..3 {
        let mut prev = 0u8;
        let mut i = channel;
        while i < data.len() {
            let predicted = if i >= width + 3 {
                rgb_predict(prev, out[i - width], out[i - width - 3])
            } else {
                prev
            };
            let encoded = *data
                .get(src)
                .ok_or(E::Bad("RGB filter source is truncated"))?;
            prev = predicted.wrapping_sub(encoded);
            out[i] = prev;
            src += 1;
            i += 3;
        }
    }
    for i in (pos_r..data.len().saturating_sub(2)).step_by(3) {
        let green = out[i + 1];
        out[i] = out[i].wrapping_add(green);
        out[i + 2] = out[i + 2].wrapping_add(green);
    }
    Ok(out)
}

fn rgb_predict(prev: u8, upper: u8, upper_left: u8) -> u8 {
    let predicted = i32::from(prev) + i32::from(upper) - i32::from(upper_left);
    let pa = (predicted - i32::from(prev)).abs();
    let pb = (predicted - i32::from(upper)).abs();
    let pc = (predicted - i32::from(upper_left)).abs();
    if pa <= pb && pa <= pc {
        prev
    } else if pb <= pc {
        upper
    } else {
        upper_left
    }
}

fn audio_decode(data: &[u8], channels: usize) -> Res<Vec<u8>> {
    let mut out = vec![0u8; data.len()];
    let mut src = 0usize;
    for channel in 0..channels {
        let mut prev_byte = 0u32;
        let mut prev_delta = 0i32;
        let mut d1 = 0i32;
        let mut d2 = 0i32;
        let mut k1 = 0i32;
        let mut k2 = 0i32;
        let mut k3 = 0i32;
        let mut dif = [0u32; 7];
        let mut byte_count = 0usize;
        let mut i = channel;
        while i < data.len() {
            let d3 = d2;
            d2 = prev_delta - d1;
            d1 = prev_delta;
            let predicted = ((8 * prev_byte as i32 + k1 * d1 + k2 * d2 + k3 * d3) >> 3) & 0xff;
            let encoded = *data
                .get(src)
                .ok_or(E::Bad("AUDIO filter source is truncated"))?;
            src += 1;
            let decoded = (predicted as u8).wrapping_sub(encoded);
            out[i] = decoded;
            prev_delta = decoded.wrapping_sub(prev_byte as u8) as i8 as i32;
            prev_byte = decoded as u32;
            let d = (encoded as i8 as i32) << 3;
            dif[0] += d.unsigned_abs();
            dif[1] += (d - d1).unsigned_abs();
            dif[2] += (d + d1).unsigned_abs();
            dif[3] += (d - d2).unsigned_abs();
            dif[4] += (d + d2).unsigned_abs();
            dif[5] += (d - d3).unsigned_abs();
            dif[6] += (d + d3).unsigned_abs();
            if byte_count & 0x1f == 0 {
                let mut min = dif[0];
                let mut min_index = 0usize;
                dif[0] = 0;
                for (index, value) in dif.iter_mut().enumerate().skip(1) {
                    if *value < min {
                        min = *value;
                        min_index = index;
                    }
                    *value = 0;
                }
                match min_index {
                    1 if k1 >= -16 => k1 -= 1,
                    2 if k1 < 16 => k1 += 1,
                    3 if k2 >= -16 => k2 -= 1,
                    4 if k2 < 16 => k2 += 1,
                    5 if k3 >= -16 => k3 -= 1,
                    6 if k3 < 16 => k3 += 1,
                    _ => {}
                }
            }
            byte_count += 1;
            i += channels;
        }
    }
    Ok(out)
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
