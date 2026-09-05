//! Compare packed sizes (ratio) and wall time of several synthetic
//! corpora (text, mixed, xml, sparse-copies) across compression levels —
//! the regression guard for collector/parse changes: ratios must stay put
//! while speed may only improve.
//!
//! Run: cargo run --release --features parallel --example ratiocheck [mt_threads]
use std::time::Instant;

fn lcg(seed: &mut u64) -> u8 {
    *seed ^= *seed >> 12;
    *seed ^= *seed << 25;
    *seed ^= *seed >> 27;
    ((*seed).wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
}

fn corpora() -> Vec<(&'static str, Vec<u8>)> {
    let mut out = Vec::new();

    // Text: repeated paragraphs with a rotating word set, so 4-byte
    // windows repeat heavily (real text-like compressibility).
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
    let mut text = String::with_capacity(6 * 1024 * 1024);
    for _ in 0..60_000 {
        let n = 8 + (lcg(&mut seed) as usize % 16);
        for _ in 0..n {
            text.push_str(words[lcg(&mut seed) as usize % words.len()]);
            text.push(' ');
        }
        text.push('\n');
    }
    out.push(("text", text.into_bytes()));
    let text_bytes = out[0].1.clone();

    // Mixed: text, then random, then text again, then structured binary.
    let mut mixed = Vec::with_capacity(8 * 1024 * 1024);
    mixed.extend_from_slice(&text_bytes[..2 * 1024 * 1024]);
    let mut seed = 777u64;
    for _ in 0..4 * 1024 * 1024 {
        mixed.push(lcg(&mut seed));
    }
    mixed.extend_from_slice(&text_bytes[..2 * 1024 * 1024]);
    // Structured binary: periodic 3-byte patterns in random noise.
    let mut bin = Vec::with_capacity(4 * 1024 * 1024);
    let mut seed = 99u64;
    for i in 0..4 * 1024 * 1024 {
        if i % 97 < 64 {
            bin.push((i % 251) as u8);
        } else {
            bin.push(lcg(&mut seed));
        }
    }
    mixed.extend_from_slice(&bin);
    out.push(("mixed", mixed));

    // XML-ish: tags around repetitive text with some random attribute
    // values.
    let mut seed = 555u64;
    let mut xml = String::with_capacity(4 * 1024 * 1024);
    for i in 0..40_000 {
        xml.push_str("<item id=\"");
        xml.push_str(&(i % 1000).to_string());
        xml.push_str("\" attr=\"");
        xml.push_str(words[(lcg(&mut seed) as usize) % words.len()]);
        xml.push_str("\">");
        xml.push_str(words[(lcg(&mut seed) as usize) % words.len()]);
        xml.push_str("</item>\n");
    }
    out.push(("xml", xml.into_bytes()));

    // Mostly-random with sparse self-copies: the fast-mode threshold
    // matters here (long miss runs interrupted by real matches).
    let mut seed = 31415u64;
    let mut sparse = Vec::with_capacity(8 * 1024 * 1024);
    for _ in 0..16 {
        let base = sparse.len();
        for _ in 0..(400 * 1024) {
            sparse.push(lcg(&mut seed));
        }
        // Copy a 64 KiB window from 1 MiB earlier.
        if base >= 1024 * 1024 {
            let src = base - 1024 * 1024;
            let copy = sparse[src..src + 64 * 1024].to_vec();
            sparse.extend_from_slice(&copy);
        }
    }
    out.push(("sparse-copies", sparse));

    out
}

fn main() {
    let threads: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1".into())
        .parse()
        .unwrap();
    let data = corpora();
    for (name, corpus) in &data {
        for level in [2u8, 3, 5] {
            let t0 = Instant::now();
            let mut seed = rar_rs::EncoderState::default();
            let packed = rar_rs::encode_chunked_mt(
                corpus,
                level,
                6,
                4 * 1024 * 1024,
                &mut seed,
                threads,
                true,
                rar_rs::ArchiveVersion::Rar50,
            );
            let ms = t0.elapsed().as_millis();
            let ratio = packed.len() as f64 * 100.0 / corpus.len() as f64;
            println!(
                "{name:<14} l{level} mt{threads}: {:>7.2}%  {:>5} ms  ({} -> {})",
                ratio,
                ms,
                corpus.len(),
                packed.len()
            );
        }
    }
}
