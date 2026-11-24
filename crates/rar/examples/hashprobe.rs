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
    let repeats: usize = args.next().map(|raw| raw.parse()).transpose()?.unwrap_or(3);
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
    let (in_block, whole) = probe_reads(&data, 64 * 1024, 1 << 20);
    println!(
        "reads: {in_block:.1} ns/lookup inside a 64 KiB block slice, {whole:.1} ns/lookup across the whole member"
    );
    println!(
        "precompute {n} positions: {} ms ({} ns/position, 2 tables, {} KiB written)",
        best / 1000,
        (per_pos * 1000.0).round() / 1000.0,
        n * 8 / 1024
    );
    Ok(())
}

/// Second P1 gate: after the stream is written, what does a descent-cost
/// *read* pattern cost — inside one 64 KiB block's slice (512 KiB of tables,
/// L2/L3 resident) versus against the whole-member table (108 MB, cold)?
fn probe_reads(data: &[u8], block: usize, lookups: usize) -> (f64, f64) {
    let n = data.len().saturating_sub(8);
    let mut hashes = vec![0u32; n];
    for (pos, slot) in hashes.iter_mut().enumerate() {
        let word = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        *slot = (word.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as u32;
    }
    let in_block = |spread: usize| {
        let start = Instant::now();
        let mut acc = 0u64;
        let mut cursor = 0x1234_5678u32;
        for _ in 0..lookups {
            cursor = cursor.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let pos = (cursor as usize % spread).min(n - 1);
            acc = acc.wrapping_add(hashes[pos] as u64);
        }
        black_box(acc);
        start.elapsed().as_nanos() as f64 / lookups as f64
    };
    (in_block(block.min(n)), in_block(n))
}
