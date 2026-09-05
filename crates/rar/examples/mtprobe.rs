//! Probe the MT encode cost on random (incompressible) data.
//!
//! On data with no real matches the optimal parse still walks the tree
//! (cache-missing son array) and encodes literal-only blocks, all wasted
//! when the member falls back to STORE. This benchmark times the full MT
//! encode of random data at several thread counts against a cheap
//! hashing+STORE baseline — a regression detector for the matchless fast
//! path and the collector's fast mode (both must keep this near the
//! baseline, not at the old full-parse cost).
//!
//! Run: cargo run --release --features parallel --example mtprobe <size_mb> <level>
use std::time::Instant;

fn random_data(size: usize) -> Vec<u8> {
    // Deterministic pseudo-random: no compressible structure at all.
    let mut out = Vec::with_capacity(size);
    let mut state = 0x9E3779B97F4A7C15u64;
    while out.len() < size {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        out.push((state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8);
    }
    out
}

fn main() {
    let size_mb: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "64".into())
        .parse()
        .unwrap();
    let level: u8 = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "3".into())
        .parse()
        .unwrap();
    let data = random_data(size_mb * 1024 * 1024);
    let mb = size_mb as f64;
    const DICT: u8 = 8;

    // Cheap baseline: hash + STORE (what the encoder falls back to when
    // compression is a net loss). This is the floor the MT encode should
    // approach if we skip the optimal parse on incompressible input.
    let t0 = Instant::now();
    let mut acc = 0u64;
    for chunk in data.chunks(65536) {
        // A cheap deterministic fold standing in for a hash: reads every
        // byte (the part that costs bandwidth) without doing any
        // match-finding.
        let mut h = 0x9E3779B97F4A7C15u64;
        for &b in chunk {
            h = h.wrapping_mul(31).wrapping_add(b as u64);
        }
        acc ^= h;
    }
    std::hint::black_box(acc);
    let base_ms = t0.elapsed().as_millis() as f64;
    println!(
        "baseline  hash+STORE {size_mb} MiB: {:>6.0} ms  {:>6.1} MiB/s",
        base_ms,
        mb / (base_ms / 1000.0),
    );

    // Full MT encode (optimal parse per slice) — the current cost.
    for threads in [1usize, 2, 4, 8, 16] {
        let t1 = Instant::now();
        let mut seed = rar_rs::EncoderState::default();
        let packed = rar_rs::encode_chunked_mt(
            &data,
            level,
            DICT,
            4 * 1024 * 1024,
            &mut seed,
            threads,
            true,
            rar_rs::ArchiveVersion::V50,
        );
        let mt_ms = t1.elapsed().as_millis() as f64;
        let ratio = packed.len() as f64 * 100.0 / data.len() as f64;
        let speedup = if mt_ms > 0.0 {
            base_ms / mt_ms
        } else {
            f64::NAN
        };
        println!(
            "mt{threads:>2} l{level} {size_mb} MiB: {:>6.0} ms  {:>6.1} MiB/s  ratio {:>6.2}%  (x{:.2} vs baseline)",
            mt_ms,
            mb / (mt_ms / 1000.0),
            ratio,
            speedup,
        );
        let out = rar_rs::decode(&packed, level, data.len() as u64, DICT, None).unwrap();
        assert_eq!(out.len(), data.len(), "mt length mismatch");
        assert_eq!(out, data, "mt decode mismatch");
    }
    println!("all MT outputs decode byte-identically");
}
