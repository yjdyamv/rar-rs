/// RAR4 LZSS+Huffman decompressor.
///
/// Clean-room implementation based on the decompression algorithm from
/// libarchive's archive_read_support_format_rar.c (BSD-2-Clause).
use crate::codec::bitstream::BitReader;
use crate::codec::huffman::{DecodeTable, decode_symbol};
use crate::codec::window::SlidingWindow;

use super::constants::*;

// ── Lookup Tables ─────────────────────────────────────────────────────────

/// Length base values (28 entries, used for match length decoding).
static LENGTH_BASES: [u32; 28] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224,
];

/// Extra bits for each length slot.
static LENGTH_BITS: [u8; 28] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5,
];

/// Distance base values (60 entries).
static OFFSET_BASES: [u32; 60] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536,
    2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072, 196608,
    262144, 327680, 393216, 458752, 524288, 589824, 655360, 720896, 786432, 851968, 917504, 983040,
    1048576, 1310720, 1572864, 1835008, 2097152, 2359296, 2621440, 2883584, 3145728, 3407872,
    3670016, 3932160,
];

/// Extra bits for each distance slot.
static OFFSET_BITS: [u8; 60] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14, 15, 15, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 18, 18, 18, 18, 18,
    18, 18, 18, 18, 18, 18, 18,
];

/// Short offset base values (8 entries, for symbols 263-270).
static SHORT_BASES: [u32; 8] = [0, 4, 8, 16, 32, 64, 128, 192];

/// Extra bits for short offsets.
static SHORT_BITS: [u8; 8] = [2, 2, 3, 4, 5, 6, 6, 6];

// ── Standard filter fingerprints ──────────────────────────────────────────

const FILTER_DELTA_FP: u64 = 0x1D_0E06077D;
const FILTER_E8_FP: u64 = 0x35_AD576887;
const FILTER_E8E9_FP: u64 = 0x39_3CD7E57E;
const FILTER_RGB_FP: u64 = 0x95_1C2C5DC8;
const FILTER_AUDIO_FP: u64 = 0xD8_BC85E701;

/// VM memory size (256 KB + 4 bytes).
const VM_MEMORY_SIZE: usize = 0x40000 + 4;
/// Fixed filesize constant used by E8/E8E9 filters.
const E8_FILESIZE: u32 = 0x1000000;

// ── Filter types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Rar4Filter {
    /// Absolute position in the LZSS output where filter input begins.
    block_start: u64,
    /// Number of bytes the filter processes.
    block_length: u32,
    /// Filter fingerprint (CRC32 | (len << 32)).
    fingerprint: u64,
    /// Initial register values.
    registers: [u32; 8],
}

/// Stored filter program (for reuse across invocations).
#[derive(Debug, Clone)]
struct FilterProgram {
    fingerprint: u64,
    old_filter_length: u32,
    usage_count: u32,
}

// ── Decoder State ─────────────────────────────────────────────────────────

/// Persistent decoder state for RAR4 solid archives.
pub struct Rar4DecoderState {
    pub window: SlidingWindow,
    pub old_offset: [u32; 4],
    pub last_offset: u32,
    pub last_length: u32,
    pub last_low_offset: u32,
    pub num_low_offset_repeats: u32,
    /// Persistent length table for incremental Huffman table updates.
    pub length_table: [u8; HUFFMAN_TABLE_SIZE],
    pub table_main: Option<DecodeTable>,
    pub table_offset: Option<DecodeTable>,
    pub table_low_offset: Option<DecodeTable>,
    pub table_length: Option<DecodeTable>,
    /// Whether tables need to be read at the start of the next block.
    pub start_new_table: bool,
    /// Pending filters to be applied at specific output positions.
    pending_filters: Vec<Rar4Filter>,
    /// Stored filter programs (indexed by filter number).
    programs: Vec<FilterProgram>,
    /// Last filter number used (for implicit reuse).
    last_filter_num: usize,
}

const HUFFMAN_TABLE_SIZE: usize = RAR4_NC + RAR4_DC + RAR4_LDC + RAR4_RC; // 404

impl Rar4DecoderState {
    pub fn new(dict_size: usize) -> Self {
        Rar4DecoderState {
            window: SlidingWindow::new(dict_size),
            old_offset: [0; 4],
            last_offset: 0,
            last_length: 0,
            last_low_offset: 0,
            num_low_offset_repeats: 0,
            length_table: [0; HUFFMAN_TABLE_SIZE],
            table_main: None,
            table_offset: None,
            table_low_offset: None,
            table_length: None,
            start_new_table: true,
            pending_filters: Vec::new(),
            programs: Vec::new(),
            last_filter_num: 0,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────

/// Decompress RAR4 LZSS+Huffman data.
pub fn rar4_decompress(
    data: &[u8],
    unpacked_size: u64,
    state: Option<&mut Rar4DecoderState>,
) -> Result<Vec<u8>, String> {
    if let Some(st) = state {
        decompress_inner(data, unpacked_size, st)
    } else {
        let dict_size = compute_dict_size(unpacked_size);
        let mut st = Rar4DecoderState::new(dict_size);
        decompress_inner(data, unpacked_size, &mut st)
    }
}

fn compute_dict_size(unpacked_size: u64) -> usize {
    let mut size = RAR4_DEFAULT_DICT_SIZE.min(unpacked_size as usize);
    // Round up to next power of 2, minimum 64KB
    size = size.max(65536);
    if !size.is_power_of_two() {
        size = size.next_power_of_two();
    }
    size.min(RAR4_DEFAULT_DICT_SIZE)
}

fn decompress_inner(
    data: &[u8],
    unpacked_size: u64,
    state: &mut Rar4DecoderState,
) -> Result<Vec<u8>, String> {
    let mut reader = BitReader::new(data);
    let mut output = Vec::with_capacity(unpacked_size as usize);
    let output_start = state.window.total_written();
    let mut last_flush = output_start;

    loop {
        let written_total = state.window.total_written() - output_start;
        if written_total >= unpacked_size {
            break;
        }

        if state.start_new_table {
            parse_codes(&mut reader, state)?;
            state.start_new_table = false;
        }

        // Check for pending filters whose data range is now fully decompressed.
        // Flush up to the filter end, apply the filter, then continue.
        while let Some(filter) = state.pending_filters.first() {
            let filter_end = filter.block_start + filter.block_length as u64;
            if state.window.total_written() >= filter_end {
                // Flush window data up through the filter end
                let flush_end = filter_end;
                if flush_end <= last_flush {
                    // Already flushed past this filter, just apply it
                    let filter = state.pending_filters.remove(0);
                    let rel_start = (filter.block_start - output_start) as usize;
                    let block_len = filter.block_length as usize;
                    if rel_start + block_len <= output.len() {
                        let region = &mut output[rel_start..rel_start + block_len];
                        execute_filter(&filter, region, filter.block_start)?;
                    }
                    continue;
                }
                let to_flush = flush_end - last_flush;
                if to_flush > 0 {
                    let chunk = state.window.get_output(last_flush, to_flush as usize);
                    output.extend_from_slice(&chunk);
                    last_flush = flush_end;
                }
                // Apply the filter to the output buffer
                let filter = state.pending_filters.remove(0);
                let rel_start = (filter.block_start - output_start) as usize;
                let block_len = filter.block_length as usize;
                if rel_start + block_len <= output.len() {
                    let region = &mut output[rel_start..rel_start + block_len];
                    execute_filter(&filter, region, filter.block_start)?;
                }
            } else {
                break;
            }
        }

        let mut need_filter_read = false;

        loop {
            let written_total = state.window.total_written() - output_start;
            if written_total >= unpacked_size {
                break;
            }
            if reader.bits_remaining() == 0 {
                break;
            }

            // If a pending filter exists, stop decompressing at its end
            // so we can flush and apply the filter
            if let Some(filter) = state.pending_filters.first() {
                let filter_end = filter.block_start + filter.block_length as u64;
                if state.window.total_written() >= filter_end {
                    break; // go to outer loop to apply filter
                }
            }

            // Periodically flush the window to avoid overflow
            let unflushed = state.window.total_written() - last_flush;
            let window_size = state.window.size();
            if unflushed as usize >= window_size / 2 {
                // Determine safe flush boundary: up to the earliest pending
                // filter's block_start or current position, whichever is less
                let flush_to = if let Some(f) = state.pending_filters.first() {
                    if f.block_start > last_flush {
                        f.block_start.min(state.window.total_written())
                    } else {
                        // Filter start is at or before last_flush, can't flush more
                        last_flush
                    }
                } else {
                    state.window.total_written()
                };
                let to_flush = flush_to - last_flush;
                if to_flush > 0 {
                    let chunk = state.window.get_output(last_flush, to_flush as usize);
                    output.extend_from_slice(&chunk);
                    last_flush = flush_to;
                }
            }

            if need_filter_read {
                read_filter(&mut reader, state)?;
                break;
            }

            let t_main = state.table_main.as_ref().ok_or("no Huffman tables")?;
            let t_offset = state.table_offset.as_ref().ok_or("no Huffman tables")?;
            let t_low = state.table_low_offset.as_ref().ok_or("no Huffman tables")?;
            let t_length = state.table_length.as_ref().ok_or("no Huffman tables")?;

            let sym = decode_symbol(t_main, &mut reader).map_err(|e| e.to_string())?;

            if sym < 256 {
                // Literal byte
                state.window.put_byte(sym as u8);
            } else if sym == 256 {
                // Block control
                let new_file = reader.read_bits(1).map_err(|e| e.to_string())? == 0;
                if new_file {
                    state.start_new_table = reader.read_bits(1).map_err(|e| e.to_string())? != 0;
                    break;
                } else {
                    parse_codes(&mut reader, state)?;
                    break; // re-bind table refs
                }
            } else if sym == 257 {
                // VM filter command — need mutable state, break and handle
                need_filter_read = true;
                continue;
            } else if sym == 258 {
                // Repeat last match
                if state.last_length == 0 {
                    continue;
                }
                let offs = state.last_offset;
                let len = state.last_length;
                if offs > 0 {
                    state.window.copy_match(offs as usize, len as usize);
                }
            } else if sym >= 259 && sym <= 262 {
                // Old offset cache reference
                let cache_idx = sym - 259;
                let offs = state.old_offset[cache_idx];

                let len_sym = decode_symbol(t_length, &mut reader).map_err(|e| e.to_string())?;
                let mut len = LENGTH_BASES[len_sym] + 2;
                if LENGTH_BITS[len_sym] > 0 {
                    len += reader
                        .read_bits(LENGTH_BITS[len_sym])
                        .map_err(|e| e.to_string())?;
                }

                let o = state.old_offset[cache_idx];
                for i in (1..=cache_idx).rev() {
                    state.old_offset[i] = state.old_offset[i - 1];
                }
                state.old_offset[0] = o;

                state.last_offset = offs;
                state.last_length = len;
                if offs > 0 {
                    state.window.copy_match(offs as usize, len as usize);
                }
            } else if sym >= 263 && sym <= 270 {
                // Short offset match (length = 2)
                let idx = sym - 263;
                let mut offs = SHORT_BASES[idx] + 1;
                if SHORT_BITS[idx] > 0 {
                    offs += reader
                        .read_bits(SHORT_BITS[idx])
                        .map_err(|e| e.to_string())?;
                }

                state.old_offset[3] = state.old_offset[2];
                state.old_offset[2] = state.old_offset[1];
                state.old_offset[1] = state.old_offset[0];
                state.old_offset[0] = offs;

                state.last_offset = offs;
                state.last_length = 2;
                state.window.copy_match(offs as usize, 2);
            } else if sym >= 271 && sym < 299 {
                // Full match (length + distance)
                let len_idx = sym - 271;
                let mut len = LENGTH_BASES[len_idx] + 3;
                if LENGTH_BITS[len_idx] > 0 {
                    len += reader
                        .read_bits(LENGTH_BITS[len_idx])
                        .map_err(|e| e.to_string())?;
                }

                let off_sym = decode_symbol(t_offset, &mut reader).map_err(|e| e.to_string())?;
                let mut offs = OFFSET_BASES[off_sym] + 1;

                if OFFSET_BITS[off_sym] > 0 {
                    if off_sym > 9 {
                        if OFFSET_BITS[off_sym] > 4 {
                            let upper = reader
                                .read_bits(OFFSET_BITS[off_sym] - 4)
                                .map_err(|e| e.to_string())?;
                            offs += upper << 4;
                        }
                        if state.num_low_offset_repeats > 0 {
                            state.num_low_offset_repeats -= 1;
                            offs += state.last_low_offset;
                        } else {
                            let low_sym =
                                decode_symbol(t_low, &mut reader).map_err(|e| e.to_string())?;
                            if low_sym == 16 {
                                state.num_low_offset_repeats = 15;
                                offs += state.last_low_offset;
                            } else {
                                offs += low_sym as u32;
                                state.last_low_offset = low_sym as u32;
                            }
                        }
                    } else {
                        offs += reader
                            .read_bits(OFFSET_BITS[off_sym])
                            .map_err(|e| e.to_string())?;
                    }
                }

                if offs >= 0x2000 {
                    len += 1;
                }
                if offs >= 0x40000 {
                    len += 1;
                }

                state.old_offset[3] = state.old_offset[2];
                state.old_offset[2] = state.old_offset[1];
                state.old_offset[1] = state.old_offset[0];
                state.old_offset[0] = offs;

                state.last_offset = offs;
                state.last_length = len;
                state.window.copy_match(offs as usize, len as usize);
            }
        }

        let written_total = state.window.total_written() - output_start;
        if written_total >= unpacked_size {
            break;
        }
        if reader.bits_remaining() == 0 && !state.start_new_table {
            break;
        }
    }

    // Final flush: get any remaining data from the window
    let remaining = state.window.total_written() - last_flush;
    if remaining > 0 {
        let chunk = state.window.get_output(last_flush, remaining as usize);
        output.extend_from_slice(&chunk);
    }

    // Apply any remaining pending filters
    for filter in state.pending_filters.drain(..) {
        let rel_start = filter.block_start.saturating_sub(output_start) as usize;
        let block_len = filter.block_length as usize;
        if rel_start + block_len <= output.len() {
            let region = &mut output[rel_start..rel_start + block_len];
            execute_filter(&filter, region, filter.block_start)?;
        }
    }

    output.truncate(unpacked_size as usize);
    Ok(output)
}

// ── Filter Reading ────────────────────────────────────────────────────────

/// Read a VM filter command from the bitstream (symbol 257).
fn read_filter(reader: &mut BitReader, state: &mut Rar4DecoderState) -> Result<(), String> {
    // Read flags byte
    let flags = rar_decode_byte(reader)?;

    // Read blob length (contains filter params + optionally program bytecode)
    let code_len = {
        let len_field = (flags & 0x07) + 1;
        if len_field == 7 {
            let v = rar_decode_byte(reader)? as usize;
            v + 7 // 7..262
        } else if len_field == 8 {
            let hi = rar_decode_byte(reader)? as usize;
            let lo = rar_decode_byte(reader)? as usize;
            (hi << 8) | lo // 0..65535
        } else {
            len_field as usize // 1..6
        }
    };

    // Read the blob
    let mut code = vec![0u8; code_len];
    for byte in code.iter_mut() {
        *byte = rar_decode_byte(reader)?;
    }

    // Parse filter parameters from the blob using a secondary bit reader
    let mut br = MemBitReader::new(&code);

    // Count existing programs
    let num_progs = state.programs.len();

    // Filter number
    let filter_num = if flags & 0x80 != 0 {
        let num = membr_next_rarvm_number(&mut br)?;
        if num == 0 {
            // Reset: clear all programs and filters
            state.programs.clear();
            state.pending_filters.clear();
        } else {
            // num is 1-based when nonzero
        }
        let num = if num > 0 { (num - 1) as usize } else { 0 };
        if num > num_progs {
            return Err("Invalid filter number".into());
        }
        state.last_filter_num = num;
        num
    } else {
        state.last_filter_num
    };

    // Get existing program (if any)
    let prog_exists = filter_num < state.programs.len();
    if prog_exists {
        state.programs[filter_num].usage_count += 1;
    }

    // Block start position (relative to current LZSS write position)
    let mut block_start_pos =
        membr_next_rarvm_number(&mut br)? as u64 + state.window.total_written();
    if flags & 0x40 != 0 {
        block_start_pos += 258;
    }

    // Block length
    let block_length = if flags & 0x20 != 0 {
        let len = membr_next_rarvm_number(&mut br)?;
        len
    } else if prog_exists {
        state.programs[filter_num].old_filter_length
    } else {
        0
    };

    // Initialize registers
    let mut registers = [0u32; 8];
    registers[3] = 0x3C000; // PROGRAM_SYSTEM_GLOBAL_ADDRESS
    registers[4] = block_length;
    registers[5] = if prog_exists {
        state.programs[filter_num].usage_count
    } else {
        0
    };
    registers[7] = VM_MEMORY_SIZE as u32;

    // Optional register overrides
    if flags & 0x10 != 0 {
        let mask = membr_bits(&mut br, 7)?;
        for i in 0..7 {
            if mask & (1 << i) != 0 {
                registers[i] = membr_next_rarvm_number(&mut br)?;
            }
        }
    }

    // If program doesn't exist yet, read bytecode and compile (compute fingerprint)
    let fingerprint = if !prog_exists {
        let bytecode_len = membr_next_rarvm_number(&mut br)? as usize;
        if bytecode_len == 0 || bytecode_len > 0x10000 {
            return Err("Invalid filter bytecode length".into());
        }
        let mut bytecode = vec![0u8; bytecode_len];
        for byte in bytecode.iter_mut() {
            *byte = membr_bits(&mut br, 8)? as u8;
        }

        // Fingerprint = CRC32 of bytecode | (len << 32)
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&bytecode);
        let crc = hasher.finalize();
        let fp = (bytecode_len as u64) << 32 | crc as u64;

        // Store new program
        while state.programs.len() <= filter_num {
            state.programs.push(FilterProgram {
                fingerprint: 0,
                old_filter_length: 0,
                usage_count: 0,
            });
        }
        state.programs[filter_num].fingerprint = fp;
        state.programs[filter_num].usage_count = 1;
        fp
    } else {
        state.programs[filter_num].fingerprint
    };

    // Update old_filter_length
    if filter_num < state.programs.len() {
        state.programs[filter_num].old_filter_length = block_length;
    }

    // Store the filter
    state.pending_filters.push(Rar4Filter {
        block_start: block_start_pos,
        block_length,
        fingerprint,
        registers,
    });

    Ok(())
}

/// Read a byte from the Huffman-decoded bitstream.
fn rar_decode_byte(reader: &mut BitReader) -> Result<u8, String> {
    reader
        .read_bits(8)
        .map(|v| v as u8)
        .map_err(|e| e.to_string())
}

// ── Memory bit reader for VM bytecode parsing ─────────────────────────────

struct MemBitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_pos: u8,
}

impl<'a> MemBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        MemBitReader {
            data,
            pos: 0,
            bit_pos: 0,
        }
    }
}

fn membr_bits(br: &mut MemBitReader, mut n: u8) -> Result<u32, String> {
    let mut result: u32 = 0;
    while n > 0 {
        if br.pos >= br.data.len() {
            return Ok(result); // pad with zeros
        }
        let avail = 8 - br.bit_pos;
        let take = avail.min(n);
        let shift = avail - take;
        let mask = ((1u16 << take) - 1) as u8;
        let bits = (br.data[br.pos] >> shift) & mask;
        result = (result << take) | bits as u32;
        br.bit_pos += take;
        n -= take;
        if br.bit_pos >= 8 {
            br.bit_pos = 0;
            br.pos += 1;
        }
    }
    Ok(result)
}

/// Read a variable-length integer from the bytecode.
fn membr_next_rarvm_number(br: &mut MemBitReader) -> Result<u32, String> {
    let tag = membr_bits(br, 2)?;
    match tag {
        0 => membr_bits(br, 4),
        1 => {
            let val = membr_bits(br, 8)?;
            if val >= 16 {
                Ok(val)
            } else {
                let low = membr_bits(br, 4)?;
                Ok(0xFFFFFF00u32.wrapping_add(val << 4).wrapping_add(low))
            }
        }
        2 => membr_bits(br, 16),
        3 => membr_bits(br, 32),
        _ => unreachable!(),
    }
}

// ── Filter Application ────────────────────────────────────────────────────

/// Execute a single filter on a data region.
fn execute_filter(filter: &Rar4Filter, data: &mut [u8], file_pos: u64) -> Result<(), String> {
    match filter.fingerprint {
        FILTER_E8_FP => {
            execute_filter_e8(data, file_pos as u32, false);
            Ok(())
        }
        FILTER_E8E9_FP => {
            execute_filter_e8(data, file_pos as u32, true);
            Ok(())
        }
        FILTER_DELTA_FP => {
            let channels = filter.registers[0] as usize;
            if channels > 0 {
                execute_filter_delta(data, channels);
            }
            Ok(())
        }
        FILTER_RGB_FP => {
            let width = filter.registers[0] as usize;
            let pos_r = filter.registers[1] as usize;
            execute_filter_rgb(data, width, pos_r);
            Ok(())
        }
        FILTER_AUDIO_FP => {
            let channels = filter.registers[0] as usize;
            if channels > 0 {
                execute_filter_audio(data, channels);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// E8/E8E9 filter: convert relative x86 CALL/JMP addresses to absolute.
fn execute_filter_e8(data: &mut [u8], pos: u32, e9_also: bool) {
    if data.len() < 5 {
        return;
    }
    let mut i = 0;
    while i <= data.len() - 5 {
        if data[i] == 0xE8 || (e9_also && data[i] == 0xE9) {
            let cur_pos = pos.wrapping_add(i as u32).wrapping_add(1);
            let addr = u32::from_le_bytes([data[i + 1], data[i + 2], data[i + 3], data[i + 4]]);
            let addr_i32 = addr as i32;

            let new_addr = if addr_i32 < 0 {
                if cur_pos >= (!(addr) + 1) {
                    addr.wrapping_add(E8_FILESIZE)
                } else {
                    addr
                }
            } else if (addr as u32) < E8_FILESIZE {
                addr.wrapping_sub(cur_pos)
            } else {
                addr
            };

            let bytes = new_addr.to_le_bytes();
            data[i + 1] = bytes[0];
            data[i + 2] = bytes[1];
            data[i + 3] = bytes[2];
            data[i + 4] = bytes[3];
            i += 5;
        } else {
            i += 1;
        }
    }
}

/// Delta filter: de-interleave channels and apply delta decoding.
/// Matches libarchive's execute_filter_delta exactly.
fn execute_filter_delta(data: &mut [u8], channels: usize) {
    let length = data.len();
    if channels == 0 || length == 0 {
        return;
    }

    let src = data.to_vec();
    let mut dst = vec![0u8; length];
    let mut src_pos = 0usize;
    for ch in 0..channels {
        let mut last_byte: u8 = 0;
        let mut j = ch;
        while j < length {
            if src_pos >= src.len() {
                break;
            }
            last_byte = last_byte.wrapping_sub(src[src_pos]);
            src_pos += 1;
            dst[j] = last_byte;
            j += channels;
        }
    }

    data.copy_from_slice(&dst);
}

/// RGB filter: predictor for 3-channel image data.
/// Matches libarchive's execute_filter_rgb exactly.
fn execute_filter_rgb(data: &mut [u8], stride: usize, byte_offset: usize) {
    let length = data.len();
    if stride > length || length < 3 || byte_offset > 2 {
        return;
    }

    let src = data.to_vec();
    let mut dst = vec![0u8; length];
    let mut src_pos = 0usize;

    for i in 0..3usize {
        let mut byte: u8 = 0;
        let mut j = i;
        while j < length {
            if src_pos >= src.len() {
                break;
            }
            // Predictor: use "above" value if available
            if j >= stride {
                let prev_idx = j - stride;
                if prev_idx + 3 < length && j >= 3 {
                    let delta1 = (dst[prev_idx + 3] as i32 - dst[prev_idx] as i32).unsigned_abs();
                    let delta2 = (byte as i32 - dst[prev_idx] as i32).unsigned_abs();
                    let delta3 = (dst[prev_idx + 3] as i32 - dst[prev_idx] as i32 + byte as i32
                        - dst[prev_idx] as i32)
                        .unsigned_abs();
                    if delta1 > delta2 || delta1 > delta3 {
                        byte = if delta2 <= delta3 {
                            dst[prev_idx + 3]
                        } else {
                            dst[prev_idx]
                        };
                    }
                }
            }
            byte = byte.wrapping_sub(src[src_pos]);
            src_pos += 1;
            dst[j] = byte;
            j += 3;
        }
    }

    // Add green channel to the other two channels
    let mut i = byte_offset;
    while i + 2 < length {
        dst[i] = dst[i].wrapping_add(dst[i + 1]);
        dst[i + 2] = dst[i + 2].wrapping_add(dst[i + 1]);
        i += 3;
    }

    data[..length].copy_from_slice(&dst[..length]);
}

/// Audio filter: adaptive linear predictor for audio samples.
/// Matches libarchive's execute_filter_audio exactly.
fn execute_filter_audio(data: &mut [u8], channels: usize) {
    let length = data.len();
    if channels == 0 || length == 0 {
        return;
    }

    let src = data.to_vec();
    let mut dst = vec![0u8; length];
    let mut src_pos = 0usize;

    for ch in 0..channels {
        // Match libarchive's audio_state exactly:
        // int8_t weight[5]; int16_t delta[4]; int8_t lastdelta;
        // int error[11]; int count; uint8_t lastbyte;
        let mut weight: [i8; 5] = [0; 5];
        let mut delta: [i16; 4] = [0; 4];
        let mut lastdelta: i8 = 0;
        let mut error: [i32; 11] = [0; 11];
        let mut count: i32 = 0;
        let mut lastbyte: u8 = 0;

        let mut j = ch;
        while j < length {
            if src_pos >= src.len() {
                break;
            }
            let src_delta = src[src_pos] as i8;
            src_pos += 1;

            delta[2] = delta[1];
            delta[1] = (lastdelta as i32 - delta[0] as i32) as i16;
            delta[0] = lastdelta as i16;

            let predbyte = ((8i32 * lastbyte as i32
                + weight[0] as i32 * delta[0] as i32
                + weight[1] as i32 * delta[1] as i32
                + weight[2] as i32 * delta[2] as i32)
                >> 3) as u8;
            let byte = predbyte.wrapping_sub(src_delta as u8);

            let prederror = (src_delta as i32) << 3;
            error[0] += (prederror).abs();
            error[1] += (prederror - delta[0] as i32).abs();
            error[2] += (prederror + delta[0] as i32).abs();
            error[3] += (prederror - delta[1] as i32).abs();
            error[4] += (prederror + delta[1] as i32).abs();
            error[5] += (prederror - delta[2] as i32).abs();
            error[6] += (prederror + delta[2] as i32).abs();

            lastdelta = byte.wrapping_sub(lastbyte) as i8;
            dst[j] = byte;
            lastbyte = byte;

            // C: !(state.count++ & 0x1F) — post-increment, triggers at 0, 32, 64...
            if count & 0x1F == 0 {
                let mut idx: u8 = 0;
                for k in 1u8..7 {
                    if error[k as usize] < error[idx as usize] {
                        idx = k;
                    }
                }
                error = [0; 11];
                match idx {
                    1 => {
                        if weight[0] >= -16 {
                            weight[0] -= 1;
                        }
                    }
                    2 => {
                        if weight[0] < 16 {
                            weight[0] += 1;
                        }
                    }
                    3 => {
                        if weight[1] >= -16 {
                            weight[1] -= 1;
                        }
                    }
                    4 => {
                        if weight[1] < 16 {
                            weight[1] += 1;
                        }
                    }
                    5 => {
                        if weight[2] >= -16 {
                            weight[2] -= 1;
                        }
                    }
                    6 => {
                        if weight[2] < 16 {
                            weight[2] += 1;
                        }
                    }
                    _ => {}
                }
            }
            count += 1;

            j += channels;
        }
    }

    data[..length].copy_from_slice(&dst);
}

// ── Huffman Table Reading ─────────────────────────────────────────────────

/// Parse RAR4 Huffman codes from the bitstream.
fn parse_codes(reader: &mut BitReader, state: &mut Rar4DecoderState) -> Result<(), String> {
    // Align to byte boundary
    reader.align();

    // Check for PPMd
    let ppmd_flag = reader.read_bits(1).map_err(|e| e.to_string())?;
    if ppmd_flag != 0 {
        return Err("RAR4 PPMd compression not yet supported".into());
    }

    // Keep old table?
    let keep_old = reader.read_bits(1).map_err(|e| e.to_string())?;
    if keep_old == 0 {
        state.length_table = [0; HUFFMAN_TABLE_SIZE];
    }

    // Read precode (20 symbols, 4-bit lengths with escape)
    let mut bc_lengths = [0u8; RAR4_BC];
    let mut i = 0;
    while i < RAR4_BC {
        let val = reader.read_bits(4).map_err(|e| e.to_string())? as u8;
        if val == 0x0F {
            let zero_count = reader.read_bits(4).map_err(|e| e.to_string())? as usize;
            if zero_count == 0 {
                // Literal 15
                bc_lengths[i] = 15;
                i += 1;
            } else {
                // Fill zero_count + 2 zeros
                let fill = zero_count + 2;
                for _ in 0..fill {
                    if i < RAR4_BC {
                        bc_lengths[i] = 0;
                        i += 1;
                    }
                }
            }
        } else {
            bc_lengths[i] = val;
            i += 1;
        }
    }

    let bc_table = DecodeTable::new(&bc_lengths);

    // Decode the 404 code lengths using the precode
    let mut j = 0usize;
    while j < HUFFMAN_TABLE_SIZE {
        let sym = decode_symbol(&bc_table, reader).map_err(|e| e.to_string())?;

        if sym < 16 {
            // Additive mod 16
            state.length_table[j] = (state.length_table[j].wrapping_add(sym as u8)) & 0x0F;
            j += 1;
        } else if sym == 16 {
            let count = 3 + reader.read_bits(3).map_err(|e| e.to_string())? as usize;
            let prev = if j > 0 { state.length_table[j - 1] } else { 0 };
            for _ in 0..count {
                if j >= HUFFMAN_TABLE_SIZE {
                    break;
                }
                state.length_table[j] = prev;
                j += 1;
            }
        } else if sym == 17 {
            let count = 11 + reader.read_bits(7).map_err(|e| e.to_string())? as usize;
            let prev = if j > 0 { state.length_table[j - 1] } else { 0 };
            for _ in 0..count {
                if j >= HUFFMAN_TABLE_SIZE {
                    break;
                }
                state.length_table[j] = prev;
                j += 1;
            }
        } else if sym == 18 {
            let count = 3 + reader.read_bits(3).map_err(|e| e.to_string())? as usize;
            for _ in 0..count {
                if j >= HUFFMAN_TABLE_SIZE {
                    break;
                }
                state.length_table[j] = 0;
                j += 1;
            }
        } else if sym == 19 {
            let count = 11 + reader.read_bits(7).map_err(|e| e.to_string())? as usize;
            for _ in 0..count {
                if j >= HUFFMAN_TABLE_SIZE {
                    break;
                }
                state.length_table[j] = 0;
                j += 1;
            }
        }
    }

    let main_lengths = &state.length_table[..RAR4_NC];
    let offset_lengths = &state.length_table[RAR4_NC..RAR4_NC + RAR4_DC];
    let low_offset_lengths = &state.length_table[RAR4_NC + RAR4_DC..RAR4_NC + RAR4_DC + RAR4_LDC];
    let length_lengths = &state.length_table[RAR4_NC + RAR4_DC + RAR4_LDC..HUFFMAN_TABLE_SIZE];

    state.table_main = Some(DecodeTable::new(main_lengths));
    state.table_offset = Some(DecodeTable::new(offset_lengths));
    state.table_low_offset = Some(DecodeTable::new(low_offset_lengths));
    state.table_length = Some(DecodeTable::new(length_lengths));

    Ok(())
}
