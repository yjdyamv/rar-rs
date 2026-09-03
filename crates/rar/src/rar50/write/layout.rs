//! Dictionary sizing and compressibility probing (write-side layout
//! policy): WinRAR-compatible `-md` semantics and the STORE fallback probe.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::codec::lzss_huff;
use crate::error::{RarError, RarResult};
use crate::io_util::read_up_to;

/// WinRAR 7.23 dictionary selection for a non-solid member: the requested
/// dictionary (`-md`, or the default 32 MiB at every compression level) is
/// capped at twice the file size rounded down to a power of two, floored at
/// 128 KiB, and clamped to the RAR5 range (128 KiB .. 4 GiB, log 0..15).
fn dict_log_for(data_size: usize, requested: Option<u8>, _level: u8) -> u8 {
    let base = 128 * 1024;
    let file_pow2 = 1usize << (usize::BITS - 1 - data_size.max(1).leading_zeros());
    let auto_cap = (file_pow2 * 2).max(base);
    let requested_bytes = requested.map_or(32 * 1024 * 1024, |log| base << log);
    let target = auto_cap.min(requested_bytes);
    let mut log = 0u8;
    while (base << log) < target && log < 15 {
        log += 1;
    }
    log
}

/// WinRAR 7.23 dictionary selection for one member, covering both RAR5
/// (v50) and RAR7 (v70) creation. Returns `(encoder_window_log,
/// header_dict_bytes)`:
///
/// - `header_dict_bytes = None`: a plain RAR5 member; the log drives both
///   the header `comp_dict_size` field and the encoder window.
/// - `header_dict_bytes = Some(b)`: a RAR7 member whose header declares an
///   actual dictionary of `b` bytes (possibly not a power of two, WinRAR's
///   `-md` above 4 GiB). The encoder window stays bounded — match
///   distances are chunk-limited anyway — only the header declares the
///   large dictionary.
///
/// Like WinRAR, a > 4 GiB request is still capped at twice the file size
/// rounded down to a power of two; when the cap lands in the RAR5 range
/// the member is written as plain v50 with the capped log. `force_v70`
/// overrides that downgrade (the format allows v70 with any dictionary;
/// it is the test seam that runs the v70 paths at small scale).
pub(crate) fn dict_params_for(
    data_size: usize,
    requested_log: Option<u8>,
    requested_bytes: Option<u64>,
    level: u8,
    force_v70: bool,
) -> (u8, Option<u64>) {
    let Some(requested) = requested_bytes else {
        return (dict_log_for(data_size, requested_log, level), None);
    };
    let base = 128 * 1024;
    let file_pow2 = 1usize << (usize::BITS - 1 - data_size.max(1).leading_zeros());
    let auto_cap = file_pow2.saturating_mul(2).max(base);
    let capped = (requested as usize).min(auto_cap);
    if force_v70 || capped as u64 > 4 * 1024 * 1024 * 1024u64 {
        // RAR7 (v70): the header declares the (capped) dictionary — floored
        // at 128 KiB, the smallest the 5+5-bit encoding can represent. The
        // encoder window follows the plain RAR5 selection rules but is
        // clamped to the declared dictionary: the decoder window IS the
        // declared dict, so emitting a longer distance would be
        // undecodable. (The > 4 GiB path never tripped this because the
        // RAR5 log ceiling of 4 GiB is already below any v70 dict; the
        // clamp matters for force_v70's small dicts.)
        let declared = capped.max(base) as u64;
        let window_log = dict_log_for(data_size, requested_log, level)
            .min((63 - (declared / base as u64).leading_zeros()) as u8);
        (window_log, Some(declared))
    } else {
        // The 2x-file-size cap fell into the RAR5 range: plain v50 member
        // with the capped dictionary.
        let mut log = 0u8;
        while (base << log) < capped && log < 15 {
            log += 1;
        }
        (log, None)
    }
}

/// Sample-probe large inputs for incompressibility.
///
/// Compressing small samples with the same method costs ~20 ms per sample
/// and reliably identifies media/archives/random data, which would
/// otherwise spend minutes in the match finder only to end up STORE
/// anyway. The 90% threshold is conservative: genuinely compressible
/// inputs (text, code, structured binary) compress the samples far below
/// it. Sampling the head plus quarter points catches files whose tails are
/// incompressible (e.g. text + random media), which the old head-only
/// probe missed. A file is only STOREd when at least half of the samples
/// are incompressible, so files with a small random section keep
/// compressing.
pub(crate) const SAMPLE_PROBE_HEAD: usize = 512 * 1024;
const SAMPLE_PROBE_TAIL: usize = 256 * 1024;
const SAMPLE_REPEAT_STEP: usize = 16;
const SAMPLE_REPEAT_MIN_MATCH: usize = 64;

/// In-memory stride probe (used by `add_bytes`).
pub(crate) fn sample_is_incompressible(data: &[u8], method: u8) -> bool {
    if data.len() < 4 * SAMPLE_PROBE_HEAD {
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
    use std::collections::HashMap;
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

/// Compute the plaintext CRC32 (and optional BLAKE2sp) of a file in a
/// single streaming pass.
pub(crate) fn hash_file(
    path: &Path,
    size: u64,
    want_blake: bool,
) -> RarResult<(u32, Option<[u8; 32]>)> {
    let mut crc = crc32fast::Hasher::new();
    let mut blake = want_blake.then(crate::rar50::blake2sp::Hasher::new);
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        crc.update(&buf[..n]);
        if let Some(h) = blake.as_mut() {
            h.update(&buf[..n]);
        }
    }
    if total != size {
        return Err(RarError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("file changed size while being hashed: expected {size} bytes, read {total}"),
        )));
    }
    Ok((crc.finalize(), blake.map(|h| h.finalize())))
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
