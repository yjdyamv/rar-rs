//! Bit-level decode primitives.
//!
//! Huffman table reading (nibble RLE + BC/LDC/DELTA layouts), length and
//! distance decoding against the RAR5/RAR7 code tables, the distance cache
//! and the length bonus, plus filter-record parsing.

use super::*;

use super::super::{
    FILTER_DELTA, HUFF_BC, HUFF_DC, HUFF_DCX, HUFF_LDC, HUFF_NC, HUFF_RC, NIBBLE_ESCAPE,
};
use crate::codec::common::bitstream::BitReader;
use crate::codec::common::huffman::{DecodeTable, decode_symbol};
use crate::error::{RarError, RarResult};
use crate::version::ArchiveVersion;
// ── Huffman Table Reading ──────────────────────────────────────────────────

pub(super) fn read_tables(
    reader: &mut BitReader,
    variant: ArchiveVersion,
) -> RarResult<(DecodeTable, DecodeTable, DecodeTable, DecodeTable)> {
    // RAR7 (v70) extends the distance code table from 64 to 80 codes.
    let dc_count = if variant.uses_extra_dist() {
        HUFF_DCX
    } else {
        HUFF_DC
    };
    // Read BC table: 20 code lengths as nibbles with escape mechanism
    let mut bc_lengths = Vec::with_capacity(HUFF_BC);
    while bc_lengths.len() < HUFF_BC {
        let val = reader
            .read_bits(4)
            .map_err(|e| RarError::Format(e.to_string()))? as u8;
        if val == NIBBLE_ESCAPE {
            let next_val = reader
                .read_bits(4)
                .map_err(|e| RarError::Format(e.to_string()))? as u8;
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

    let total = HUFF_NC + dc_count + HUFF_LDC + HUFF_RC;
    let all_lengths = read_code_lengths(reader, &table_bc, total)?;

    let nc_len = &all_lengths[..HUFF_NC];
    let dc_len = &all_lengths[HUFF_NC..HUFF_NC + dc_count];
    let ldc_len = &all_lengths[HUFF_NC + dc_count..HUFF_NC + dc_count + HUFF_LDC];
    let rc_len = &all_lengths[HUFF_NC + dc_count + HUFF_LDC..];

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
) -> RarResult<Vec<u8>> {
    let mut lengths = vec![0u8; count];
    let mut i = 0;
    while i < count {
        let sym = decode_symbol(bc_table, reader).map_err(|e| RarError::Format(e.to_string()))?;
        if sym < 16 {
            lengths[i] = sym as u8;
            i += 1;
        } else if sym < 18 {
            if i == 0 {
                return Err(RarError::Format(
                    "run-length repeat with no previous length".into(),
                ));
            }
            let repeat = if sym == 16 {
                3 + reader
                    .read_bits(3)
                    .map_err(|e| RarError::Format(e.to_string()))? as usize
            } else {
                11 + reader
                    .read_bits(7)
                    .map_err(|e| RarError::Format(e.to_string()))? as usize
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
                3 + reader
                    .read_bits(3)
                    .map_err(|e| RarError::Format(e.to_string()))? as usize
            } else {
                11 + reader
                    .read_bits(7)
                    .map_err(|e| RarError::Format(e.to_string()))? as usize
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

pub(super) fn decode_length(slot: usize, reader: &mut BitReader) -> RarResult<u32> {
    if slot < 8 {
        Ok(2 + slot as u32)
    } else {
        let lbits = (slot / 4 - 1) as u8;
        let base = 2 + ((4 | (slot & 3)) << lbits) as u32;
        if lbits > 0 {
            let extra = reader
                .read_bits(lbits)
                .map_err(|e| RarError::Format(e.to_string()))?;
            Ok(base + extra)
        } else {
            Ok(base)
        }
    }
}

pub(super) fn decode_distance(
    dist_slot: usize,
    reader: &mut BitReader,
    table_ldc: &DecodeTable,
) -> RarResult<u64> {
    // RAR7's extended table (80 codes) reaches dist slots up to 79, i.e.
    // DBits up to 38 and distances beyond 4 GB — hence u64 arithmetic.
    if dist_slot < 4 {
        Ok(1 + dist_slot as u64)
    } else {
        let dbits = (dist_slot / 2 - 1) as u8;
        let mut dist = 1u64 + (((2 | (dist_slot & 1)) as u64) << dbits);

        if dbits > 0 {
            if dbits >= 4 {
                if dbits > 4 {
                    let upper = reader
                        .read_bits(dbits - 4)
                        .map_err(|e| RarError::Format(e.to_string()))?;
                    dist = dist.wrapping_add((upper as u64) << 4);
                }
                let low_dist = decode_symbol(table_ldc, reader)
                    .map_err(|e| RarError::Format(e.to_string()))?;
                dist = dist.wrapping_add(low_dist as u64);
            } else {
                let extra = reader
                    .read_bits(dbits)
                    .map_err(|e| RarError::Format(e.to_string()))?;
                dist = dist.wrapping_add(extra as u64);
            }
        }
        Ok(dist)
    }
}

// ── Distance Cache ─────────────────────────────────────────────────────────

pub(super) fn dist_cache_push(cache: &mut [u64; DIST_CACHE_SIZE], value: u64) {
    cache[3] = cache[2];
    cache[2] = cache[1];
    cache[1] = cache[0];
    cache[0] = value;
}

pub(super) fn dist_cache_touch(cache: &mut [u64; DIST_CACHE_SIZE], idx: usize) -> u64 {
    let value = cache[idx];
    for i in (1..=idx).rev() {
        cache[i] = cache[i - 1];
    }
    cache[0] = value;
    value
}

// ── Length Bonus ────────────────────────────────────────────────────────────

pub(super) fn apply_length_bonus_u64(length: u32, dist: u64) -> u32 {
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

pub(super) fn parse_filter(reader: &mut BitReader, write_pos: u64) -> RarResult<PendingFilter> {
    let block_start = write_pos + parse_filter_data(reader)? as u64;
    let block_length = parse_filter_data(reader)? as u64;
    let filter_type = reader
        .read_bits(3)
        .map_err(|e| RarError::Format(e.to_string()))? as u8;

    let channels = if filter_type == FILTER_DELTA {
        reader
            .read_bits(5)
            .map_err(|e| RarError::Format(e.to_string()))? as u8
            + 1
    } else {
        0
    };

    Ok(PendingFilter {
        filter_type,
        block_start,
        block_length,
        channels,
        applied: false,
    })
}

fn parse_filter_data(reader: &mut BitReader) -> RarResult<u32> {
    let byte_count = reader
        .read_bits(2)
        .map_err(|e| RarError::Format(e.to_string()))?
        + 1;
    let mut value: u32 = 0;
    for i in 0..byte_count {
        let b = reader
            .read_bits(8)
            .map_err(|e| RarError::Format(e.to_string()))?;
        value |= b << (i * 8);
    }
    Ok(value)
}
