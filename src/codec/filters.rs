/// RAR5 output filters: Delta, E8, E8E9, ARM.
///
/// Post-processing filters applied to regions of decompressed output.
/// Each filter has decode (inverse) and encode (forward) functions.
use super::tables::*;

/// Apply the inverse filter (for decompression).
pub fn apply_filter_decode(
    filter_type: u8,
    data: &mut [u8],
    channels: u8,
    file_offset: u64,
) -> Vec<u8> {
    match filter_type {
        FILTER_DELTA => delta_decode(data, channels),
        FILTER_E8 => e8_decode(data, file_offset, true),
        FILTER_E8E9 => e8_decode(data, file_offset, false),
        FILTER_ARM => arm_decode(data, file_offset),
        _ => data.to_vec(),
    }
}

/// Apply the forward filter (for compression).
pub fn apply_filter_encode(
    filter_type: u8,
    data: &mut [u8],
    channels: u8,
    file_offset: u64,
) -> Vec<u8> {
    match filter_type {
        FILTER_DELTA => delta_encode(data, channels),
        FILTER_E8 => e8_encode(data, file_offset, true),
        FILTER_E8E9 => e8_encode(data, file_offset, false),
        FILTER_ARM => arm_encode(data, file_offset),
        _ => data.to_vec(),
    }
}

// ── Delta Filter ───────────────────────────────────────────────────────────

fn delta_decode(data: &[u8], channels: u8) -> Vec<u8> {
    if channels < 1 {
        return data.to_vec();
    }
    let n = data.len();
    let ch = channels as usize;
    let mut result = vec![0u8; n];
    let mut src = 0;
    for c in 0..ch {
        let mut prev: u8 = 0;
        let mut i = c;
        while i < n {
            prev = prev.wrapping_sub(data[src]);
            result[i] = prev;
            src += 1;
            i += ch;
        }
    }
    result
}

fn delta_encode(data: &[u8], channels: u8) -> Vec<u8> {
    if channels < 1 {
        return data.to_vec();
    }
    let n = data.len();
    let ch = channels as usize;
    let mut result = vec![0u8; n];
    let mut dst = 0;
    for c in 0..ch {
        let mut prev: u8 = 0;
        let mut i = c;
        while i < n {
            let val = data[i];
            result[dst] = prev.wrapping_sub(val);
            prev = val;
            dst += 1;
            i += ch;
        }
    }
    result
}

// ── x86 E8/E8E9 Filter ────────────────────────────────────────────────────
//
// RAR5 uses a conditional address normalisation scheme with a virtual
// file_size of 0x1000000 (16 MB). During compression the encoder converts
// relative CALL/JMP targets to position-independent canonical form; the
// decoder reverses the transform.
//
// The transform formulas follow the WinRAR-interop-verified `rars` project
// (https://github.com/bitplane/rars, MIT OR Apache-2.0) `codec/filters.rs`:
// encode keeps `addr + offset` when it stays below the 16 MB model and
// otherwise folds negative wraparound targets; decode is the exact inverse.

fn e8_decode(data: &mut [u8], file_offset: u64, e8_only: bool) -> Vec<u8> {
    let n = data.len();
    if n < 5 {
        return data.to_vec();
    }
    let cmp_mask = if e8_only { 0xFF } else { 0xFE };
    let opcode_limit = n - 4;
    let mut i = 0usize;
    while i < opcode_limit {
        let opcode = data[i];
        if opcode & cmp_mask == 0xE8 {
            let cur_pos = i + 1;
            let offset = file_offset.wrapping_add(cur_pos as u64) as u32;
            let addr = u32::from_le_bytes(data[cur_pos..cur_pos + 4].try_into().unwrap());

            let new_addr = if addr < 0x0100_0000 {
                Some(addr.wrapping_sub(offset))
            } else if addr & 0x8000_0000 != 0
                && addr.wrapping_add(offset) & 0x8000_0000 == 0
            {
                Some(addr.wrapping_add(0x0100_0000))
            } else {
                None
            };
            if let Some(value) = new_addr {
                data[cur_pos..cur_pos + 4].copy_from_slice(&value.to_le_bytes());
            }
            i = cur_pos + 4;
        } else {
            i += 1;
        }
    }
    data.to_vec()
}

fn e8_encode(data: &mut [u8], file_offset: u64, e8_only: bool) -> Vec<u8> {
    let n = data.len();
    if n < 5 {
        return data.to_vec();
    }
    let cmp_mask = if e8_only { 0xFF } else { 0xFE };
    let opcode_limit = n - 4;
    let mut i = 0usize;
    while i < opcode_limit {
        let opcode = data[i];
        if opcode & cmp_mask == 0xE8 {
            let cur_pos = i + 1;
            let offset = file_offset.wrapping_add(cur_pos as u64) as u32;
            let addr = u32::from_le_bytes(data[cur_pos..cur_pos + 4].try_into().unwrap());

            let candidate = addr.wrapping_add(offset);
            if candidate < 0x0100_0000 {
                data[cur_pos..cur_pos + 4].copy_from_slice(&candidate.to_le_bytes());
            } else {
                let candidate = addr.wrapping_sub(0x0100_0000);
                if candidate & 0x8000_0000 != 0
                    && candidate.wrapping_add(offset) & 0x8000_0000 == 0
                {
                    data[cur_pos..cur_pos + 4].copy_from_slice(&candidate.to_le_bytes());
                }
            }
            i = cur_pos + 4;
        } else {
            i += 1;
        }
    }
    data.to_vec()
}

// ── ARM Filter ─────────────────────────────────────────────────────────────

fn arm_decode(data: &mut [u8], file_offset: u64) -> Vec<u8> {
    let n = data.len();
    if n < 4 {
        return data.to_vec();
    }
    let mut i = 0;
    while i + 3 < n {
        if data[i + 3] == 0xEB {
            let offset =
                (data[i] as u32) | ((data[i + 1] as u32) << 8) | ((data[i + 2] as u32) << 16);
            let adj = offset.wrapping_sub(((file_offset as u32).wrapping_add(i as u32)) >> 2);
            let masked = adj & 0xFF_FFFF;
            data[i] = (masked & 0xFF) as u8;
            data[i + 1] = ((masked >> 8) & 0xFF) as u8;
            data[i + 2] = ((masked >> 16) & 0xFF) as u8;
        }
        i += 4;
    }
    data.to_vec()
}

fn arm_encode(data: &mut [u8], file_offset: u64) -> Vec<u8> {
    let n = data.len();
    if n < 4 {
        return data.to_vec();
    }
    let mut i = 0;
    while i + 3 < n {
        if data[i + 3] == 0xEB {
            let offset =
                (data[i] as u32) | ((data[i + 1] as u32) << 8) | ((data[i + 2] as u32) << 16);
            let adj = offset.wrapping_add(((file_offset as u32).wrapping_add(i as u32)) >> 2);
            let masked = adj & 0xFF_FFFF;
            data[i] = (masked & 0xFF) as u8;
            data[i + 1] = ((masked >> 8) & 0xFF) as u8;
            data[i + 2] = ((masked >> 16) & 0xFF) as u8;
        }
        i += 4;
    }
    data.to_vec()
}
