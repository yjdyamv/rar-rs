// Source for the dense x86-64 test corpus.
//
// `scripts/gen-x86-fixture.sh` compiles this file and keeps the first
// 129,400 bytes of the resulting PE — the headers plus roughly 128 KiB of
// register-register code with disp32 operands. That prefix is what
// `crates/rar/tests/data/dense-x86-prefix.bin` holds, and what the
// `dense_x86_multi_chunk_roundtrips_byte_identical` regression in
// `crates/rar/tests/rar50_roundtrip.rs` encodes and decodes.
//
// Why a compiled binary rather than random bytes: the multi-chunk match
// finder corruption only showed up on real machine code — small-dictionary
// chunks over a stream that repeats instruction shapes at many distances,
// which is exactly what a compiler emits. Hand-written filler is either too
// random to produce long matches or too regular to look like code.
//
// The bytes are generated, not copied: the corpus is this file's own
// compilation, so it needs no third-party entry beyond the one that records
// how it is produced (see `## Test data` in THIRD_PARTY_LICENSES.md).

#![allow(clippy::needless_range_loop)]

/// A ring of records with mixed widths, so the compiler emits disp32
/// addressing on every field access.
struct Ring {
    key: u64,
    len: u32,
    tag: u16,
    stamp: i64,
    next: u32,
    prev: u32,
}

#[inline(always)]
fn step(ring: &mut [Ring], i: usize) -> u64 {
    let mut acc = ring[i].key;
    acc = acc.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let j = (acc as usize) & (ring.len() - 1);
    ring[i].stamp = acc as i64;
    ring[i].next = j as u32;
    ring[i].len = ring[j].len.wrapping_add(1);
    ring[j].prev = i as u32;
    ring[j].tag = (acc as u16) ^ ring[i].tag;
    acc ^ (acc >> 29)
}

/// Width switches keep several instruction shapes alive (movzx/movsx, shifts,
/// imul with small immediates) instead of a single tight loop.
#[inline(never)]
fn width_mix(v: u64, tag: u16) -> u64 {
    match tag & 7 {
        0 => v.wrapping_add(0x1234_5678),
        1 => v.wrapping_mul(31),
        2 => v << 7,
        3 => v >> 3,
        4 => v.rotate_left(13),
        5 => (v as u32 as u64).wrapping_mul(0x9E37_79B9),
        6 => v ^ 0xAAAA_BBBB_CCCC_DDDD,
        _ => v.wrapping_sub(0x5555_5555_5555_5555),
    }
}

/// A second access pattern over the same array: strided reads plus a copy,
/// which is where long repeats at distant offsets come from.
#[inline(never)]
fn gather(ring: &[Ring], out: &mut [u64]) {
    let stride = 3usize;
    let mut acc = 0u64;
    let mut i = 0;
    while i + stride * 4 < ring.len() {
        let a = &ring[i];
        let b = &ring[i + stride];
        let c = &ring[i + stride * 2];
        let d = &ring[i + stride * 3];
        acc = acc
            .wrapping_add(a.key)
            .wrapping_add((b.len as u64) << 9)
            .wrapping_add(c.stamp as u64)
            .wrapping_add((d.tag as u64) << 41);
        out[i & (out.len() - 1)] = acc;
        i += 1;
    }
}

fn main() {
    let n = 4096;
    let mut ring: Vec<Ring> = (0..n)
        .map(|i| Ring {
            key: i as u64 * 0x10001,
            len: i as u32,
            tag: i as u16,
            stamp: 0,
            next: 0,
            prev: 0,
        })
        .collect();
    let mut acc = 0u64;
    for i in 0..n * 4 {
        let idx = i & (n - 1);
        acc = acc.wrapping_add(step(&mut ring, idx));
        acc = width_mix(acc, ring[idx].tag);
    }
    let mut out = vec![0u64; n];
    for _ in 0..8 {
        gather(&ring, &mut out);
    }
    println!("{}", acc ^ out[0]);
}