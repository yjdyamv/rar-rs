//! P1 gate for ADR 0011: what does a per-block hash precompute pass cost?
//!
//! The official matcher lays down 4/5-byte and long hashes for a whole block
//! before it descends. Before building that, measure the pass on its own: if a
//! plain sweep (load 8 bytes, mix, store a u32) already costs a large share of
//! `collect`, the design is dead on arrival and no child-array work is worth
//! starting.
//!
//! Usage: `hashprobe <file> [repeats]`

use std::hint::black_box;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: hashprobe <file> [repeats]")?;
    let repeats: usize = args
        .next()
        .map(|raw| raw.parse())
        .transpose()?
        .unwrap_or(3);
    let data = std::fs::read(&path)?;
    let n = data.len().saturating_sub(8);

    // One u32 per position: the hash stream a descent would read back.
    let mut hashes = vec![0u32; n];
    let mut longs = vec![0u32; n];

    let mut best = u128::MAX;
    for _ in 0..repeats {
        let started = Instant::now();
        for (pos, slot) in hashes.iter_mut().enumerate() {
            let word = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            let mixed = word.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            *slot = (mixed >> 40) as u32;
            // Second table: a coarser long hash, as the official matcher keeps.
            let long = (word as u32).wrapping_mul(0x2773) & 0x3fff_ffff;
            longs[pos] = long ^ ((mixed >> 20) as u32);
        }
        let elapsed = started.elapsed().as_micros();
        best = best.min(elapsed);
        black_box((&hashes[..n / 2], &longs[..n / 2]));
    }

    let per_pos = best as f64 / n as f64;
    println!(
        "precompute {n} positions: {} ms ({} ns/position, 2 tables, {} KiB written)",
        best / 1000,
        (per_pos * 1000.0).round() / 1000.0,
        n * 8 / 1024
    );
    Ok(())
}
