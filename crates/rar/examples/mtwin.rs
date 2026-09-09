//! Near-window reach probe for the RAR5 MT encoder.
//!
//! The multi-threaded path parses each slice with the low-step chain tier
//! against a tail capped at `NEAR_WINDOW_MAX` (8 MiB, as in the sequential
//! path): the slice's near window is the combined tail+slice frame the
//! chain finder seeds. This benchmark measures what the far band of that
//! window (2-8 MiB backward depth) contributes in ratio and what a wider
//! reach costs in speed, on corpora that place real matches there — and
//! how the low-step MT output diverges from the sequential bytes at each
//! thread count.
//!
//! Run: cargo run --release --features parallel --example mtwin [size_mb]
use std::time::Instant;

fn lcg(seed: &mut u64) -> u8 {
    *seed ^= *seed >> 12;
    *seed ^= *seed << 25;
    *seed ^= *seed >> 27;
    ((*seed).wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
}

/// Repetitive prose: matches land at all depths, but mostly shallow.
fn text_data(target: usize) -> Vec<u8> {
    use std::fmt::Write;
    let words = [
        "the",
        "quick",
        "brown",
        "fox",
        "jumps",
        "over",
        "lazy",
        "dog",
        "lorem",
        "ipsum",
        "dolor",
        "sit",
        "amet",
        "consectetur",
        "adipiscing",
        "elit",
        "sed",
        "do",
        "eiusmod",
        "tempor",
        "incididunt",
        "ut",
        "labore",
        "et",
        "dolore",
        "magna",
        "aliqua",
    ];
    let mut seed = 12345u64;
    let mut out = String::with_capacity(target);
    while out.len() < target {
        let n = 8 + (lcg(&mut seed) as usize % 16);
        for _ in 0..n {
            out.write_fmt(format_args!(
                "{} ",
                words[lcg(&mut seed) as usize % words.len()]
            ))
            .unwrap();
        }
        out.push('\n');
    }
    out.truncate(target);
    out.into_bytes()
}

/// Random noise interrupted by exact copies of windows from 1/2/4/6 MiB in
/// the past — the far band of the 8 MiB near window (within the chain
/// finder's lookbehind), which would otherwise ride the sampled long-range
/// table.
///
/// Copy-heavy on purpose: fresh random bytes are the expensive arm of the
/// sequential parse (which has no incompressible skip), so random is kept
/// to a short guard between copy runs — the benchmark stays quick while
/// the distant-copy band is exercised on every cycle.
fn distant_data(target: usize) -> Vec<u8> {
    const DEPTHS: [usize; 4] = [
        1024 * 1024,
        2 * 1024 * 1024,
        4 * 1024 * 1024,
        6 * 1024 * 1024,
    ];
    let mut seed = 0xC0FFEEu64;
    let mut out = Vec::with_capacity(target);
    while out.len() < target {
        for _ in 0..64 * 1024 {
            if out.len() >= target {
                break;
            }
            out.push(lcg(&mut seed));
        }
        for depth in DEPTHS {
            if out.len() < depth + 256 * 1024 || out.len() >= target {
                break;
            }
            let src = out.len() - depth;
            let len = 256 * 1024;
            let limit = len.min(target - out.len());
            let copy: Vec<u8> = out[src..src + limit].to_vec();
            out.extend_from_slice(&copy);
        }
    }
    out.truncate(target);
    out
}

/// The case the near-window reach governs: a compressible lookbehind
/// (patterned header at each window boundary keeps the incompressible probe
/// quiet) plus exact copies reach 2/4/6 MiB back. Those copies are inside
/// the 8 MiB near-window tail the low-step chain finder seeds; the copies
/// anchor on the seeded tail positions instead of the sampled long-range
/// table.
fn blockdup_data(target: usize) -> Vec<u8> {
    let mut seed = 0xBEEFCAFEu64;
    let mut out = Vec::with_capacity(target);
    while out.len() < target {
        // Patterned header — compressible tail head for the seed probe.
        for i in 0..256 * 1024 {
            if out.len() >= target {
                break;
            }
            out.push((i as u8).wrapping_mul(31).wrapping_add(7));
        }
        for depth in [2usize * 1024 * 1024, 4 * 1024 * 1024, 6 * 1024 * 1024] {
            if out.len() >= target {
                break;
            }
            for _ in 0..128 * 1024 {
                if out.len() >= target {
                    break;
                }
                out.push(lcg(&mut seed));
            }
            if out.len() < depth + 128 * 1024 || out.len() >= target {
                continue;
            }
            let src = out.len() - depth;
            let limit = (128 * 1024).min(target - out.len());
            let copy: Vec<u8> = out[src..src + limit].to_vec();
            out.extend_from_slice(&copy);
        }
    }
    out.truncate(target);
    out
}

/// A real on-disk sample (e.g. an x86 executable), read up to `target`
/// bytes. The corpus the DLL-class parse speed was measured on.
fn file_data(path: &str, target: usize) -> Vec<u8> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mut out = Vec::with_capacity(target);
    let mut buf = [0u8; 65536];
    while out.len() < target {
        let n = f
            .read(&mut buf)
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out.truncate(target);
    out
}

fn main() {
    let size_mb: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "24".into())
        .parse()
        .unwrap();
    let size = size_mb * 1024 * 1024;
    const DICT_LOG: u8 = 7; // 16 MiB
    let want_corpus = std::env::args().nth(2).unwrap_or_default();
    let thread_csv = std::env::args().nth(3).unwrap_or_else(|| "1,8".into());
    let thread_list = thread_csv
        .split(',')
        .map(|s| s.trim().parse::<usize>().expect("thread list"))
        .collect::<Vec<_>>();
    let mut corpora = [
        ("text", text_data(size)),
        ("distant", distant_data(size)),
        ("blockdup", blockdup_data(size)),
    ]
    .into_iter()
    .filter(|(name, _)| want_corpus.is_empty() || *name == want_corpus)
    .collect::<Vec<_>>();
    if want_corpus == "file" {
        let path = std::env::args()
            .nth(4)
            .unwrap_or_else(|| panic!("file corpus needs a path arg"));
        corpora = vec![("file", file_data(&path, size.max(1024 * 1024)))];
    }

    for (name, corpus) in &corpora {
        println!(
            "== {name}: {} MiB, dict 2^{DICT_LOG} ==",
            corpus.len() / (1 << 20)
        );
        println!();
        for level in [3u8, 5] {
            // Sequential baseline: persistent tree, cache carried across the
            // whole member.
            eprintln!("[{name} l{level}] seq ...");
            let t0 = Instant::now();
            let opts = rar_rs::EncodeOptions::new(level, DICT_LOG);
            let packed = rar_rs::encode_chunked(corpus, opts).expect("sequential encode failed");
            let seq_ms = t0.elapsed().as_millis();
            let seq_ratio = packed.len() as f64 * 100.0 / corpus.len() as f64;
            let seq_bytes = packed.len();

            // Multi-threaded sweep (default 1 and 8 threads).
            let mut pair = Vec::new();
            for threads in &thread_list {
                eprintln!("[{name} l{level}] mt{threads} ...");
                let t1 = Instant::now();
                let mut seed = rar_rs::EncoderState::default();
                let packed = rar_rs::encode_chunked_mt(
                    corpus,
                    level,
                    DICT_LOG,
                    4 * 1024 * 1024,
                    &mut seed,
                    *threads,
                    true,
                    rar_rs::ArchiveVersion::V50,
                );
                let mt_ms = t1.elapsed().as_millis();
                let mt_ratio = packed.len() as f64 * 100.0 / corpus.len() as f64;
                let out = rar_rs::decode(&packed, level, corpus.len() as u64, DICT_LOG, None)
                    .unwrap_or_else(|e| panic!("mt {threads} decode: {e:?}"));
                assert_eq!(out, *corpus, "mt{threads} decode mismatch");
                pair.push((*threads, mt_ms, mt_ratio, packed.len()));
            }

            let (first_ms, first_ratio, first_bytes) = (pair[0].1, pair[0].2, pair[0].3);
            let d1_at = first_bytes as isize - seq_bytes as isize;
            println!(
                "  l{level}: seq {:>6}ms {:>7.2}% ({}) | mt{} {:>6}ms {:>7.2}% ({} {:+})",
                seq_ms, seq_ratio, seq_bytes, pair[0].0, first_ms, first_ratio, first_bytes, d1_at,
            );
            for (threads, mt_ms, mt_ratio, mt_bytes) in pair.iter().skip(1) {
                println!(
                    "        mt{threads} {:>6}ms {:>7.2}% ({} {:+})   x{:.2} vs seq",
                    mt_ms,
                    mt_ratio,
                    mt_bytes,
                    *mt_bytes as isize - seq_bytes as isize,
                    seq_ms as f64 / *mt_ms as f64,
                );
            }
        }
    }
}
