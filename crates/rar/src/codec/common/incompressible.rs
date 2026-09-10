//! Sample-based incompressibility probing.
//!
//! Shared by the codec entry points (so a bare [`lzss_huff::encode`](crate::codec::lzss_huff::encode)
//! short-circuits incompressible inputs instead of paying for match finding
//! plus literal entropy coding) and by the archive write path, which needs
//! the verdict *before* reading the whole member.
//!
//! Compressing a few small samples with the same method costs ~20 ms per
//! sample and reliably identifies media/archives/random data, which would
//! otherwise spend minutes in the match finder only to end up STORE anyway.
//! The 90% threshold is conservative: genuinely compressible inputs (text,
//! code, structured binary) compress the samples far below it. Sampling the
//! head plus quarter points catches files whose tails are incompressible
//! (e.g. text + random media), which a head-only probe misses. Only inputs
//! where at least half of the samples are incompressible are declared
//! incompressible, so files with a small random section keep compressing.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use crate::codec::lzss_huff;
use crate::error::RarResult;
use crate::fs::atomic::read_up_to;

/// Size of the head sample, and the lower bound on the samples below.
pub(crate) const SAMPLE_PROBE_HEAD: usize = 512 * 1024;
/// Size of each tail sample (taken at the quarter points).
const SAMPLE_PROBE_TAIL: usize = 256 * 1024;
/// Stride used when hashing4-byte windows for the distant-repeat escape
/// hatch.
const SAMPLE_REPEAT_STEP: usize = 16;
/// Number of byte-identical bytes required to accept a distant repeat.
const SAMPLE_REPEAT_MIN_MATCH: usize = 64;

/// Incompressible data below this size is not worth probing: the samples
/// dominate the input and the codec would finish anyway.
pub(crate) const SAMPLE_PROBE_MIN_INPUT: usize = 4 * SAMPLE_PROBE_HEAD;

/// In-memory stride probe (used by `add_bytes` and the codec entry points).
pub(crate) fn sample_is_incompressible(data: &[u8], method: u8) -> bool {
    if data.len() < SAMPLE_PROBE_MIN_INPUT {
        return false;
    }
    let mut bad = 0;
    if incompressible_sample(&data[..SAMPLE_PROBE_HEAD], method) {
        bad += 1;
    }
    let mut samples: Vec<&[u8]> = Vec::new();
    for &pos in &[data.len() / 4, data.len() / 2, data.len() * 3 / 4] {
        if pos >= SAMPLE_PROBE_HEAD
            && pos + SAMPLE_PROBE_TAIL <= data.len()
            && incompressible_sample(&data[pos..pos + SAMPLE_PROBE_TAIL], method)
        {
            bad += 1;
        }
        if pos + SAMPLE_PROBE_TAIL <= data.len() {
            samples.push(&data[pos..pos + SAMPLE_PROBE_TAIL]);
        }
    }
    // A file whose random-looking regions repeat each other (e.g. a
    // backup with a distant copy of a random block) is compressible via
    // long-range matching — the raw incompressibility vote must not
    // STORE it. Such regions are byte-identical, which no sampling
    // density can distinguish from plain randomness.
    if bad >= 2 && samples_have_distant_repeats(&data[..SAMPLE_PROBE_HEAD], &samples) {
        return false;
    }
    bad >= 2
}

/// File-based stride probe: head + samples at the quarter points.
pub(crate) fn sample_is_incompressible_file(path: &Path, size: u64, method: u8) -> RarResult<bool> {
    let mut f = File::open(path)?;
    let mut head = vec![0u8; SAMPLE_PROBE_HEAD];
    let n = read_up_to(&mut f, &mut head)?;
    let mut bad = 0;
    if incompressible_sample(&head[..n], method) {
        bad += 1;
    }
    let mut samples: Vec<Vec<u8>> = Vec::new();
    for &quarter in &[size / 4, size / 2, size * 3 / 4] {
        if quarter < SAMPLE_PROBE_HEAD as u64 {
            continue;
        }
        f.seek(SeekFrom::Start(quarter))?;
        let mut sample = vec![0u8; SAMPLE_PROBE_TAIL];
        let n = read_up_to(&mut f, &mut sample)?;
        if n > 0 && incompressible_sample(&sample[..n], method) {
            bad += 1;
        }
        if n > 0 {
            samples.push(sample[..n].to_vec());
        }
    }
    // Same long-range-repeat escape hatch as the in-memory probe.
    let slices: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
    if bad >= 2 && samples_have_distant_repeats(&head[..n], &slices) {
        return Ok(false);
    }
    Ok(bad >= 2)
}

/// Detect byte-identical repeats between the head sample and the quarter
/// samples (sampled every [`SAMPLE_REPEAT_STEP`] bytes, requiring at
/// least [`SAMPLE_REPEAT_MIN_MATCH`] equal bytes). Used to avoid STORE
/// for files whose incompressible-looking regions are distant copies of
/// each other — compressible through the long-range match finder.
fn samples_have_distant_repeats(head: &[u8], samples: &[&[u8]]) -> bool {
    let mut regions: Vec<&[u8]> = Vec::with_capacity(samples.len() + 1);
    regions.push(head);
    regions.extend(samples.iter().copied());
    for i in 0..regions.len() {
        let a = regions[i];
        if a.len() < SAMPLE_REPEAT_STEP + SAMPLE_REPEAT_MIN_MATCH {
            continue;
        }
        // Hash every SAMPLE_REPEAT_STEP-th 4-byte window of region a.
        let mut hashes: HashMap<u32, usize> = HashMap::with_capacity(a.len() / SAMPLE_REPEAT_STEP);
        let mut off = 0;
        while off + 4 <= a.len() {
            let h = (a[off] as u32)
                | ((a[off + 1] as u32) << 8)
                | ((a[off + 2] as u32) << 16)
                | ((a[off + 3] as u32) << 24);
            hashes.insert(h.wrapping_mul(0x9E3779B1), off);
            off += SAMPLE_REPEAT_STEP;
        }
        for b in &regions[i + 1..] {
            let mut off = 0;
            while off + 4 <= b.len() {
                let h = (b[off] as u32)
                    | ((b[off + 1] as u32) << 8)
                    | ((b[off + 2] as u32) << 16)
                    | ((b[off + 3] as u32) << 24);
                if let Some(&a_off) = hashes.get(&h.wrapping_mul(0x9E3779B1)) {
                    // Verify a real run of equal bytes (hash collisions
                    // must not count).
                    let limit = SAMPLE_REPEAT_MIN_MATCH
                        .min(a.len() - a_off)
                        .min(b.len() - off);
                    let mut len = 0;
                    while len < limit && a[a_off + len] == b[off + len] {
                        len += 1;
                    }
                    if len >= SAMPLE_REPEAT_MIN_MATCH {
                        return true;
                    }
                }
                off += SAMPLE_REPEAT_STEP;
            }
        }
    }
    false
}

fn incompressible_sample(sample: &[u8], method: u8) -> bool {
    if sample.is_empty() {
        return false;
    }
    let packed =
        lzss_huff::encode(sample, lzss_huff::EncodeOptions::new(method, 0)).unwrap_or_default();
    packed.len() >= sample.len() * 9 / 10
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// Deterministic pseudo-random bytes (LCG) — incompressible.
    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn probe_recognizes_distant_copy_as_compressible() {
        // 8 MiB of random data followed by its exact copy: the probe
        // samples are all random, but the distant repeat means the file
        // compresses via long-range matching — it must NOT be STOREd.
        let half = 4 * 1024 * 1024usize;
        let mut data = pseudo_random(half, 42);
        data.extend_from_slice(&data.clone());
        assert!(
            !sample_is_incompressible(&data, 3),
            "distant copy must not be probed as incompressible"
        );
    }

    #[test]
    fn probe_stores_pure_random() {
        let data = pseudo_random(8 * 1024 * 1024, 7);
        assert!(
            sample_is_incompressible(&data, 3),
            "pure random must be probed as incompressible"
        );
    }

    #[test]
    fn probe_leaves_compressible_data_alone() {
        // Text-like data compresses far below the 90% threshold.
        let mut data = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .copied()
            .cycle()
            .take(8 * 1024 * 1024)
            .collect::<Vec<u8>>();
        data.extend_from_slice(&data.clone());
        assert!(!sample_is_incompressible(&data, 3));
    }
}
