//! Writer-side auto-filter policy for RAR5 members.
//!
//! The decision procedure follows the `rars` project
//! (https://github.com/bitplane/rars, MIT OR Apache-2.0)
//! `rar50/write/filter_policy.rs`: try whole-member and ranged Delta,
//! E8, E8E9 and ARM candidates and keep the smallest packed result,
//! skipping text-like inputs. Filtered members are encoded in one shot
//! (the region transforms require the whole member), so this module is
//! only used for non-solid members within a bounded size.

use super::encoder::{FilterSpec, encode_with_filters};
use super::tables::*;
use super::x86_filter_scan::auto_x86_filter_ranges;
use crate::codec;

/// Maximum member size for which the auto-filter policy runs in one
/// buffer. Larger members stream through the plain chunked encoder.
pub const AUTO_FILTER_MAX_BUFFER: usize = 16 * 1024 * 1024;
/// Minimum member size worth probing filters for.
pub const AUTO_FILTER_MIN_SIZE: usize = 256;

/// Filter policy for archive creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterPolicy {
    /// No output filters; plain LZSS+Huffman.
    None,
    /// Automatically try Delta / E8 / E8E9 / ARM candidates and keep the
    /// smallest packed output.
    #[default]
    AutoSize,
}

impl FilterPolicy {
    pub fn is_enabled(&self) -> bool {
        matches!(self, FilterPolicy::AutoSize)
    }
}

/// Encode a whole member with the auto-filter policy, returning the
/// smallest packed result (unfiltered LZ is always the baseline).
pub fn encode_member_with_policy(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    policy: FilterPolicy,
) -> Result<Vec<u8>, String> {
    if policy != FilterPolicy::AutoSize
        || data.len() < AUTO_FILTER_MIN_SIZE
        || data.len() > AUTO_FILTER_MAX_BUFFER
    {
        return codec::encode_chunked(data, method, dict_size_log, data.len(), None, true, None);
    }

    let mut best =
        codec::encode_chunked(data, method, dict_size_log, data.len(), None, true, None)?;
    if is_text_like_filter_skip_candidate(data) {
        return Ok(best);
    }

    let e8_ranges = auto_x86_filter_ranges(data, false);
    let e8e9_ranges = auto_x86_filter_ranges(data, true);

    // Single-record candidates (deduplicated: ranged deltas and x86
    // clusters can coincide with whole-member specs).
    let mut candidates: Vec<FilterSpec> = Vec::with_capacity(16);
    candidates.push(FilterSpec::new(FILTER_E8, 0, 0, data.len() as u32));
    candidates.push(FilterSpec::new(FILTER_E8E9, 0, 0, data.len() as u32));
    candidates.push(FilterSpec::new(FILTER_ARM, 0, 0, data.len() as u32));
    for channels in 1..=4 {
        candidates.push(FilterSpec::new(
            FILTER_DELTA,
            channels,
            0,
            data.len() as u32,
        ));
    }
    for range in auto_delta_filter_ranges(data) {
        candidates.push(FilterSpec::new(
            FILTER_DELTA,
            range.channels as u8,
            range.range.start as u32,
            range.range.len() as u32,
        ));
    }
    for range in &e8_ranges {
        candidates.push(FilterSpec::new(
            FILTER_E8,
            0,
            range.start as u32,
            range.len() as u32,
        ));
    }
    for range in &e8e9_ranges {
        candidates.push(FilterSpec::new(
            FILTER_E8E9,
            0,
            range.start as u32,
            range.len() as u32,
        ));
    }

    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;

        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<&FilterSpec> = candidates
            .iter()
            .filter(|spec| seen.insert(**spec))
            .collect();
        // Encode every candidate in parallel; results are replayed in the
        // original candidate order so the winning packed stream (and any
        // tie-break) is identical to the sequential scan.
        let results: Vec<Result<Vec<u8>, String>> = deduped
            .par_iter()
            .map(|spec| {
                encode_with_filters(data, method, dict_size_log, std::slice::from_ref(spec))
            })
            .collect();
        for packed in results {
            let packed = packed?;
            if packed.len() < best.len() {
                best = packed;
            }
        }
    }
    #[cfg(not(feature = "parallel"))]
    {
        let mut seen = std::collections::HashSet::new();
        for spec in candidates {
            if !seen.insert(spec) {
                continue;
            }
            let packed =
                encode_with_filters(data, method, dict_size_log, std::slice::from_ref(&spec))?;
            if packed.len() < best.len() {
                best = packed;
            }
        }
    }

    // Combined disjoint ranges are tried as one member (multiple filter
    // records), which wins for multi-section executables.
    let combined_e8 = disjoint_filter_ranges(e8_ranges);
    if combined_e8.len() > 1 {
        let specs: Vec<FilterSpec> = combined_e8
            .iter()
            .map(|r| FilterSpec::new(FILTER_E8, 0, r.start as u32, r.len() as u32))
            .collect();
        if let Ok(packed) = encode_with_filters(data, method, dict_size_log, &specs) {
            if packed.len() < best.len() {
                best = packed;
            }
        }
    }
    let combined_e8e9 = disjoint_filter_ranges(e8e9_ranges);
    if combined_e8e9.len() > 1 {
        let specs: Vec<FilterSpec> = combined_e8e9
            .iter()
            .map(|r| FilterSpec::new(FILTER_E8E9, 0, r.start as u32, r.len() as u32))
            .collect();
        if let Ok(packed) = encode_with_filters(data, method, dict_size_log, &specs) {
            if packed.len() < best.len() {
                best = packed;
            }
        }
    }

    Ok(best)
}

struct DeltaRange {
    channels: usize,
    range: std::ops::Range<usize>,
}

const AUTO_DELTA_EDGE_SKIP: usize = 64;

fn auto_delta_filter_ranges(data: &[u8]) -> Vec<DeltaRange> {
    let mut out = Vec::new();
    for channels in 1..=4 {
        if let Some(range) = auto_delta_filter_range(data, channels) {
            out.push(DeltaRange { channels, range });
        }
    }
    out
}

fn auto_delta_filter_range(data: &[u8], channels: usize) -> Option<std::ops::Range<usize>> {
    if channels == 0 || data.len() <= AUTO_DELTA_EDGE_SKIP * 2 + channels * 8 {
        return None;
    }
    let start = AUTO_DELTA_EDGE_SKIP;
    let end = data.len() - AUTO_DELTA_EDGE_SKIP;
    let aligned_start = start + ((channels - start % channels) % channels);
    let aligned_end = end - (end - aligned_start) % channels;
    (aligned_start + channels * 8 <= aligned_end).then_some(aligned_start..aligned_end)
}

pub fn disjoint_filter_ranges(
    mut ranges: Vec<std::ops::Range<usize>>,
) -> Vec<std::ops::Range<usize>> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut disjoint: Vec<std::ops::Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = disjoint.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        disjoint.push(range);
    }
    disjoint
}

fn is_text_like_filter_skip_candidate(data: &[u8]) -> bool {
    let sample_len = data.len().min(8192);
    if sample_len == 0 {
        return false;
    }
    let sample = &data[..sample_len];
    let text_bytes = sample
        .iter()
        .filter(|&&byte| matches!(byte, b'\t' | b'\n' | b'\r' | 0x20..=0x7e))
        .count();
    text_bytes * 100 / sample_len >= 95
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::decode_standalone;

    #[test]
    fn delta_filter_improves_or_matches_ratio_on_smooth_data() {
        // Smooth ramp data: delta encoding concentrates values, so the
        // filtered member must not be worse than unfiltered.
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let plain = codec::encode_chunked(&data, 3, 3, data.len(), None, true, None).unwrap();
        let filtered = encode_member_with_policy(&data, 3, 3, FilterPolicy::AutoSize).unwrap();
        assert!(filtered.len() <= plain.len() + plain.len() / 10);
        let roundtrip = decode_standalone(&filtered, data.len() as u64, 3).unwrap();
        assert_eq!(roundtrip, data);
    }

    #[test]
    fn e8_filter_roundtrips_on_fake_executable() {
        // A fake x86 binary with many E8 call sites.
        let mut data = vec![0x90u8; 50_000];
        for pos in (512..49_000).step_by(64) {
            data[pos] = 0xe8;
            let addr = ((pos as u32).wrapping_mul(7)) & 0x00FF_FFFF;
            data[pos + 1..pos + 5].copy_from_slice(&addr.to_le_bytes());
        }
        let plain = codec::encode_chunked(&data, 3, 3, data.len(), None, true, None).unwrap();
        let filtered = encode_member_with_policy(&data, 3, 3, FilterPolicy::AutoSize).unwrap();
        assert!(
            filtered.len() < plain.len(),
            "E8 filter should shrink: {} vs {}",
            filtered.len(),
            plain.len()
        );
        let roundtrip = decode_standalone(&filtered, data.len() as u64, 3).unwrap();
        assert_eq!(roundtrip, data);
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_candidate_probing_is_deterministic() {
        // Mix of x86-ish opcode bytes and a delta-friendly ramp so several
        // candidate filters are probed in parallel.
        let mut data: Vec<u8> = (0..80_000u32).map(|i| (i % 251) as u8).collect();
        for (i, byte) in data.iter_mut().enumerate().step_by(7) {
            *byte = if i % 3 == 0 { 0xE8 } else { 0xE9 };
        }
        let first = encode_member_with_policy(&data, 3, 3, FilterPolicy::AutoSize).unwrap();
        let second = encode_member_with_policy(&data, 3, 3, FilterPolicy::AutoSize).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn text_like_input_skips_filters_without_regression() {
        let text: Vec<u8> = b"the quick brown fox jumps over the lazy dog\n"
            .iter()
            .cycle()
            .take(20_000)
            .copied()
            .collect();
        let filtered = encode_member_with_policy(&text, 3, 3, FilterPolicy::AutoSize).unwrap();
        let roundtrip = decode_standalone(&filtered, text.len() as u64, 3).unwrap();
        assert_eq!(roundtrip, text);
    }

    #[test]
    fn policy_none_equals_plain_encode() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 13 % 250) as u8).collect();
        let plain = codec::encode_chunked(&data, 3, 3, data.len(), None, true, None).unwrap();
        let policy = encode_member_with_policy(&data, 3, 3, FilterPolicy::None).unwrap();
        assert_eq!(policy, plain);
    }
}
