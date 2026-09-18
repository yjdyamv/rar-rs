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
//! A cheap structural screen ([`samples_look_structured`]) runs first and
//! skips those encodes for members that are clearly compressible — the common
//! case, where the samples cost 18-28% of the member's own encode time and
//! cannot change the verdict anyway.
//! The 90% threshold is conservative: genuinely compressible inputs (text,
//! code, structured binary) compress the samples far below it. Sampling the
//! head plus quarter points catches files whose tails are incompressible
//! (e.g. text + random media), which a head-only probe misses. Only inputs
//! where at least half of the samples are incompressible are declared
//! incompressible, so files with a small random section keep compressing.

use std::collections::{HashMap, HashSet};
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

/// Stride used when sampling 4-byte windows for the structural screen.
const SCREEN_STRIDE: usize = 16;
/// Sampled windows below this count cannot judge a region (64 KiB of input).
const SCREEN_MIN_WINDOWS: usize = 4096;
/// A region counts as structured when at most this share of its sampled
/// 4-byte windows are distinct: repeated windows at a fixed stride are what a
/// match finder turns into matches, while random and already-compressed data
/// keep them all distinct.
const SCREEN_DISTINCT_PERCENT: usize = 95;

/// Share of distinct 4-byte windows sampled every [`SCREEN_STRIDE`] bytes, or
/// `None` when the region is too short to judge (fewer than
/// [`SCREEN_MIN_WINDOWS`] windows). Windows are compared raw (no hashing), so
/// the count is exact: random input measures ~100%, and anything with real
/// repeats — text, code, structured binary, even base64/hex whose alphabet
/// cycles within a window — stays well below.
pub(crate) fn distinct_window_percent(region: &[u8]) -> Option<usize> {
    let mut seen = HashSet::with_capacity(region.len() / SCREEN_STRIDE + 1);
    let mut windows = 0usize;
    let mut off = 0usize;
    while off + 4 <= region.len() {
        let value = u32::from_le_bytes([
            region[off],
            region[off + 1],
            region[off + 2],
            region[off + 3],
        ]);
        seen.insert(value);
        windows += 1;
        off += SCREEN_STRIDE;
    }
    (windows >= SCREEN_MIN_WINDOWS).then(|| seen.len() * 100 / windows)
}

/// Cheap structural screen over the probe's own sample regions: `true` when
/// *every* region shows repeated 4-byte windows, i.e. the member clearly has
/// structure to compress. A region too short to judge blocks the screen.
///
/// It exists to skip the sample encodes below (four encodes of up to 512 KiB
/// each, at the member's own method — measured at 18-28% of a 4 MiB
/// compressible member's encode time). Skipping only ever removes a probe that
/// could have forced STORE, so the archive either keeps its bytes or gets
/// smaller, never larger; a screen that misses a structured region costs the
/// old sample encodes, and one that calls incompressible data structured costs
/// a full parse that still falls back to STORE. Both are time only.
///
/// The window test separates the cases order-0 entropy cannot: base64 of random
/// data sits at ~6 bits/byte like real DLLs, but its windows are all distinct.
pub(crate) fn samples_look_structured(samples: &[&[u8]]) -> bool {
    // Test seam: the byte-identity contract is "the screen may only skip a
    // probe whose verdict is `false`", so tests re-run the same inputs with
    // the screen disabled and compare verdicts (and whole archives).
    #[cfg(test)]
    if SCREEN_DISABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    !samples.is_empty()
        && samples.iter().all(|sample| {
            distinct_window_percent(sample).is_some_and(|percent| percent < SCREEN_DISTINCT_PERCENT)
        })
}

#[cfg(test)]
static SCREEN_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The screen switch is process-global, so the tests that read or flip it must
/// not run next to each other (the same reason the napi `set_var` tests take a
/// lock). The probe's verdicts do not depend on it, so no other test needs it.
#[cfg(test)]
pub(crate) fn screen_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Test seam: turn the cheap screen off, so the sample encodes run exactly as
/// they did before it existed.
#[cfg(test)]
pub(crate) fn set_screen_enabled(enabled: bool) {
    SCREEN_DISABLED.store(!enabled, std::sync::atomic::Ordering::Relaxed);
}

/// In-memory stride probe (used by `add_bytes` and the codec entry points).
pub(crate) fn sample_is_incompressible(data: &[u8], method: u8) -> bool {
    if data.len() < SAMPLE_PROBE_MIN_INPUT {
        return false;
    }
    let mut samples: Vec<&[u8]> = Vec::with_capacity(3);
    let mut voting: Vec<&[u8]> = Vec::with_capacity(3);
    for &pos in &[data.len() / 4, data.len() / 2, data.len() * 3 / 4] {
        if pos + SAMPLE_PROBE_TAIL <= data.len() {
            samples.push(&data[pos..pos + SAMPLE_PROBE_TAIL]);
            // A quarter sample that overlaps the head adds no information.
            if pos >= SAMPLE_PROBE_HEAD {
                voting.push(&data[pos..pos + SAMPLE_PROBE_TAIL]);
            }
        }
    }
    // Cheap screen first: a member whose every sample shows structure is
    // compressible, so the sample encodes below cannot change the verdict and
    // their cost is pure waste.
    let mut regions = Vec::with_capacity(samples.len() + 1);
    regions.push(&data[..SAMPLE_PROBE_HEAD]);
    regions.extend(samples.iter().copied());
    if samples_look_structured(&regions) {
        return false;
    }

    let mut bad = 0;
    if incompressible_sample(&data[..SAMPLE_PROBE_HEAD], method) {
        bad += 1;
    }
    for sample in &voting {
        if incompressible_sample(sample, method) {
            bad += 1;
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
    let mut samples: Vec<Vec<u8>> = Vec::new();
    let mut voting: Vec<Vec<u8>> = Vec::new();
    for &quarter in &[size / 4, size / 2, size * 3 / 4] {
        if quarter < SAMPLE_PROBE_HEAD as u64 {
            continue;
        }
        f.seek(SeekFrom::Start(quarter))?;
        let mut sample = vec![0u8; SAMPLE_PROBE_TAIL];
        let n = read_up_to(&mut f, &mut sample)?;
        if n > 0 {
            samples.push(sample[..n].to_vec());
            voting.push(sample[..n].to_vec());
        }
    }

    // Same cheap screen as the in-memory probe: every sample structured means
    // the encodes cannot change the verdict.
    {
        let mut regions: Vec<&[u8]> = Vec::with_capacity(samples.len() + 1);
        regions.push(&head[..n]);
        regions.extend(samples.iter().map(|sample| sample.as_slice()));
        if samples_look_structured(&regions) {
            return Ok(false);
        }
    }

    let mut bad = 0;
    if incompressible_sample(&head[..n], method) {
        bad += 1;
    }
    for sample in &voting {
        if incompressible_sample(sample, method) {
            bad += 1;
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

    /// Structured corpora for the screen tests: text with a small vocabulary,
    /// x86-shaped code, and markup.
    fn structured_corpora() -> Vec<(&'static str, Vec<u8>)> {
        let words = [
            "the", "quick", "brown", "fox", "archive", "window", "match", "price",
        ];
        let mut state = 0x1234_5678u64;
        let mut text = Vec::with_capacity(4 << 20);
        while text.len() < 4 << 20 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            text.extend_from_slice(words[(state >> 33) as usize % words.len()].as_bytes());
            text.push(b' ');
        }

        let mut code = Vec::with_capacity(4 << 20);
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        while code.len() < 4 << 20 {
            match pseudo_random(1, state)[0] % 5 {
                0 => {
                    code.push(0xE8);
                    let target = 0x40_0000u32.wrapping_sub(code.len() as u32);
                    code.extend_from_slice(&target.to_le_bytes());
                }
                1 => code.extend_from_slice(&[0x90; 3]),
                2 => code.extend_from_slice(&[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x20]),
                _ => code.push(pseudo_random(1, state)[0]),
            }
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        }

        let mut xml = Vec::with_capacity(4 << 20);
        let mut index = 0u32;
        while xml.len() < 4 << 20 {
            xml.extend_from_slice(
                format!(
                    "<item id=\"{index}\"><name>element {}</name></item>\n",
                    index % 97
                )
                .as_bytes(),
            );
            index += 1;
        }

        vec![("text", text), ("code", code), ("xml", xml)]
    }

    /// The screen must fire on structured data, and stay quiet on data whose
    /// sample encodes would call it incompressible — firing there would cost
    /// the full parse the probe exists to avoid.
    #[test]
    fn structural_screen_separates_structured_from_random() {
        let _lock = screen_test_lock();
        for (tag, data) in structured_corpora() {
            let regions: Vec<&[u8]> = vec![&data[..SAMPLE_PROBE_HEAD]];
            assert!(
                samples_look_structured(&regions),
                "{tag} must screen as structured"
            );
        }

        let random = pseudo_random(4 << 20, 7);
        let regions: Vec<&[u8]> = vec![&random[..SAMPLE_PROBE_HEAD]];
        assert!(
            !samples_look_structured(&regions),
            "random data must not screen as structured"
        );

        // Base64 of random bytes sits at ~6 bits/byte, like a real DLL, but it
        // has no repeated windows: entropy alone cannot separate the two.
        let raw = pseudo_random(1 << 20, 11);
        let b64 = base64(&raw);
        let regions: Vec<&[u8]> = vec![&b64[..SAMPLE_PROBE_HEAD]];
        assert!(
            !samples_look_structured(&regions),
            "base64 of random data must not screen as structured"
        );
    }

    /// The contract that makes the screen safe: whenever it fires, the sample
    /// encodes (run as they were before the screen existed) also call the
    /// member compressible, so skipping them cannot change the archive.
    #[test]
    fn structural_screen_only_skips_probes_that_would_say_compressible() {
        let _lock = screen_test_lock();
        for (tag, data) in structured_corpora() {
            let regions: Vec<&[u8]> = vec![&data[..SAMPLE_PROBE_HEAD]];
            assert!(
                samples_look_structured(&regions),
                "{tag} must screen as structured"
            );

            set_screen_enabled(false);
            let raw = sample_is_incompressible(&data, 3);
            set_screen_enabled(true);
            assert!(
                !raw,
                "{tag}: the screen fired, so the raw probe must also say compressible"
            );
            assert!(!sample_is_incompressible(&data, 3));
        }

        // And the screen must not change the incompressible verdicts either.
        let random = pseudo_random(4 << 20, 7);
        assert!(sample_is_incompressible(&random, 3));
        set_screen_enabled(false);
        let raw = sample_is_incompressible(&random, 3);
        set_screen_enabled(true);
        assert!(raw);
    }

    /// The screen is a pure optimization: for a structured member the codec's
    /// bytes are identical with the screen on and off, because the sample
    /// encodes it skips would have cleared the member anyway. (This replaces an
    /// archive-level version of the same check, which made `src/archive/tests.rs`
    /// reach into this module — forbidden by the layer boundaries.)
    #[test]
    fn structural_screen_keeps_codec_bytes_identical() {
        let _lock = screen_test_lock();
        let (_, data) = structured_corpora().remove(0);
        set_screen_enabled(true);
        let screened = crate::codec::lzss_huff::encode_chunked(
            &data,
            crate::codec::lzss_huff::EncodeOptions::new(3, 8),
        )
        .unwrap();
        set_screen_enabled(false);
        let raw = crate::codec::lzss_huff::encode_chunked(
            &data,
            crate::codec::lzss_huff::EncodeOptions::new(3, 8),
        )
        .unwrap();
        set_screen_enabled(true);
        assert!(
            screened.len() < data.len() / 2,
            "the member must compress, else the screen never mattered"
        );
        assert_eq!(
            screened,
            raw,
            "the screen must not change the codec's bytes ({} vs {})",
            screened.len(),
            raw.len()
        );
    }

    /// Minimal standard base64 (test-only): enough to build a high-entropy
    /// text corpus without a dependency.
    fn base64(data: &[u8]) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let triple = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(ALPHABET[(triple >> 18) as usize & 0x3F]);
            out.push(ALPHABET[(triple >> 12) as usize & 0x3F]);
            out.push(if chunk.len() > 1 {
                ALPHABET[(triple >> 6) as usize & 0x3F]
            } else {
                b'='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[triple as usize & 0x3F]
            } else {
                b'='
            });
        }
        out
    }
}
