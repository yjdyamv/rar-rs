//! Block serialisation.
//!
//! Turns one emitted block of [`Symbol`]s into its RAR5 byte image: block
//! header (checksum over the header bytes), Huffman code lengths (nibble RLE,
//! then BC/LDC/DELTA tables), the symbol stream itself and the end-of-stream
//! marker. Distance slots select the RAR5 (64) or RAR7/v70 (80) table via
//! [`HUFF_DC`](super::super::HUFF_DC) / [`HUFF_DCX`](super::super::HUFF_DCX).

use super::*;

use super::super::{
    BLOCK_CHECKSUM_SEED, FILTER_DELTA, HUFF_BC, HUFF_DC, HUFF_DCX, HUFF_LDC, HUFF_NC, HUFF_RC,
    MAX_CODE_LENGTH, NIBBLE_ESCAPE, SYM_CACHE_BASE, SYM_FILTER, SYM_MATCH_BASE, SYM_REPEAT,
};
use crate::codec::common::bitstream::BitWriter;
use crate::codec::common::huffman::{EncodeTable, build_code_lengths_from_freqs, encode_symbol};
use crate::version::ArchiveVersion;

// ── Block encoding ─────────────────────────────────────────────────────────

pub(super) fn encode_block(symbols: &[Symbol], is_last: bool, variant: ArchiveVersion) -> Vec<u8> {
    // Collect frequencies
    let dc_count = if variant.uses_extra_dist() {
        HUFF_DCX
    } else {
        HUFF_DC
    };
    let mut nc_freq = vec![0u32; HUFF_NC];
    let mut dc_freq = vec![0u32; dc_count];
    let mut ldc_freq = vec![0u32; HUFF_LDC];
    let mut rc_freq = vec![0u32; HUFF_RC];

    for sym in symbols {
        match sym {
            Symbol::Literal(b) => nc_freq[*b as usize] += 1,
            Symbol::Match { distance, length } => {
                let len_slot = encode_length_slot(*length);
                nc_freq[SYM_MATCH_BASE + len_slot] += 1;
                let (dist_slot, _, _) = encode_distance_slot(*distance, variant);
                dc_freq[dist_slot] += 1;
                if dist_slot >= 4 {
                    let dbits = dist_slot / 2 - 1;
                    if dbits >= 4 {
                        let base = (2 | (dist_slot & 1)) << dbits;
                        let low_dist = ((*distance as usize) - 1 - base) & 0xF;
                        ldc_freq[low_dist] += 1;
                    }
                }
            }
            Symbol::CacheRef { index: _, length } => {
                nc_freq[SYM_CACHE_BASE] += 1; // simplified — actual index varies
                let len_slot = encode_length_slot(*length);
                rc_freq[len_slot] += 1;
            }
            Symbol::Repeat => nc_freq[SYM_REPEAT] += 1,
            Symbol::Filter { .. } => nc_freq[SYM_FILTER] += 1,
        }
    }

    // Re-count cache refs with actual indices
    // (The above was a simplification; let's do it properly)
    nc_freq[SYM_CACHE_BASE] = 0;
    for sym in symbols {
        if let Symbol::CacheRef { index, .. } = sym {
            nc_freq[SYM_CACHE_BASE + index] += 1;
        }
    }
    // Subtract the earlier simplified count was only for SYM_CACHE_BASE,
    // but we zeroed it — the per-index counts are correct now.

    ensure_nonzero(&mut nc_freq);
    ensure_nonzero(&mut dc_freq);
    ensure_nonzero(&mut ldc_freq);
    ensure_nonzero(&mut rc_freq);

    let nc_lengths = build_code_lengths_from_freqs(&nc_freq, MAX_CODE_LENGTH);
    let dc_lengths = build_code_lengths_from_freqs(&dc_freq, MAX_CODE_LENGTH);
    let ldc_lengths = build_code_lengths_from_freqs(&ldc_freq, MAX_CODE_LENGTH);
    let rc_lengths = build_code_lengths_from_freqs(&rc_freq, MAX_CODE_LENGTH);

    let enc_nc = EncodeTable::new(&nc_lengths);
    let enc_dc = EncodeTable::new(&dc_lengths);
    let enc_ldc = EncodeTable::new(&ldc_lengths);
    let enc_rc = EncodeTable::new(&rc_lengths);

    let mut writer = BitWriter::new();

    write_tables(
        &mut writer,
        &nc_lengths,
        &dc_lengths,
        &ldc_lengths,
        &rc_lengths,
    );

    for sym in symbols {
        match sym {
            Symbol::Literal(b) => encode_symbol(&enc_nc, &mut writer, *b as usize),
            Symbol::Match { distance, length } => {
                write_match(
                    &mut writer,
                    &enc_nc,
                    &enc_dc,
                    &enc_ldc,
                    *distance,
                    *length,
                    variant,
                );
            }
            Symbol::CacheRef { index, length } => {
                write_cache_ref(&mut writer, &enc_nc, &enc_rc, *index, *length);
            }
            Symbol::Repeat => encode_symbol(&enc_nc, &mut writer, SYM_REPEAT),
            Symbol::Filter {
                block_start,
                block_length,
                filter_type,
                channels,
            } => {
                encode_symbol(&enc_nc, &mut writer, SYM_FILTER);
                write_filter_data(&mut writer, *block_start);
                write_filter_data(&mut writer, *block_length);
                writer.write_bits(*filter_type as u32, 3);
                if *filter_type == FILTER_DELTA {
                    writer.write_bits(channels.saturating_sub(1) as u32, 5);
                }
            }
        }
    }

    let total_bits = writer.bit_count();
    let block_data = writer.into_bytes();

    let (total_bits, block_data) = if total_bits == 0 {
        (8, vec![0u8])
    } else {
        (total_bits, block_data)
    };

    build_block_header(&block_data, total_bits, is_last, true)
}

pub(super) fn encode_empty_block(variant: ArchiveVersion) -> Vec<u8> {
    let mut writer = BitWriter::new();
    let nc_lengths = {
        let mut v = vec![0u8; HUFF_NC];
        v[0] = 1;
        v
    };
    let dc_lengths = {
        let mut v = vec![
            0u8;
            if variant.uses_extra_dist() {
                HUFF_DCX
            } else {
                HUFF_DC
            }
        ];
        v[0] = 1;
        v
    };
    let ldc_lengths = {
        let mut v = vec![0u8; HUFF_LDC];
        v[0] = 1;
        v
    };
    let rc_lengths = {
        let mut v = vec![0u8; HUFF_RC];
        v[0] = 1;
        v
    };

    write_tables(
        &mut writer,
        &nc_lengths,
        &dc_lengths,
        &ldc_lengths,
        &rc_lengths,
    );

    let total_bits = writer.bit_count();
    let block_data = writer.into_bytes();
    let (total_bits, block_data) = if total_bits == 0 {
        (8, vec![0u8])
    } else {
        (total_bits, block_data)
    };

    build_block_header(&block_data, total_bits, true, true)
}

pub(super) fn build_block_header(
    block_data: &[u8],
    total_bits: usize,
    is_last: bool,
    table_present: bool,
) -> Vec<u8> {
    let block_size = block_data.len();
    let valid_last_bits = total_bits - (block_size - 1) * 8;
    let bit_size = if valid_last_bits > 0 {
        (valid_last_bits - 1) as u8
    } else {
        7
    };

    let byte_count: u8 = if block_size <= 0xFF {
        1
    } else if block_size <= 0xFFFF {
        2
    } else {
        3
    };

    let mut flags: u8 = 0;
    if table_present {
        flags |= 1 << 7;
    }
    if is_last {
        flags |= 1 << 6;
    }
    flags |= (byte_count - 1) << 3;
    flags |= bit_size & 7;

    let mut size_bytes = vec![0u8; byte_count as usize];
    for (i, byte) in size_bytes.iter_mut().enumerate() {
        *byte = ((block_size >> (i * 8)) & 0xFF) as u8;
    }

    let mut checksum = BLOCK_CHECKSUM_SEED ^ flags;
    for &b in &size_bytes {
        checksum ^= b;
    }

    let mut header = Vec::with_capacity(2 + size_bytes.len() + block_data.len());
    header.push(flags);
    header.push(checksum);
    header.extend(&size_bytes);
    header.extend(block_data);
    header
}

// ── Huffman table writing ──────────────────────────────────────────────────

pub(super) fn write_tables(
    writer: &mut BitWriter,
    nc_lengths: &[u8],
    dc_lengths: &[u8],
    ldc_lengths: &[u8],
    rc_lengths: &[u8],
) {
    let mut all_lengths = Vec::with_capacity(HUFF_NC + HUFF_DC + HUFF_LDC + HUFF_RC);
    all_lengths.extend_from_slice(nc_lengths);
    all_lengths.extend_from_slice(dc_lengths);
    all_lengths.extend_from_slice(ldc_lengths);
    all_lengths.extend_from_slice(rc_lengths);

    let rle_symbols = rle_encode_lengths(&all_lengths);

    let mut bc_freq = vec![0u32; HUFF_BC];
    for item in &rle_symbols {
        bc_freq[item.0 as usize] += 1;
    }
    ensure_nonzero(&mut bc_freq);

    let bc_lengths = build_code_lengths_from_freqs(&bc_freq, MAX_CODE_LENGTH);
    write_bc_nibbles(writer, &bc_lengths);

    let enc_bc = EncodeTable::new(&bc_lengths);
    for item in &rle_symbols {
        encode_symbol(&enc_bc, writer, item.0 as usize);
        match item.0 {
            16 => writer.write_bits(item.1 as u32 - 3, 3),
            17 => writer.write_bits(item.1 as u32 - 11, 7),
            18 => writer.write_bits(item.1 as u32 - 3, 3),
            19 => writer.write_bits(item.1 as u32 - 11, 7),
            _ => {}
        }
    }
}

fn write_bc_nibbles(writer: &mut BitWriter, bc_lengths: &[u8]) {
    let mut i = 0;
    while i < HUFF_BC {
        let val = bc_lengths[i];
        if val == 0 {
            let mut run = 0;
            while i + run < HUFF_BC && bc_lengths[i + run] == 0 {
                run += 1;
            }
            while run > 0 {
                if run >= 3 {
                    let count = run.min(16);
                    writer.write_bits(NIBBLE_ESCAPE as u32, 4);
                    writer.write_bits((count - 2) as u32, 4);
                    run -= count;
                    i += count;
                } else {
                    writer.write_bits(0, 4);
                    run -= 1;
                    i += 1;
                }
            }
        } else if val == 15 {
            writer.write_bits(NIBBLE_ESCAPE as u32, 4);
            writer.write_bits(0, 4);
            i += 1;
        } else {
            writer.write_bits(val as u32, 4);
            i += 1;
        }
    }
}

/// RLE item: (symbol, repeat_count). For symbols 0-15, count is 0.
struct RleItem(u8, usize);

fn rle_encode_lengths(lengths: &[u8]) -> Vec<RleItem> {
    let mut result = Vec::new();
    let n = lengths.len();
    let mut i = 0;

    while i < n {
        let val = lengths[i];
        if val == 0 {
            let mut run = 0;
            while i + run < n && lengths[i + run] == 0 {
                run += 1;
            }
            while run > 0 {
                if run >= 11 {
                    let count = run.min(138);
                    result.push(RleItem(19, count));
                    run -= count;
                    i += count;
                } else if run >= 3 {
                    let count = run.min(10);
                    result.push(RleItem(18, count));
                    run -= count;
                    i += count;
                } else {
                    result.push(RleItem(0, 0));
                    run -= 1;
                    i += 1;
                }
            }
        } else {
            result.push(RleItem(val, 0));
            i += 1;
            let mut run = 0;
            while i + run < n && lengths[i + run] == val {
                run += 1;
            }
            while run > 0 {
                if run >= 11 {
                    let count = run.min(138);
                    result.push(RleItem(17, count));
                    run -= count;
                    i += count;
                } else if run >= 3 {
                    let count = run.min(10);
                    result.push(RleItem(16, count));
                    run -= count;
                    i += count;
                } else {
                    break;
                }
            }
        }
    }

    result
}

// ── Match/distance encoding helpers ────────────────────────────────────────

fn write_match(
    writer: &mut BitWriter,
    enc_nc: &EncodeTable,
    enc_dc: &EncodeTable,
    enc_ldc: &EncodeTable,
    dist: u32,
    length: u32,
    variant: ArchiveVersion,
) {
    let len_slot = encode_length_slot(length);
    encode_symbol(enc_nc, writer, SYM_MATCH_BASE + len_slot);
    write_length_extra(writer, length, len_slot);

    let (dist_slot, extra, dbits) = encode_distance_slot(dist, variant);
    encode_symbol(enc_dc, writer, dist_slot);
    write_distance_extra(writer, enc_ldc, dist_slot, extra, dbits);
}

fn write_cache_ref(
    writer: &mut BitWriter,
    enc_nc: &EncodeTable,
    enc_rc: &EncodeTable,
    cache_idx: usize,
    length: u32,
) {
    encode_symbol(enc_nc, writer, SYM_CACHE_BASE + cache_idx);
    let len_slot = encode_length_slot(length);
    encode_symbol(enc_rc, writer, len_slot);
    write_length_extra(writer, length, len_slot);
}

pub(super) fn encode_length_slot(length: u32) -> usize {
    if length < 2 {
        return 0;
    }
    if length <= 9 {
        return (length - 2) as usize;
    }
    let val = length - 2;
    let high_bit = 32 - val.leading_zeros() - 1;
    if high_bit < 2 {
        return (length - 2) as usize;
    }
    let lbits = high_bit - 2;
    let sub = (val >> lbits) & 3;
    let slot = 4 * (lbits + 1) + sub;
    slot.min(HUFF_RC as u32 - 1) as usize
}

fn write_length_extra(writer: &mut BitWriter, length: u32, slot: usize) {
    if slot >= 8 {
        let lbits = (slot / 4 - 1) as u8;
        let base = 2 + ((4 | (slot & 3)) << lbits) as u32;
        let extra = length - base;
        if lbits > 0 {
            writer.write_bits(extra, lbits);
        }
    }
}

/// Map a match distance to its distance-code slot.
///
/// RAR5 caps the table at 64 entries (`HUFF_DC`), RAR7 (v70) at 80
/// (`HUFF_DCX`), which covers distances up to 1 TiB. `dbits >= 4` slots
/// split their extra bits between a raw prefix and the low nibble encoded
/// through the LDC table.
pub(super) fn encode_distance_slot(dist: u32, variant: ArchiveVersion) -> (usize, u32, usize) {
    if dist <= 4 {
        return ((dist - 1) as usize, 0, 0);
    }
    let val = dist - 1;
    let high_bit = (32 - val.leading_zeros() - 1) as usize;
    if high_bit < 1 {
        return ((dist - 1) as usize, 0, 0);
    }
    let dbits = high_bit - 1;
    let sub = (val >> dbits) & 1;
    let slot = 2 * (dbits + 1) + sub as usize;
    let base = (2 | sub) << dbits;
    let extra = val - base;
    let max_slot = if variant.uses_extra_dist() {
        HUFF_DCX - 1
    } else {
        HUFF_DC - 1
    };
    (slot.min(max_slot), extra, dbits)
}

fn write_distance_extra(
    writer: &mut BitWriter,
    enc_ldc: &EncodeTable,
    dist_slot: usize,
    extra: u32,
    dbits: usize,
) {
    if dist_slot >= 4 && dbits > 0 {
        if dbits >= 4 {
            if dbits > 4 {
                let upper = extra >> 4;
                writer.write_bits(upper, (dbits - 4) as u8);
            }
            let low = (extra & 0xF) as usize;
            encode_symbol(enc_ldc, writer, low);
        } else {
            writer.write_bits(extra, dbits as u8);
        }
    }
}

// ── Cache helpers ──────────────────────────────────────────────────────────

pub(super) fn cache_find(cache: &[u32; DIST_CACHE_SIZE], dist: u32) -> Option<usize> {
    cache.iter().position(|&d| d == dist)
}

pub(super) fn cache_push(cache: &mut [u32; DIST_CACHE_SIZE], dist: u32) {
    cache[3] = cache[2];
    cache[2] = cache[1];
    cache[1] = cache[0];
    cache[0] = dist;
}

pub(super) fn cache_touch(cache: &mut [u32; DIST_CACHE_SIZE], idx: usize) {
    let val = cache[idx];
    for i in (1..=idx).rev() {
        cache[i] = cache[i - 1];
    }
    cache[0] = val;
}

pub(super) fn apply_length_bonus(length: u32, dist: u32) -> u32 {
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

pub(super) fn remove_length_bonus(length: u32, dist: u32) -> u32 {
    let mut l = length;
    if dist > 0x100 {
        l = l.saturating_sub(1);
    }
    if dist > 0x2000 {
        l = l.saturating_sub(1);
    }
    if dist > 0x40000 {
        l = l.saturating_sub(1);
    }
    l
}

pub(super) fn ensure_nonzero(freq: &mut [u32]) {
    let nonzero = freq.iter().filter(|&&f| f > 0).count();
    if nonzero < 2 {
        let mut added = 0;
        for f in freq.iter_mut() {
            if *f == 0 {
                *f = 1;
                added += 1;
                if nonzero + added >= 2 {
                    break;
                }
            }
        }
    }
}

/// Emit RAR5 filter data: `[2 bits byte_count-1][LE bytes]`.
pub(super) fn write_filter_data(writer: &mut BitWriter, value: u32) {
    let bytes = value.to_le_bytes();
    let count = if value <= 0xFF {
        1
    } else if value <= 0xFFFF {
        2
    } else if value <= 0xFF_FFFF {
        3
    } else {
        4
    };
    writer.write_bits((count - 1) as u32, 2);
    for &b in &bytes[..count] {
        writer.write_bits(b as u32, 8);
    }
}
