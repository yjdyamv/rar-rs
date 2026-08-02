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
// file_size of 0x1000000.  During compression the encoder converts
// relative CALL/JMP targets to position-independent canonical form;
// the decoder reverses the transform.
//
// Reference: libarchive archive_read_support_format_rar5.c run_e8e9_filter()

const E8_FILE_SIZE: u32 = 0x100_0000;

fn e8_decode(data: &mut [u8], file_offset: u64, e8_only: bool) -> Vec<u8> {
    let n = data.len();
    if n < 5 {
        return data.to_vec();
    }
    let mut i = 0usize;
    while i < n - 4 {
        let opcode = data[i];
        if opcode == 0xE8 || (!e8_only && opcode == 0xE9) {
            i += 1; // advance past opcode — i now points at address bytes
            let offset = ((i as u64 + file_offset) % E8_FILE_SIZE as u64) as u32;
            let addr = u32::from_le_bytes(data[i..i + 4].try_into().unwrap());

            let new_addr = if addr & 0x8000_0000 != 0 {
                // Negative address
                if addr.wrapping_add(offset) & 0x8000_0000 == 0 {
                    addr.wrapping_add(E8_FILE_SIZE)
                } else {
                    addr // unchanged
                }
            } else {
                // Positive address
                if addr.wrapping_sub(E8_FILE_SIZE) & 0x8000_0000 != 0 {
                    // addr < E8_FILE_SIZE
                    addr.wrapping_sub(offset)
                } else {
                    addr // unchanged
                }
            };

            if new_addr != addr {
                data[i..i + 4].copy_from_slice(&new_addr.to_le_bytes());
            }
            i += 4;
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
    let mut i = 0usize;
    while i < n - 4 {
        let opcode = data[i];
        if opcode == 0xE8 || (!e8_only && opcode == 0xE9) {
            i += 1;
            let offset = ((i as u64 + file_offset) % E8_FILE_SIZE as u64) as u32;
            let addr = u32::from_le_bytes(data[i..i + 4].try_into().unwrap());

            let new_addr = if addr & 0x8000_0000 != 0 {
                // Negative address
                if addr.wrapping_add(offset) & 0x8000_0000 == 0 {
                    addr.wrapping_sub(E8_FILE_SIZE)
                } else {
                    addr
                }
            } else {
                // Positive address
                if addr.wrapping_add(offset) & 0x8000_0000 == 0 {
                    addr.wrapping_add(offset)
                } else {
                    addr
                }
            };

            if new_addr != addr {
                data[i..i + 4].copy_from_slice(&new_addr.to_le_bytes());
            }
            i += 4;
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
