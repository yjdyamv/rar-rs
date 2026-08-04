//! x86 E8/E8E9 call-site scanner for writer-side auto-filter detection.
//!
//! Ported from the `rars` project (https://github.com/bitplane/rars,
//! MIT OR Apache-2.0): `x86_filter_scan.rs`. The clustering/spanning
//! logic matches the upstream implementation; the inner opcode scan uses
//! a portable 64-bit word-compare loop instead of the nightly `std::simd`
//! fast path, so this module compiles on stable Rust.

use std::ops::Range;

const AUTO_X86_CLUSTER_GAP: usize = 4096;
const AUTO_X86_TIGHT_CLUSTER_GAP: usize = 512;
const AUTO_X86_SPAN_CLUSTER_GAP: usize = 32768;
const AUTO_X86_RANGE_PADDING: usize = 16;
const AUTO_X86_MAX_RANGES: usize = 8;
const AUTO_X86_MAX_SPAN_RANGES: usize = 4;
const AUTO_X86_MIN_SPAN_OPCODES: usize = 4;

/// Scan `data` for x86 CALL/JMP opcode sites and return candidate filter
/// ranges. `include_e9` also treats `0xE9` (near JMP) as a call site.
pub fn auto_x86_filter_ranges(data: &[u8], include_e9: bool) -> Vec<Range<usize>> {
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
) -> Vec<Range<usize>> {
    if data.len() <= 5 {
        return Vec::new();
    }

    let cmp_mask = if include_e9 { 0xfe } else { 0xff };
    let mut clusters = Vec::new();
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
    if let Some((span_start, span_last, span_opcodes)) = span {
        if span_opcodes >= AUTO_X86_MIN_SPAN_OPCODES && span_count < AUTO_X86_MAX_SPAN_RANGES {
            push_x86_filter_range(&mut ranges, data.len(), span_start, span_last);
        }
    }

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
    ranges: &mut Vec<Range<usize>>,
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

/// Find the next byte position in `data[start..end)` whose value masked
/// with `cmp_mask` equals `0xE8` (i.e. byte == 0xE8 for mask 0xFF, or
/// byte == 0xE8 | 0xE9 for mask 0xFE). 64-bit word-compare scan with a
/// scalar tail; safe on all alignments.
fn next_x86_opcode(data: &[u8], start: usize, end: usize, cmp_mask: u8) -> Option<usize> {
    #[cfg(feature = "simd")]
    {
        // Portable SIMD scan (memchr dispatch: SSE2/AVX2 on x86_64, NEON on
        // aarch64); the scalar word-scan below is the fallback build.
        if start >= end {
            return None;
        }
        let haystack = &data[start..end];
        let found = if cmp_mask == 0xFF {
            memchr::memchr(0xE8, haystack)
        } else {
            memchr::memchr2(0xE8, 0xE9, haystack)
        };
        return found.map(|off| start + off);
    }
    #[cfg(not(feature = "simd"))]
    {
        let mut pos = start;
        if pos >= end {
            return None;
        }
        // Fast path: process 8 bytes at a time. Each byte is matched when
        // (byte & mask) == 0xE8; compute `(bytes & mask_repl) ^ 0xE8_repl`
        // and look for zero bytes, which mark matching positions.
        while pos + 8 <= end {
            let bytes = u64::from_le_bytes(data[pos..pos + 8].try_into().expect("8-byte window"));
            let mask_repl = u64::from_le_bytes([cmp_mask; 8]);
            let e8_repl = u64::from_le_bytes([0xE8; 8]);
            let xored = (bytes & mask_repl) ^ e8_repl;
            if let Some(off) = has_zero_byte(xored) {
                return Some(pos + off);
            }
            pos += 8;
        }
        // Scalar tail: at most 7 remaining bytes.
        while pos < end {
            if data[pos] & cmp_mask == 0xE8 {
                return Some(pos);
            }
            pos += 1;
        }
        None
    }
}

/// Return the byte offset of the first zero byte in `v`, or `None`.
#[cfg(not(feature = "simd"))]
fn has_zero_byte(v: u64) -> Option<usize> {
    let y = v.wrapping_sub(0x0101_0101_0101_0101) & !v & 0x8080_8080_8080_8080;
    if y == 0 {
        return None;
    }
    let tz = y.trailing_zeros() as usize;
    Some(tz / 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar_next_x86_opcode(data: &[u8], start: usize, end: usize, mask: u8) -> Option<usize> {
        (start..end).find(|&i| data[i] & mask == 0xE8)
    }

    #[test]
    fn word_scan_matches_scalar_scan() {
        let mut data = vec![0x41u8; 150_000];
        for pos in [
            0usize, 1, 7, 8, 9, 31, 32, 33, 1024, 1088, 4096, 4160, 80_000, 80_032, 80_064,
            149_998, 149_999,
        ] {
            data[pos] = 0xe8;
        }
        data[80_096] = 0xe9;
        for mask in [0xffu8, 0xfe] {
            for start in [0usize, 1, 33, 1088, 80_000, 80_095] {
                let mut expected = scalar_next_x86_opcode(&data, start, data.len() - 4, mask);
                let mut got = next_x86_opcode(&data, start, data.len() - 4, mask);
                let mut count = 0;
                while expected.is_some() && count < 100 {
                    assert_eq!(
                        got, expected,
                        "mismatch at iteration {count}, start {start}, mask {mask:#x}"
                    );
                    let next = expected.unwrap() + 1;
                    expected = scalar_next_x86_opcode(&data, next, data.len() - 4, mask);
                    got = next_x86_opcode(&data, next, data.len() - 4, mask);
                    count += 1;
                }
                assert_eq!(got, expected);
            }
        }
    }

    #[test]
    fn returns_no_ranges_for_inputs_too_short_to_contain_a_call() {
        for len in 0..=5 {
            let data = vec![0xe8; len];
            assert!(auto_x86_filter_ranges(&data, false).is_empty());
            assert!(auto_x86_filter_ranges(&data, true).is_empty());
        }
    }

    #[test]
    fn drops_isolated_opcodes_that_never_form_a_cluster() {
        let mut data = vec![0x41; 20_000];
        data[100] = 0xe8;
        data[10_000] = 0xe8;
        assert!(auto_x86_filter_ranges(&data, false).is_empty());
    }

    #[test]
    fn clamps_padded_range_to_buffer_bounds_at_both_ends() {
        let mut data = vec![0x41u8; 30];
        for pos in [0, 4, 8, 12] {
            data[pos] = 0xe8;
        }

        let ranges = auto_x86_filter_ranges(&data, false);

        assert_eq!(ranges, vec![0..30]);
    }

    #[test]
    fn does_not_duplicate_a_span_range_already_emitted_for_a_cluster() {
        let mut data = vec![0x41u8; 20_000];
        for pos in [1024, 1050, 1090, 1130] {
            data[pos] = 0xe8;
        }

        let ranges = auto_x86_filter_ranges(&data, false);

        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 1008..1151);
    }

    #[test]
    fn includes_tighter_ranges_inside_sparse_code_spans() {
        let mut data = vec![0x41u8; 8_000];
        for pos in [1024, 1088, 3600, 3664] {
            data[pos] = 0xe8;
        }

        let ranges = auto_x86_filter_ranges(&data, false);

        assert!(
            ranges
                .iter()
                .any(|range| range.start <= 1024 && range.end > 3664 && range.len() > 2000),
            "missing broad sparse-code span: {ranges:?}"
        );
        assert!(
            ranges.iter().any(|range| range.contains(&1024)
                && range.contains(&1088)
                && !range.contains(&3600)),
            "missing first tight code cluster: {ranges:?}"
        );
        assert!(
            ranges.iter().any(|range| range.contains(&3600)
                && range.contains(&3664)
                && !range.contains(&1088)),
            "missing second tight code cluster: {ranges:?}"
        );
    }

    #[test]
    fn keeps_more_disjoint_code_section_candidates() {
        let mut data = vec![0x41u8; 700_000];
        for section in 0..8 {
            let start = 16_384 + section * 80_000;
            for index in 0..6 {
                data[start + index * 64] = 0xe8;
            }
        }

        let ranges = auto_x86_filter_ranges(&data, false);

        for section in 0..8 {
            let start = 16_384 + section * 80_000;
            assert!(
                ranges.iter().any(|range| range.contains(&start)),
                "missing x86 section at {start}"
            );
        }
    }
}
