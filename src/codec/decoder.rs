/// RAR5 block decoder — decompresses the RAR5 compressed bitstream.
///
/// Bitstream format derived from analysis of libarchive's
/// archive_read_support_format_rar5.c by Grzegorz Antoniak (2018),
/// an independent BSD-2-Clause licensed implementation.
use super::bitstream::BitReader;
use super::filters::apply_filter_decode;
use super::huffman::{DecodeTable, decode_symbol};
use super::tables::*;
use super::window::SlidingWindow;

// ── Pending Filter ─────────────────────────────────────────────────────────

struct PendingFilter {
    filter_type: u8,
    block_start: u64,
    block_length: u64,
    channels: u8,
}

// ── Decoder State (for solid archives) ─────────────────────────────────────

/// Persistent decoder state for solid archive support.
///
/// In a solid archive, the sliding window, distance cache, and Huffman
/// tables carry over between files.
pub struct DecoderState {
    pub window: SlidingWindow,
    pub dist_cache: [u32; DIST_CACHE_SIZE],
    pub last_length: u32,
    pub prev_low_dist: u32,
    pub table_nc: Option<DecodeTable>,
    pub table_dc: Option<DecodeTable>,
    pub table_ldc: Option<DecodeTable>,
    pub table_rc: Option<DecodeTable>,
}

impl DecoderState {
    pub fn new(dict_size: usize) -> Self {
        DecoderState {
            window: SlidingWindow::new(dict_size),
            dist_cache: [0; DIST_CACHE_SIZE],
            last_length: 0,
            prev_low_dist: 0,
            table_nc: None,
            table_dc: None,
            table_ldc: None,
            table_rc: None,
        }
    }
}

// ── Public decode function ─────────────────────────────────────────────────

/// Decode RAR5 compressed data.
///
/// - `data`: raw compressed bytes (the data area from the file block)
/// - `unpacked_size`: expected decompressed size in bytes
/// - `dict_size_log`: dictionary size as log2(size/128KB), 0 = 128KB
/// - `state`: optional DecoderState for solid archive continuity
pub fn decode(
    data: &[u8],
    unpacked_size: u64,
    dict_size_log: u8,
    state: Option<&mut DecoderState>,
) -> Result<Vec<u8>, String> {
    let mut reader = BitReader::new(data);

    match state {
        Some(st) => decode_inner(
            &mut reader,
            unpacked_size,
            &mut st.window,
            &mut st.dist_cache,
            &mut st.last_length,
            &mut st.prev_low_dist,
            &mut st.table_nc,
            &mut st.table_dc,
            &mut st.table_ldc,
            &mut st.table_rc,
        ),
        None => decode_standalone(data, unpacked_size, dict_size_log),
    }
}

/// Decode RAR5 compressed data (standalone, no solid state).
pub fn decode_standalone(
    data: &[u8],
    unpacked_size: u64,
    dict_size_log: u8,
) -> Result<Vec<u8>, String> {
    let dict_size_log = dict_size_log.max(0) as u32;
    let mut dict_size = 128 * 1024 * (1usize << dict_size_log);
    if !dict_size.is_power_of_two() {
        dict_size = dict_size.next_power_of_two();
    }
    // The decoder reconstructs the whole file in the sliding window before
    // extracting it (see `get_output`), so the window must be at least as
    // large as the unpacked output. The encoder caps its dictionary at 1 MiB
    // for compression performance, so grow the decode buffer here instead of
    // reverting that cap.
    if (unpacked_size as usize) > dict_size {
        dict_size = (unpacked_size as usize).next_power_of_two();
    }

    let mut reader = BitReader::new(data);
    let mut window = SlidingWindow::new(dict_size);
    let mut dist_cache = [0u32; DIST_CACHE_SIZE];
    let mut last_length = 0u32;
    let mut prev_low_dist = 0u32;
    let mut table_nc: Option<DecodeTable> = None;
    let mut table_dc: Option<DecodeTable> = None;
    let mut table_ldc: Option<DecodeTable> = None;
    let mut table_rc: Option<DecodeTable> = None;

    decode_inner(
        &mut reader,
        unpacked_size,
        &mut window,
        &mut dist_cache,
        &mut last_length,
        &mut prev_low_dist,
        &mut table_nc,
        &mut table_dc,
        &mut table_ldc,
        &mut table_rc,
    )
}

fn decode_inner(
    reader: &mut BitReader,
    unpacked_size: u64,
    window: &mut SlidingWindow,
    dist_cache: &mut [u32; DIST_CACHE_SIZE],
    last_length: &mut u32,
    prev_low_dist: &mut u32,
    table_nc: &mut Option<DecodeTable>,
    table_dc: &mut Option<DecodeTable>,
    table_ldc: &mut Option<DecodeTable>,
    table_rc: &mut Option<DecodeTable>,
) -> Result<Vec<u8>, String> {
    let mut pending_filters: Vec<PendingFilter> = Vec::new();
    let output_start = window.total_written();

    while (window.total_written() - output_start) < unpacked_size {
        // ── Read block header ──────────────────────────────────────────
        let block_flags_byte = reader.read_byte().map_err(|e| e.to_string())?;

        let table_present = (block_flags_byte >> 7) & 1 != 0;
        let is_last_block = (block_flags_byte >> 6) & 1 != 0;
        let byte_count = ((block_flags_byte >> 3) & 7) + 1;
        let bit_size = block_flags_byte & 7;

        let checksum_byte = reader.read_byte().map_err(|e| e.to_string())?;

        let block_size_bytes = reader
            .read_bytes(byte_count as usize)
            .map_err(|e| e.to_string())?;
        let mut block_size: u32 = 0;
        for (i, &b) in block_size_bytes.iter().enumerate() {
            block_size |= (b as u32) << (i * 8);
        }

        // Verify checksum
        let mut expected_ck = BLOCK_CHECKSUM_SEED ^ block_flags_byte;
        for &b in block_size_bytes {
            expected_ck ^= b;
        }
        if checksum_byte != expected_ck {
            return Err(format!(
                "block checksum mismatch: got {checksum_byte:#x}, expected {expected_ck:#x}"
            ));
        }

        let block_bits = ((block_size as u64) - 1) * 8 + (1 + bit_size as u64);
        let block_start_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;

        // ── Read Huffman tables if present ──────────────────────────────
        if table_present {
            let (nc, dc, ldc, rc) = read_tables(reader)?;
            *table_nc = Some(nc);
            *table_dc = Some(dc);
            *table_ldc = Some(ldc);
            *table_rc = Some(rc);
        }

        let t_nc = table_nc.as_ref().ok_or("no Huffman tables defined")?;
        let t_dc = table_dc.as_ref().ok_or("no Huffman tables defined")?;
        let t_ldc = table_ldc.as_ref().ok_or("no Huffman tables defined")?;
        let t_rc = table_rc.as_ref().ok_or("no Huffman tables defined")?;

        // ── Decode symbols ─────────────────────────────────────────────
        while (window.total_written() - output_start) < unpacked_size {
            let cur_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;
            if cur_bits - block_start_bits >= block_bits {
                break;
            }

            let sym = decode_symbol(t_nc, reader).map_err(|e| e.to_string())?;

            if sym < 256 {
                window.put_byte(sym as u8);
            } else if sym == SYM_FILTER {
                let filt = parse_filter(reader, window.total_written())?;
                pending_filters.push(filt);
            } else if sym == SYM_REPEAT {
                if *last_length > 0 && dist_cache[0] > 0 {
                    window.copy_match(dist_cache[0] as usize, *last_length as usize);
                }
            } else if sym >= SYM_CACHE_BASE && sym <= SYM_CACHE_BASE + 3 {
                let cache_idx = sym - SYM_CACHE_BASE;
                let dist = dist_cache_touch(dist_cache, cache_idx);
                let len_slot = decode_symbol(t_rc, reader).map_err(|e| e.to_string())?;
                let length = decode_length(len_slot, reader)?;
                *last_length = length;
                *prev_low_dist = dist & 0xF;
                window.copy_match(dist as usize, length as usize);
            } else if sym >= SYM_MATCH_BASE {
                let len_slot = sym - SYM_MATCH_BASE;
                let mut length = decode_length(len_slot, reader)?;
                let dist_slot = decode_symbol(t_dc, reader).map_err(|e| e.to_string())?;
                let dist = decode_distance(dist_slot, reader, t_ldc)?;
                length = apply_length_bonus(length, dist);
                *last_length = length;
                *prev_low_dist = dist & 0xF;
                dist_cache_push(dist_cache, dist);
                window.copy_match(dist as usize, length as usize);
            }
        }

        // Position reader at exact end of block
        let block_end_bits = block_start_bits + block_bits;
        reader.set_position((block_end_bits / 8) as usize, (block_end_bits % 8) as u8);

        if is_last_block {
            break;
        }
    }

    // Extract output
    let written = (window.total_written() - output_start).min(unpacked_size);
    let mut output = window.get_output(output_start, written as usize);

    // Apply pending filters
    for filt in &pending_filters {
        let start = (filt.block_start - output_start) as usize;
        let end = (start + filt.block_length as usize).min(output.len());
        if start >= output.len() {
            continue;
        }
        let region = &mut output[start..end];
        let filtered =
            apply_filter_decode(filt.filter_type, region, filt.channels, filt.block_start);
        output[start..start + filtered.len()].copy_from_slice(&filtered);
    }

    output.truncate(unpacked_size as usize);
    Ok(output)
}

// ── Huffman Table Reading ──────────────────────────────────────────────────

fn read_tables(
    reader: &mut BitReader,
) -> Result<(DecodeTable, DecodeTable, DecodeTable, DecodeTable), String> {
    // Read BC table: 20 code lengths as nibbles with escape mechanism
    let mut bc_lengths = Vec::with_capacity(HUFF_BC);
    while bc_lengths.len() < HUFF_BC {
        let val = reader.read_bits(4).map_err(|e| e.to_string())? as u8;
        if val == NIBBLE_ESCAPE {
            let next_val = reader.read_bits(4).map_err(|e| e.to_string())? as u8;
            if next_val == 0 {
                bc_lengths.push(15);
            } else {
                for _ in 0..(next_val as usize + 2) {
                    if bc_lengths.len() < HUFF_BC {
                        bc_lengths.push(0);
                    }
                }
            }
        } else {
            bc_lengths.push(val);
        }
    }

    let table_bc = DecodeTable::new(&bc_lengths);

    let total = HUFF_NC + HUFF_DC + HUFF_LDC + HUFF_RC;
    let all_lengths = read_code_lengths(reader, &table_bc, total)?;

    let nc_len = &all_lengths[..HUFF_NC];
    let dc_len = &all_lengths[HUFF_NC..HUFF_NC + HUFF_DC];
    let ldc_len = &all_lengths[HUFF_NC + HUFF_DC..HUFF_NC + HUFF_DC + HUFF_LDC];
    let rc_len = &all_lengths[HUFF_NC + HUFF_DC + HUFF_LDC..];

    Ok((
        DecodeTable::new(nc_len),
        DecodeTable::new(dc_len),
        DecodeTable::new(ldc_len),
        DecodeTable::new(rc_len),
    ))
}

fn read_code_lengths(
    reader: &mut BitReader,
    bc_table: &DecodeTable,
    count: usize,
) -> Result<Vec<u8>, String> {
    let mut lengths = vec![0u8; count];
    let mut i = 0;
    while i < count {
        let sym = decode_symbol(bc_table, reader).map_err(|e| e.to_string())?;
        if sym < 16 {
            lengths[i] = sym as u8;
            i += 1;
        } else if sym < 18 {
            if i == 0 {
                return Err("run-length repeat with no previous length".into());
            }
            let repeat = if sym == 16 {
                3 + reader.read_bits(3).map_err(|e| e.to_string())? as usize
            } else {
                11 + reader.read_bits(7).map_err(|e| e.to_string())? as usize
            };
            let prev = lengths[i - 1];
            for _ in 0..repeat {
                if i >= count {
                    break;
                }
                lengths[i] = prev;
                i += 1;
            }
        } else {
            let repeat = if sym == 18 {
                3 + reader.read_bits(3).map_err(|e| e.to_string())? as usize
            } else {
                11 + reader.read_bits(7).map_err(|e| e.to_string())? as usize
            };
            for _ in 0..repeat {
                if i >= count {
                    break;
                }
                lengths[i] = 0;
                i += 1;
            }
        }
    }
    Ok(lengths)
}

// ── Length/Distance Decoding ───────────────────────────────────────────────

fn decode_length(slot: usize, reader: &mut BitReader) -> Result<u32, String> {
    if slot < 8 {
        Ok(2 + slot as u32)
    } else {
        let lbits = (slot / 4 - 1) as u8;
        let base = 2 + ((4 | (slot & 3)) << lbits) as u32;
        if lbits > 0 {
            let extra = reader.read_bits(lbits).map_err(|e| e.to_string())?;
            Ok(base + extra)
        } else {
            Ok(base)
        }
    }
}

fn decode_distance(
    dist_slot: usize,
    reader: &mut BitReader,
    table_ldc: &DecodeTable,
) -> Result<u32, String> {
    if dist_slot < 4 {
        Ok(1 + dist_slot as u32)
    } else {
        let dbits = (dist_slot / 2 - 1) as u8;
        let mut dist = 1 + ((2 | (dist_slot & 1)) << dbits) as u32;

        if dbits > 0 {
            if dbits >= 4 {
                if dbits > 4 {
                    let upper = reader.read_bits(dbits - 4).map_err(|e| e.to_string())?;
                    dist += upper << 4;
                }
                let low_dist = decode_symbol(table_ldc, reader).map_err(|e| e.to_string())?;
                dist += low_dist as u32;
            } else {
                let extra = reader.read_bits(dbits).map_err(|e| e.to_string())?;
                dist += extra;
            }
        }
        Ok(dist)
    }
}

// ── Distance Cache ─────────────────────────────────────────────────────────

fn dist_cache_push(cache: &mut [u32; DIST_CACHE_SIZE], value: u32) {
    cache[3] = cache[2];
    cache[2] = cache[1];
    cache[1] = cache[0];
    cache[0] = value;
}

fn dist_cache_touch(cache: &mut [u32; DIST_CACHE_SIZE], idx: usize) -> u32 {
    let value = cache[idx];
    for i in (1..=idx).rev() {
        cache[i] = cache[i - 1];
    }
    cache[0] = value;
    value
}

// ── Length Bonus ────────────────────────────────────────────────────────────

fn apply_length_bonus(length: u32, dist: u32) -> u32 {
    let mut l = length;
    if dist > 0x100 {
        l += 1;
    }
    if dist > 0x2000 {
        l += 1;
    }
    if dist > 0x40000 {
        l += 1;
    }
    l
}

// ── Filter Parsing ─────────────────────────────────────────────────────────

fn parse_filter(reader: &mut BitReader, write_pos: u64) -> Result<PendingFilter, String> {
    let block_start = write_pos + parse_filter_data(reader)? as u64;
    let block_length = parse_filter_data(reader)? as u64;
    let filter_type = reader.read_bits(3).map_err(|e| e.to_string())? as u8;

    let channels = if filter_type == FILTER_DELTA {
        reader.read_bits(5).map_err(|e| e.to_string())? as u8 + 1
    } else {
        0
    };

    Ok(PendingFilter {
        filter_type,
        block_start,
        block_length,
        channels,
    })
}

fn parse_filter_data(reader: &mut BitReader) -> Result<u32, String> {
    let byte_count = reader.read_bits(2).map_err(|e| e.to_string())? + 1;
    let mut value: u32 = 0;
    for i in 0..byte_count {
        let b = reader.read_bits(8).map_err(|e| e.to_string())?;
        value |= b << (i * 8);
    }
    Ok(value)
}
