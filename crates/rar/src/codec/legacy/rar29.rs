//! RAR 3.x/4.x member decompressor — the legacy `unp_ver >= 29` codec used by
//! every RAR 3.0–4.x archive (`Rar!\x1a\x07\x00` container): LZSS+Huffman and
//! PPMd variant H blocks.
//!
//! Ported from the decode half of bitplane's `rars` (MIT OR Apache-2.0)
//! `codec/rar29.rs` and `codec/ppmd.rs`, which are validated against genuine
//! WinRAR archives
//! in their own fixture suites. The MSB-first bit reader, canonical-Huffman
//! tables and the sliding history are shared with the RAR 2.x decoder
//! (super::lz); the RAR5 codec stays untouched.
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
//! carries a VM-filter record (LZ symbol 257, or PPMd escape code 3) run the
//! standard filters natively and any other filter program through the
//! [`rarvm`] bytecode interpreter.

use super::encode_core::{
    LENGTH_BASES, LENGTH_BITS, LENGTH_COUNT, SHORT_BASES, SHORT_BITS, push_old_offset,
};
use super::lz::{BitReader, Error as E, History, Huffman, Res, fill_levels};
use super::ppmd::{self, PpmdDecoder};
use super::rarvm;
use crate::error::{RarError, RarResult};

// ── Table geometry ─────────────────────────────────────────────────────────

const MAIN_COUNT: usize = 299;
const OFFSET_COUNT: usize = 60;
const LOW_OFFSET_COUNT: usize = 17;
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
// ── Internal error ─────────────────────────────────────────────────────────

impl From<ppmd::Error> for E {
    fn from(error: ppmd::Error) -> E {
        match error {
            ppmd::Error::InvalidData(message) => E::Bad(message),
            ppmd::Error::NeedMoreInput => E::Truncated,
        }
    }
}

impl From<rarvm::Error> for E {
    fn from(error: rarvm::Error) -> E {
        match error {
            rarvm::Error::InvalidData(message) => E::Bad(message),
            rarvm::Error::NeedMoreInput => E::Truncated,
        }
    }
}

fn map_err(error: E) -> RarError {
    error.into_rar("RAR 2.9")
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
    /// Program globals carried by this filter record (`0x08` bit); empty
    /// means "reuse the program's persistent globals".
    global_data: Vec<u8>,
}

/// A recognized filter program: a standard fingerprint or arbitrary
/// RARVM bytecode.
#[derive(Debug, Clone)]
struct VmProgram {
    kind: VmProgramKind,
    block_size: usize,
    exec_count: u32,
    /// Globals persisted across invocations of a generic program.
    globals: Vec<u8>,
}

#[derive(Debug, Clone)]
enum VmProgramKind {
    Standard(StandardFilter),
    Generic(rarvm::Program),
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
    history: History,
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
            in_lz_block: false,
            block_mode: BlockMode::Lz,
            ppmd: Box::new(PpmdDecoder::new()),
            ppmd_esc: 2,
            filters: Vec::new(),
            programs: Vec::new(),
            last_filter: 0,
            history: History::new(MAX_HISTORY),
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
        let start = self.history.current_pos();
        let target = start
            .checked_add(output_size)
            .ok_or_else(|| RarError::Format("RAR 2.9 output size overflows".into()))?;
        if !packed.is_empty() {
            self.bits = BitReader::new();
        }
        self.bits.append(packed);
        self.decode_until(target).map_err(map_err)?;
        self.finish_member().map_err(map_err)?;
        self.validate_member_filters(start, target)
            .map_err(map_err)?;
        let out = self.filtered_range(start, target, start).map_err(map_err)?;
        self.trim_history(target);
        Ok(out)
    }

    /// Decode a member streaming its output to `writer` with bounded memory
    /// (~window + one flush chunk) instead of accumulating the whole member.
    ///
    /// VM-filter records are handled without materializing the rest of the
    /// member: a flush never passes the start of a filter whose range is not
    /// fully decoded yet, so filtered members still stream out in chunks
    /// (bounded by the flush size plus the largest pending filter block).
    pub(crate) fn decode_member_streaming_to(
        &mut self,
        packed: &[u8],
        output_size: u64,
        writer: &mut dyn std::io::Write,
    ) -> RarResult<()> {
        const FLUSH: usize = 1024 * 1024;
        let output_size = usize::try_from(output_size).map_err(|_| RarError::LimitExceeded {
            limit: u64::MAX,
            context: "RAR 2.9 member is too large for this platform".into(),
        })?;
        let member_start = self.history.current_pos();
        let target = member_start
            .checked_add(output_size)
            .ok_or_else(|| RarError::Format("RAR 2.9 output size overflows".into()))?;
        if !packed.is_empty() {
            self.bits = BitReader::new();
        }
        self.bits.append(packed);

        let mut flushed = member_start;
        let mut decode_target = member_start.saturating_add(FLUSH).min(target);
        while flushed < target {
            self.decode_until(decode_target).map_err(map_err)?;
            // Stop at the next pending filter's start: its bytes can only be
            // reversed once the whole filter range has been decoded.
            let safe_end = self
                .safe_flush_end(flushed, decode_target, target)
                .map_err(map_err)?;
            if safe_end <= flushed {
                if decode_target == target {
                    return Err(map_err(E::Bad("VM filter extends beyond output")));
                }
                decode_target = self.history.current_pos().saturating_add(FLUSH).min(target);
                continue;
            }
            if self.filters.is_empty() {
                let chunk = self.history.raw_range(flushed, safe_end).map_err(map_err)?;
                writer.write_all(chunk).map_err(RarError::Io)?;
            } else {
                let out = self
                    .filtered_range(flushed, safe_end, member_start)
                    .map_err(map_err)?;
                writer.write_all(&out).map_err(RarError::Io)?;
            }
            flushed = safe_end;
            // Drop decoded history beyond the sliding window.
            self.trim_history(flushed);
            decode_target = self.history.current_pos().saturating_add(FLUSH).min(target);
        }
        self.finish_member().map_err(map_err)?;
        self.validate_member_filters(member_start, target)
            .map_err(map_err)?;
        Ok(())
    }

    fn decode_until(&mut self, target: usize) -> Res<()> {
        while self.history.current_pos() < target {
            self.history.drain_pending_match(target)?;
            if self.history.current_pos() >= target {
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
        while self.history.current_pos() < output_size {
            let symbol = self.main.decode(&mut self.bits)?;
            match symbol {
                0..=255 => self.history.push(symbol as u8),
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
                        self.history
                            .copy_match(self.last_length, self.last_offset, output_size)?;
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
                    self.history.copy_match(length, offset, output_size)?;
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
                    self.history.copy_match(2, offset, output_size)?;
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
                    self.history.copy_match(length, offset, output_size)?;
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
            .history
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
            let kind = match identify_standard_filter(&code) {
                Some(standard) => VmProgramKind::Standard(standard),
                None => VmProgramKind::Generic(rarvm::Program::parse(&code)?),
            };
            self.programs.push(VmProgram {
                kind,
                block_size,
                exec_count: 0,
                globals: Vec::new(),
            });
        } else if let Some(program) = self.programs.get_mut(program_index) {
            program.exec_count = program.exec_count.wrapping_add(1);
            program.block_size = block_size;
        }

        let mut global_data = Vec::new();
        if first_byte & 0x08 != 0 {
            // Program globals for this invocation; generic programs use them
            // as their initial global memory.
            let data_size = vm.read_encoded_u32()? as usize;
            global_data.reserve(data_size.min(MAX_VM_GLOBAL_DATA));
            for _ in 0..data_size {
                let byte = vm.read_bits(8)? as u8;
                if global_data.len() < MAX_VM_GLOBAL_DATA {
                    global_data.push(byte);
                }
            }
        }

        if self.filters.len() >= MAX_VM_FILTERS {
            return Err(E::Bad("VM filter limit exceeded"));
        }
        self.filters.push(VmFilter {
            program: program_index,
            start: block_start,
            size: block_size,
            regs,
            global_data,
        });
        Ok(())
    }

    /// Build the decoded byte range `[start, end)`, inverse-transforming
    /// every fully-contained standard filter block (rars `filtered_range`).
    fn filtered_range(&mut self, start: usize, end: usize, member_start: usize) -> Res<Vec<u8>> {
        let mut out = Vec::with_capacity(end - start);
        let mut pos = start;
        let mut filters = Vec::new();
        for (index, filter) in self.filters.iter().enumerate() {
            let filter_end = filter
                .start
                .checked_add(filter.size)
                .ok_or(E::Bad("VM filter range overflows"))?;
            if filter.start >= start && filter_end <= end {
                filters.push((index, filter_end));
            }
        }
        for (filter_index, filter_end) in filters {
            let (program_index, filter_start, regs, global_data) = {
                let filter = self
                    .filters
                    .get(filter_index)
                    .ok_or(E::Bad("VM filter is missing"))?;
                (
                    filter.program,
                    filter.start,
                    filter.regs,
                    filter.global_data.clone(),
                )
            };
            if filter_start < pos {
                continue;
            }
            out.extend_from_slice(self.history.raw_range(pos, filter_start)?);
            let mut block = self.history.raw_range(filter_start, filter_end)?.to_vec();
            let file_offset = filter_start
                .checked_sub(member_start)
                .ok_or(E::Bad("VM filter starts before file"))?
                as u32;
            let program = self
                .programs
                .get_mut(program_index)
                .ok_or(E::Bad("VM program is missing"))?;
            match &program.kind {
                VmProgramKind::Standard(standard) => {
                    apply_standard_filter(*standard, &mut block, file_offset, &regs)?;
                }
                VmProgramKind::Generic(generic) => {
                    let globals = if global_data.is_empty() {
                        program.globals.as_slice()
                    } else {
                        global_data.as_slice()
                    };
                    let result = generic.execute(rarvm::Invocation {
                        input: &block,
                        regs,
                        global_data: globals,
                        file_offset: file_offset as u64,
                        exec_count: program.exec_count,
                    })?;
                    program.globals = result.globals;
                    block = result.output;
                }
            }
            out.extend_from_slice(&block);
            pos = filter_end;
        }
        out.extend_from_slice(self.history.raw_range(pos, end)?);
        Ok(out)
    }

    /// Last output position that can be safely flushed while decoding towards
    /// `end`: a filter whose range is not fully decoded yet forces the flush
    /// to stop at its start. Fails if a filter overruns the member output,
    /// since those bytes could never be transformed.
    fn safe_flush_end(&self, start: usize, end: usize, final_target: usize) -> Res<usize> {
        let current = self.history.current_pos();
        let mut safe_end = end;
        for filter in &self.filters {
            let filter_end = filter
                .start
                .checked_add(filter.size)
                .ok_or(E::Bad("VM filter range overflows"))?;
            if filter.start >= safe_end || filter_end <= start {
                continue;
            }
            if filter_end > final_target {
                return Err(E::Bad("VM filter extends beyond output"));
            }
            if filter_end > current {
                safe_end = safe_end.min(filter.start);
            }
        }
        Ok(safe_end)
    }

    /// Reject VM filter ranges that do not fit inside the member output:
    /// leaving them untransformed would only surface as a CRC mismatch after
    /// the bytes were already written. Filters anchored before `member_start`
    /// belong to an earlier solid-chain member and are not this member's
    /// concern.
    fn validate_member_filters(&self, member_start: usize, member_end: usize) -> Res<()> {
        for filter in &self.filters {
            let filter_end = filter
                .start
                .checked_add(filter.size)
                .ok_or(E::Bad("VM filter range overflows"))?;
            if filter.start >= member_end {
                return Err(E::Bad("VM filter starts beyond output"));
            }
            if filter.start >= member_start && filter_end > member_end {
                return Err(E::Bad("VM filter extends beyond output"));
            }
        }
        Ok(())
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
        while self.history.current_pos() < output_size {
            let Some(symbol) = self.ppmd.decode_symbol(&mut self.bits)? else {
                return Ok(());
            };
            if symbol != self.ppmd_esc {
                self.history.push(symbol);
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
                1 | 6..=u8::MAX => self.history.push(self.ppmd_esc),
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
                    self.history.copy_match(length, offset, output_size)?;
                }
                5 => {
                    let length = self.read_ppmd_required_byte()? as usize + 4;
                    self.history.copy_match(length, 1, output_size)?;
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
        push_old_offset(&mut self.old_offsets, offset);
    }

    /// Drop window state a family keeps alongside the history: applied VM
    /// filters leave the decoder once their range falls behind the window.
    fn trim_history(&mut self, flushed_pos: usize) {
        let keep_from = self.history.trim(flushed_pos);
        self.filters
            .retain(|filter| filter.start.saturating_add(filter.size) > keep_from);
    }

    fn rotate_old_offset(&mut self, index: usize) {
        let value = self.old_offsets[index];
        for i in (1..=index).rev() {
            self.old_offsets[i] = self.old_offsets[i - 1];
        }
        self.old_offsets[0] = value;
    }
}

// ── Standard VM filters ────────────────────────────────────────────────────

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
    /// Collects streamed output and remembers the largest single write, so a
    /// test can assert that filtered members do not fall back to one
    /// whole-member flush.
    #[derive(Default)]
    struct CountingWriter {
        bytes: Vec<u8>,
        max_write: usize,
    }

    impl std::io::Write for CountingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.max_write = self.max_write.max(buffer.len());
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// x86-shaped filler: E8 call opcodes with small relative targets, the
    /// shape the E8 filter rewrites and WinRAR would filter automatically.
    fn x86_like_data(bytes: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(bytes);
        while data.len() < bytes {
            data.extend_from_slice(&[0u8; 48]);
            for k in 0..6u32 {
                data.push(0xE8);
                data.extend_from_slice(&(k * 0x100 + 0x40).to_le_bytes());
                data.extend_from_slice(&[0x90, 0x90]);
            }
        }
        data.truncate(bytes);
        data
    }

    #[test]
    fn streaming_a_large_filtered_member_flushes_in_bounded_chunks() {
        use crate::codec::legacy::rar29_encoder::{
            Rar29FilterKind, Unpack29Encoder, options_for_level,
        };
        let input = x86_like_data(4 * 1024 * 1024);
        let mut encoder = Unpack29Encoder::with_options(options_for_level(1));
        let packed = encoder
            .encode_member_with_filter(&input, Rar29FilterKind::E8, None)
            .expect("encode E8-filtered member");
        assert!(packed.len() < input.len(), "E8 filter should compress");

        let mut writer = CountingWriter::default();
        Rar29Decoder::new()
            .decode_member_streaming_to(&packed, input.len() as u64, &mut writer)
            .expect("streaming filtered decode");
        assert_eq!(writer.bytes, input, "streamed bytes differ from input");
        // E8 records cover 128 KiB blocks, so the whole 4 MiB member must
        // never queue into a single write: each flush stays within one
        // 1 MiB chunk plus one pending filter block.
        assert!(
            writer.max_write <= 2 * 1024 * 1024,
            "filtered streaming was not bounded: {} bytes in one write",
            writer.max_write
        );
    }

    fn decoder_with_zeroes(len: usize) -> Rar29Decoder {
        let mut decoder = Rar29Decoder::new();
        for _ in 0..len {
            decoder.history.push(0);
        }
        decoder
    }

    fn vm_filter(start: usize, size: usize) -> VmFilter {
        VmFilter {
            program: 0,
            start,
            size,
            regs: [0; 7],
            global_data: Vec::new(),
        }
    }

    #[test]
    fn vm_filter_overrunning_the_member_is_rejected() {
        let mut decoder = decoder_with_zeroes(64);
        decoder.filters.push(vm_filter(32, 64)); // ends at 96 > 64
        assert!(decoder.validate_member_filters(0, 64).is_err());
    }

    #[test]
    fn vm_filter_starting_beyond_the_member_is_rejected() {
        let mut decoder = decoder_with_zeroes(64);
        decoder.filters.push(vm_filter(64, 4));
        assert!(decoder.validate_member_filters(0, 64).is_err());
    }

    #[test]
    fn vm_filter_range_overflow_is_rejected() {
        let mut decoder = decoder_with_zeroes(64);
        decoder.filters.push(vm_filter(usize::MAX - 1, 8));
        assert!(decoder.validate_member_filters(0, 64).is_err());
        assert!(decoder.filtered_range(0, 64, 0).is_err());
    }

    #[test]
    fn vm_filter_inside_the_member_is_accepted() {
        let mut decoder = decoder_with_zeroes(64);
        decoder.filters.push(vm_filter(8, 32));
        assert!(decoder.validate_member_filters(0, 64).is_ok());
    }

    /// Trimming the window drops filters whose whole range fell behind it,
    /// so a long filtered member (or solid chain) cannot accumulate past the
    /// filter cap.
    #[test]
    fn trimming_the_window_drops_filters_behind_it() {
        let mut decoder = decoder_with_zeroes(4 * 1024 * 1024 + 64);
        decoder.filters.push(vm_filter(8, 32));
        decoder.filters.push(vm_filter(4 * 1024 * 1024 + 8, 32));
        decoder.trim_history(decoder.history.current_pos());
        assert_eq!(decoder.filters.len(), 1);
        assert_eq!(decoder.filters[0].start, 4 * 1024 * 1024 + 8);
    }
}
