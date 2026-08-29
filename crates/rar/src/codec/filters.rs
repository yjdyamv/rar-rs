/// RAR5 output filters: Delta, E8, E8E9, ARM.
///
/// Post-processing filters applied to regions of decompressed output.
/// Each filter has decode (inverse) and encode (forward) functions.
use super::rar50::*;

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
// Both normalize the file position modulo the 16 MB virtual size, matching
// WinRAR's unpack.cpp (`Offset = (CurPos + FileOffset) % 0x1000000`).

/// Virtual file size for the x86 address normalisation model.
const X86_FILTER_FILE_SIZE: u32 = 0x0100_0000;

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
            let offset = file_offset.wrapping_add(cur_pos as u64) as u32 % X86_FILTER_FILE_SIZE;
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
            let offset = file_offset.wrapping_add(cur_pos as u64) as u32 % X86_FILTER_FILE_SIZE;
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

// ── Automatic x86 filter detection ─────────────────────────────────────────
//
// Structural scan for x86 code regions (ported from the `rars` project
// `x86_filter_scan.rs`, MIT OR Apache-2.0). E8 (CALL) / E9 (JMP) opcodes are
// clustered by proximity; clusters that meet a minimum density become filter
// regions (with padding). Isolated opcodes never form a cluster, so data that
// merely contains a few 0xE8/0xE9 bytes is not filtered.

/// Maximum gap between two opcodes that still belong to one cluster.
const AUTO_X86_CLUSTER_GAP: usize = 4096;
/// Tighter clustering pass: catches dense code inside sparse spans.
const AUTO_X86_TIGHT_CLUSTER_GAP: usize = 512;
/// Maximum gap between clusters that still belong to one broad span.
const AUTO_X86_SPAN_CLUSTER_GAP: usize = 32768;
/// Padding added around each detected region.
const AUTO_X86_RANGE_PADDING: usize = 16;
/// Maximum number of individual cluster ranges kept.
const AUTO_X86_MAX_RANGES: usize = 8;
/// Maximum number of broad span ranges kept.
const AUTO_X86_MAX_SPAN_RANGES: usize = 4;
/// Minimum total opcodes in a span for it to be filtered.
const AUTO_X86_MIN_SPAN_OPCODES: usize = 4;

/// Find the next x86 E8 (or E9 when `cmp_mask == 0xFE`) opcode at or after
/// `start`, scanning up to `end_exclusive`.
fn next_x86_opcode(data: &[u8], start: usize, end_exclusive: usize, cmp_mask: u8) -> Option<usize> {
    let end = end_exclusive.min(data.len());
    if start >= end {
        return None;
    }
    data[start..end]
        .iter()
        .position(|&byte| byte & cmp_mask == 0xE8)
        .map(|offset| start + offset)
}

/// Detect regions of x86 code in `data`, returning `[start, end)` ranges.
/// `include_e9` selects the E8-only (0xFF mask) or E8+E9 (0xFE mask) scan.
///
/// Mirrors the reference scanner: opcode clusters are formed with both a
/// wide and a tight gap; clusters become padded ranges (capped at
/// [`AUTO_X86_MAX_RANGES`]) and broad spans become additional ranges
/// (capped at [`AUTO_X86_MAX_SPAN_RANGES`]).
pub fn auto_x86_filter_ranges(data: &[u8], include_e9: bool) -> Vec<std::ops::Range<usize>> {
    let mut ranges =
        auto_x86_filter_ranges_with_cluster_gap(data, include_e9, AUTO_X86_CLUSTER_GAP);
    for range in
        auto_x86_filter_ranges_with_cluster_gap(data, include_e9, AUTO_X86_TIGHT_CLUSTER_GAP)
    {
        if !ranges.contains(&range) {
            ranges.push(range);
        }
    }
    ranges
}

fn auto_x86_filter_ranges_with_cluster_gap(
    data: &[u8],
    include_e9: bool,
    cluster_gap: usize,
) -> Vec<std::ops::Range<usize>> {
    if data.len() <= 5 {
        return Vec::new();
    }

    let cmp_mask = if include_e9 { 0xFE } else { 0xFF };
    let mut clusters: Vec<(usize, usize, usize)> = Vec::new();
    let mut current: Option<(usize, usize, usize)> = None;
    let mut scan_pos = 0usize;
    while let Some(pos) = next_x86_opcode(data, scan_pos, data.len() - 4, cmp_mask) {
        match current {
            Some((start, last, count)) if pos - last <= cluster_gap => {
                current = Some((start, pos, count + 1));
            }
            Some(cluster) => {
                clusters.push(cluster);
                current = Some((pos, pos, 1));
            }
            None => current = Some((pos, pos, 1)),
        }
        scan_pos = pos + 1;
    }
    if let Some(cluster) = current {
        clusters.push(cluster);
    }

    clusters.retain(|&(_, _, count)| count >= 2);
    let mut ranges = Vec::new();
    let mut span_count = 0;
    let mut span: Option<(usize, usize, usize)> = None;
    for &(start, last, count) in &clusters {
        match span {
            Some((span_start, span_last, span_opcodes))
                if start.saturating_sub(span_last) <= AUTO_X86_SPAN_CLUSTER_GAP =>
            {
                span = Some((span_start, last, span_opcodes + count));
            }
            Some((span_start, span_last, span_opcodes)) => {
                if span_opcodes >= AUTO_X86_MIN_SPAN_OPCODES
                    && span_count < AUTO_X86_MAX_SPAN_RANGES
                {
                    push_x86_filter_range(&mut ranges, data.len(), span_start, span_last);
                    span_count += 1;
                }
                span = Some((start, last, count));
            }
            None => span = Some((start, last, count)),
        }
    }
    if let Some((span_start, span_last, span_opcodes)) = span
        && span_opcodes >= AUTO_X86_MIN_SPAN_OPCODES
        && span_count < AUTO_X86_MAX_SPAN_RANGES
    {
        push_x86_filter_range(&mut ranges, data.len(), span_start, span_last);
    }

    // Keep the densest individual clusters as additional (smaller) ranges.
    clusters.sort_by(|a, b| {
        let a_len = a.1 - a.0 + 5;
        let b_len = b.1 - b.0 + 5;
        b.2.cmp(&a.2).then_with(|| a_len.cmp(&b_len))
    });
    clusters.truncate(AUTO_X86_MAX_RANGES);

    for (start, last, _) in clusters {
        push_x86_filter_range(&mut ranges, data.len(), start, last);
    }
    ranges
}

fn push_x86_filter_range(
    ranges: &mut Vec<std::ops::Range<usize>>,
    data_len: usize,
    start: usize,
    last: usize,
) {
    let range_start = start.saturating_sub(AUTO_X86_RANGE_PADDING);
    let range_end = (last + 5 + AUTO_X86_RANGE_PADDING).min(data_len);
    let range = range_start..range_end;
    if range.start < range.end && !ranges.contains(&range) {
        ranges.push(range);
    }
}

// ── Automatic delta (multimedia) filter detection ──────────────────────────
//
// WinRAR applies a delta filter to correlated multi-channel data (audio PCM,
// raw bitmaps, database pages) before LZSS. The channel count is chosen by
// compressed size in the encoder (see `encode_with_auto_delta_filter`), which
// is robust to byte-wrapping; here we only provide a cheap pre-gate.

/// Candidate channel counts tried for the delta filter. 1..=4 covers 8/16/24/
/// 32-bit interleaved streams, which is what real archives contain.
pub(crate) const AUTO_DELTA_CHANNELS: &[u8] = &[1, 2, 3, 4];
/// Minimum data length before a delta filter is even considered.
const AUTO_DELTA_MIN_LEN: usize = 256;

/// Per-channel statistics of the delta(`channels`)-transformed `data`,
/// computed in a single allocation-free pass. `mag_sum` is the total absolute
/// (wrapping-aware) delta magnitude — small for correlated multi-channel data,
/// large for random; `near_zero` is the count of deltas whose magnitude is at
/// most 8, which is the robust "is this correlated?" signal.
struct DeltaStats {
    mag_sum: u64,
    near_zero: u64,
    total: u64,
}

fn delta_stats(data: &[u8], channels: u8) -> DeltaStats {
    let ch = channels.max(1) as usize;
    let mut prev = vec![0u8; ch];
    let mut stats = DeltaStats {
        mag_sum: 0,
        near_zero: 0,
        total: 0,
    };
    for (i, &byte) in data.iter().enumerate() {
        let c = i % ch;
        let d = prev[c].wrapping_sub(byte) as i32; // -255..255
        let mag = d.unsigned_abs() as u64;
        stats.mag_sum += mag;
        if mag <= 8 {
            stats.near_zero += 1;
        }
        prev[c] = byte;
        stats.total += 1;
    }
    stats
}

/// Detect whether a delta (multimedia) filter is likely to help compress
/// `data`, returning the best channel count, or `None` when filtering is not
/// worthwhile.
///
/// Channels are ranked by total absolute delta magnitude; the winner is kept
/// only when a large fraction of its deltas are near-zero (`<= 8`), which
/// robustly separates correlated multi-channel data (audio PCM, raw bitmaps,
/// database pages) from random data. (A previous proxy summed the wrapping
/// `u8` form of each delta, so a delta of `-4` counted as `252` and rejected
/// exactly the correlated data it should favour — fixed by using magnitude.)
/// The encoder then compares the delta-filtered size against plain LZSS and
/// keeps the filter only when it is strictly smaller, so text and other
/// structured-but-not-multi-channel data is left to plain LZSS automatically.
pub fn auto_delta_filter_channels(data: &[u8]) -> Option<u8> {
    if data.len() < AUTO_DELTA_MIN_LEN {
        return None;
    }
    // All-identical data is already handled optimally by plain LZSS.
    if data.iter().all(|&b| b == data[0]) {
        return None;
    }
    let mut best: Option<(u8, u64)> = None; // (channel, mag_sum)
    let mut best_near_zero = 0u64;
    let mut best_total = 0u64;
    for &ch in AUTO_DELTA_CHANNELS {
        let s = delta_stats(data, ch);
        if best.is_none() || s.mag_sum < best.unwrap().1 {
            best = Some((ch, s.mag_sum));
            best_near_zero = s.near_zero;
            best_total = s.total;
        }
    }
    let (ch, _) = best?;
    // Correlated data keeps the vast majority of its deltas small.
    (best_near_zero * 2 >= best_total).then_some(ch)
}

#[cfg(test)]
mod x86_tests {
    use super::*;

    #[test]
    fn auto_x86_detects_clusters_but_not_isolated_opcodes() {
        // Two isolated opcodes never form a cluster.
        let mut data = vec![0x41u8; 20_000];
        data[100] = 0xE8;
        data[10_000] = 0xE8;
        assert!(auto_x86_filter_ranges(&data, false).is_empty());

        // A dense cluster does.
        let mut data = vec![0x41u8; 20_000];
        for pos in [1024, 1050, 1090, 1130] {
            data[pos] = 0xE8;
        }
        let ranges = auto_x86_filter_ranges(&data, false);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 1008..1151);
    }

    #[test]
    fn auto_x86_clamps_ranges_to_buffer_bounds() {
        let mut data = vec![0x41u8; 30];
        for pos in [0, 4, 8, 12] {
            data[pos] = 0xE8;
        }
        let ranges = auto_x86_filter_ranges(&data, false);
        assert_eq!(ranges, vec![0..30]);
    }

    #[test]
    fn e8e9_scan_finds_jumps_too() {
        let mut data = vec![0x41u8; 8000];
        data[1024] = 0xE8;
        data[1088] = 0xE9; // JMP — only found when include_e9
        assert!(auto_x86_filter_ranges(&data, false).is_empty());
        assert!(!auto_x86_filter_ranges(&data, true).is_empty());
    }

    /// Independent scalar reimplementation of the cluster logic, used to
    /// cross-check the scanner at chunk boundaries.
    fn scalar_ranges(data: &[u8], include_e9: bool) -> Vec<std::ops::Range<usize>> {
        let mut out = auto_x86_filter_ranges_with_cluster_gap(data, include_e9, 4096);
        for r in auto_x86_filter_ranges_with_cluster_gap(data, include_e9, 512) {
            if !out.contains(&r) {
                out.push(r);
            }
        }
        out
    }

    #[test]
    fn matches_reference_scanner() {
        let mut data = vec![0x41u8; 150_000];
        for pos in [
            31usize, 32, 33, 1024, 1088, 4096, 4160, 80_000, 80_032, 80_064,
        ] {
            data[pos] = 0xE8;
        }
        data[80_096] = 0xE9;
        assert_eq!(
            auto_x86_filter_ranges(&data, false),
            scalar_ranges(&data, false)
        );
        assert_eq!(
            auto_x86_filter_ranges(&data, true),
            scalar_ranges(&data, true)
        );
    }
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

    /// Build `n` samples of `bytes`-byte little-endian values (8/16/24-bit)
    /// with a small per-sample random walk, interleaved over `channels` lanes.
    /// Correlated across samples (small deltas) — the canonical delta-filter
    /// input.
    fn correlated_samples(bytes: usize, channels: usize, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes * channels * n);
        let mut val = vec![0i64; channels];
        let mut state = 0x1234_5678u64;
        for _ in 0..n {
            for c in 0..channels {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                let r = (state >> 33) as u32;
                val[c] += (r % 8) as i64 - 4;
                let v = val[c] as i64;
                for b in 0..bytes {
                    out.push((v >> (8 * b)) as u8);
                }
            }
        }
        out
    }

    #[test]
    fn picks_correct_channel_count_for_interleaved_streams() {
        // 8/16/24-bit interleaved streams map to delta channels 1/2/3.
        assert_eq!(
            auto_delta_filter_channels(&correlated_samples(1, 1, 4096)),
            Some(1)
        );
        assert_eq!(
            auto_delta_filter_channels(&correlated_samples(2, 2, 4096)),
            Some(2)
        );
        assert_eq!(
            auto_delta_filter_channels(&correlated_samples(3, 3, 4096)),
            Some(3)
        );
        // Two interleaved 8-bit channels (stereo u8) is a 2-channel delta.
        assert_eq!(
            auto_delta_filter_channels(&correlated_samples(1, 2, 4096)),
            Some(2)
        );
        // A 4-lane (32-bit) layout aligns to channel 4.
        assert_eq!(
            auto_delta_filter_channels(&correlated_samples(2, 4, 4096)),
            Some(4)
        );
    }

    #[test]
    fn rejects_random_data() {
        let mut state = 0x9E37_9B97_7F4A_7C15u64;
        let mut random = vec![0u8; 8192];
        for b in random.iter_mut() {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            *b = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u8;
        }
        assert_eq!(auto_delta_filter_channels(&random), None);
    }

    #[test]
    fn too_short_is_rejected() {
        assert_eq!(auto_delta_filter_channels(&[0u8; 100]), None);
    }

    #[test]
    fn all_identical_is_rejected() {
        // An all-same byte stream is already optimal for plain LZSS; a delta
        // filter is pointless (and would force a non-solid member).
        assert_eq!(auto_delta_filter_channels(&[0xABu8; 8192]), None);
    }
}
