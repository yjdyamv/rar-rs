/// RAR5 output filters: Delta, E8, E8E9, ARM.
///
/// Post-processing filters applied to regions of decompressed output.
/// Each filter has decode (inverse) and encode (forward) functions.
use super::tables::*;

/// Apply the inverse filter (for decompression).
///
/// Filter types 4-7 (ARMT/IA64/PPC/SPARC) are defined in the RAR5 format
/// notes but are never produced by any implementation (WinRAR 7.23 only
/// emits Delta/E8/E8E9; ARM was disabled in 5.80; unrar 5.9.4/7.23 and
/// 7-Zip implement and produce nothing beyond type 3). We refuse them
/// explicitly instead of silently returning unfiltered data, so a
/// hypothetical archive using them fails with a clear error rather than a
/// CRC mismatch.
pub fn apply_filter_decode(
    filter_type: u8,
    data: &mut [u8],
    channels: u8,
    file_offset: u64,
) -> Result<Vec<u8>, String> {
    match filter_type {
        FILTER_DELTA => Ok(delta_decode(data, channels)),
        FILTER_E8 => Ok(e8_decode(data, file_offset, true)),
        FILTER_E8E9 => Ok(e8_decode(data, file_offset, false)),
        FILTER_ARM => Ok(arm_decode(data, file_offset)),
        other => Err(format!("unsupported RAR5 filter type {other}")),
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
    #[cfg(feature = "simd")]
    if channels == 1 {
        return delta_decode_simd_ch1(data);
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
    #[cfg(feature = "simd")]
    if channels == 1 {
        return delta_encode_simd_ch1(data);
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

/// SIMD single-channel delta encode (16 lanes at a time, scalar tail).
/// Output is byte-identical to the scalar loop: `out[i] = prev - x[i]`.
#[cfg(feature = "simd")]
fn delta_encode_simd_ch1(data: &[u8]) -> Vec<u8> {
    use wide::u8x16;

    let n = data.len();
    let mut result = vec![0u8; n];
    let mut prev = 0u8;
    let mut i = 0usize;
    while i + 16 <= n {
        let arr: [u8; 16] = data[i..i + 16].try_into().expect("16-byte window");
        let x = u8x16::from(arr);
        let xa = x.to_array();
        let mut out = [0u8; 16];
        out[0] = prev.wrapping_sub(xa[0]);
        for j in 1..16 {
            out[j] = xa[j - 1].wrapping_sub(xa[j]);
        }
        prev = xa[15];
        result[i..i + 16].copy_from_slice(&out);
        i += 16;
    }
    for (j, &byte) in data[i..].iter().enumerate() {
        result[i + j] = prev.wrapping_sub(byte);
        prev = byte;
    }
    result
}

/// SIMD single-channel delta decode using wrapping prefix sums:
/// `out[j] = prev - (x[0] + ... + x[j])`, 16 lanes at a time.
#[cfg(feature = "simd")]
fn delta_decode_simd_ch1(data: &[u8]) -> Vec<u8> {
    use wide::u8x16;

    let n = data.len();
    let mut result = vec![0u8; n];
    let mut prev = 0u8;
    let mut i = 0usize;
    while i + 16 <= n {
        let mut prefix: [u8; 16] = data[i..i + 16].try_into().expect("16-byte window");
        let mut step = 1usize;
        while step < 16 {
            for j in (step..16).rev() {
                prefix[j] = prefix[j].wrapping_add(prefix[j - step]);
            }
            step *= 2;
        }
        let out = u8x16::splat(prev) - u8x16::from(prefix);
        let out_arr = out.to_array();
        prev = out_arr[15];
        result[i..i + 16].copy_from_slice(&out_arr);
        i += 16;
    }
    for j in i..n {
        result[j] = prev.wrapping_sub(data[j]);
        prev = result[j];
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
            } else if addr & 0x8000_0000 != 0 && addr.wrapping_add(offset) & 0x8000_0000 == 0 {
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
                if candidate & 0x8000_0000 != 0 && candidate.wrapping_add(offset) & 0x8000_0000 == 0
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

#[cfg(all(test, feature = "simd"))]
mod tests {
    use super::*;

    fn scalar_delta_encode(data: &[u8], channels: u8) -> Vec<u8> {
        let ch = channels.max(1) as usize;
        let mut result = vec![0u8; data.len()];
        let mut dst = 0usize;
        for c in 0..ch {
            let mut prev = 0u8;
            let mut i = c;
            while i < data.len() {
                let val = data[i];
                result[dst] = prev.wrapping_sub(val);
                prev = val;
                dst += 1;
                i += ch;
            }
        }
        result
    }

    fn scalar_delta_decode(data: &[u8], channels: u8) -> Vec<u8> {
        let ch = channels.max(1) as usize;
        let mut result = vec![0u8; data.len()];
        let mut src = 0usize;
        for c in 0..ch {
            let mut prev = 0u8;
            let mut i = c;
            while i < data.len() {
                prev = prev.wrapping_sub(data[src]);
                result[i] = prev;
                src += 1;
                i += ch;
            }
        }
        result
    }

    #[test]
    fn simd_delta_matches_scalar_for_all_channels() {
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut data = vec![0u8; 4099];
        for byte in data.iter_mut() {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            *byte = (state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8;
        }
        for channels in [1u8, 2, 3, 4] {
            let mut enc_input = data.clone();
            let encoded = apply_filter_encode(FILTER_DELTA, &mut enc_input, channels, 0);
            assert_eq!(
                encoded,
                scalar_delta_encode(&data, channels),
                "delta encode mismatch for channels={channels}"
            );

            let mut dec_input = encoded.clone();
            let decoded = apply_filter_decode(FILTER_DELTA, &mut dec_input, channels, 0).unwrap();
            assert_eq!(
                decoded, data,
                "delta roundtrip failed for channels={channels}"
            );
            assert_eq!(
                decoded,
                scalar_delta_decode(&encoded, channels),
                "delta decode mismatch for channels={channels}"
            );
        }
    }

    #[test]
    fn unknown_filter_type_is_rejected_not_silently_skipped() {
        // Types 4-7 (ARMT/IA64/PPC/SPARC) are never produced by WinRAR 7.23
        // or any other implementation; they must fail loudly, not return
        // the raw data (which would corrupt output without a clear error).
        let mut data = vec![0xAB; 64];
        let err = apply_filter_decode(4, &mut data, 1, 0).unwrap_err();
        assert!(err.contains("unsupported RAR5 filter type 4"), "{err}");
    }
}
