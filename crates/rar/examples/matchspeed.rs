//! Prototype matcher throughput probe (ADR 0009 follow-up).
//!
//! Not part of the encoder: it measures how fast a flat hash-table matcher
//! with 8-byte compares can walk a file, to bound what a kernel rewrite can
//! buy before we touch the production path.
//!
//! Usage: `cargo run --release --example matchspeed -- <file>`

use std::time::Instant;

const HASH_BITS: usize = 20;
const BUCKET: usize = 1 << HASH_BITS;
const MIN_MATCH: usize = 4;
const NICE: usize = 64;

#[inline(always)]
fn load8(data: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap())
}

#[inline(always)]
fn hash4(data: &[u8], pos: usize) -> usize {
    let v = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as u64;
    ((v.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> (64 - HASH_BITS)) as usize
}

/// Extend a match from `(cand, pos)` with 8-byte compares.
#[inline]
fn extend(data: &[u8], cand: usize, pos: usize, limit: usize) -> usize {
    let mut len = 0;
    while len + 8 <= limit {
        let a = load8(data, cand + len);
        let b = load8(data, pos + len);
        if a != b {
            return len + ((a ^ b).trailing_zeros() as usize) / 8;
        }
        len += 8;
    }
    while len < limit && data[cand + len] == data[pos + len] {
        len += 1;
    }
    len
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: matchspeed <file>");
    let data = std::fs::read(&path).unwrap();
    let n = data.len();
    let window = 8 * 1024 * 1024;

    let mut table = vec![u32::MAX; BUCKET];
    let mut pos = 0usize;
    let mut tokens = 0usize;
    let start = Instant::now();
    while pos + 8 <= n {
        let h = hash4(&data, pos);
        let cand = table[h];
        table[h] = pos as u32;
        let mut best_len = 0;
        let mut best_dist = 0;
        if cand != u32::MAX {
            let cand = cand as usize;
            let dist = pos - cand;
            if dist <= window && load8(&data, cand) == load8(&data, pos) {
                let limit = NICE.min(n - pos);
                let len = extend(&data, cand, pos, limit);
                if len >= MIN_MATCH {
                    best_len = len;
                    best_dist = dist;
                }
            }
        }
        if best_len >= MIN_MATCH {
            tokens += 1;
            // Insert the covered positions so later matches see them.
            for p in pos + 1..(pos + best_len).min(n.saturating_sub(3)) {
                table[hash4(&data, p)] = p as u32;
            }
            pos += best_len;
            let _ = best_dist;
        } else {
            tokens += 1;
            pos += 1;
        }
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{}: {:.1} MB in {:.0} ms ({:.0} MB/s), {} tokens",
        path,
        n as f64 / 1e6,
        ms,
        n as f64 / 1e6 / (ms / 1000.0),
        tokens
    );
}
