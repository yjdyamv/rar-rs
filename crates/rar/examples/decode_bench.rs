//! Reproducible decode (extraction) baseline for the RAR5/RAR7 path.
//!
//! Why this exists: `perfbench`/`bench`/`clioverhead` all measure *creation*.
//! Every decode number quoted in `PLAN.md` ("RAR5（解压面）") came from ad-hoc
//! timing of a hand-built archive, which nobody can reproduce on a clean
//! checkout. This example builds its own archive from synthetic corpora with a
//! fixed seed, so two runs on two machines see byte-identical input — the CRC32
//! printed per corpus is the proof.
//!
//! The archive is written once, then re-opened and fully extracted `--reps`
//! times; the fastest run is reported, because decode throughput is dominated
//! by the codec and a mean would only add scheduler noise.
//!
//! Run:
//! ```text
//! cargo run --release --example decode_bench
//! cargo run --release --example decode_bench -- --reps 20
//! ```
use std::time::Instant;

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions};

/// xorshift32, so the corpus never depends on a random crate or the platform.
struct Rng(u32);

impl Rng {
    fn next_byte(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 >> 24) as u8
    }

    fn fill(&mut self, out: &mut Vec<u8>, len: usize) {
        out.reserve(len);
        for _ in 0..len {
            out.push(self.next_byte());
        }
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--keep` leaves the generated archives on disk, so the same bytes can be
    // handed to another tool (official UnRAR, for instance) for a like-for-like
    // comparison. The path is printed.
    let keep = args.iter().any(|a| a == "--keep");
    let reps = args
        .iter()
        .position(|a| a == "--reps")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10);

    // Three shapes that stress different parts of the decoder. Each is built to
    // land in a realistic ratio band: the printed packed size is the check — a
    // corpus that packs to nothing would measure the container, not the codec.
    let mut rng = Rng(0x2545_f491);

    // `text`: word salad over a generated vocabulary. Long literal runs broken
    // by short matches — the mix ordinary source and prose produce.
    let mut vocabulary: Vec<Vec<u8>> = Vec::new();
    for _ in 0..1500 {
        let len = 3 + (rng.0 as usize >> 8) % 9;
        let word: Vec<u8> = (0..len).map(|_| b'a' + (rng.next_byte() % 26)).collect();
        vocabulary.push(word);
    }
    let mut text = Vec::with_capacity(8 << 20);
    while text.len() < 8 << 20 {
        for _ in 0..12 {
            let index = (rng.0 as usize) % vocabulary.len();
            text.extend_from_slice(&vocabulary[index]);
            rng.next_byte();
            text.push(if rng.0 & 0x8000_0000 == 0 {
                b' '
            } else {
                b'\n'
            });
        }
    }

    // `mixed`: random literals interleaved with copies from the recent window,
    // the match density of a compressed executable.
    let mut mixed = Vec::with_capacity(4 << 20);
    while mixed.len() < 4 << 20 {
        let literals = 8 + (rng.0 as usize >> 16) % 24;
        rng.fill(&mut mixed, literals);
        let dist = (64 + (rng.0 as usize) % 4032).min(mixed.len());
        let length = 8 + (rng.0 as usize >> 12) % 56;
        let start = mixed.len() - dist;
        let copied: Vec<u8> = mixed[start..(start + length).min(mixed.len())].to_vec();
        mixed.extend_from_slice(&copied);
    }

    // `code`: x86-shaped call/jmp runs with varying operands and stretch of
    // zero padding, which is what the delta filter and the match model face on
    // a real executable.
    let mut code = Vec::with_capacity(4 << 20);
    let mut counter = rng.0;
    while code.len() < 4 << 20 {
        counter = counter.wrapping_add(0x10 + (rng.0 & 0x7f));
        code.push(if rng.0 & 1 == 0 { 0xe8 } else { 0x8b });
        code.extend_from_slice(&counter.to_le_bytes());
        code.extend_from_slice(&[0x44, 0x24, rng.next_byte(), 0xc3]);
        if rng.0 & 0x3000_0000 == 0 {
            code.extend(std::iter::repeat_n(0u8, 8 + (rng.0 as usize) % 24));
        }
    }

    // (member name, label, data) — the member name is a plain file name so the
    // kept archives can be handed to another tool without shell quoting games.
    let corpora: [(&str, &str, Vec<u8>); 3] = [
        ("text.bin", "text 8 MiB", text),
        ("mixed.bin", "mixed 4 MiB", mixed),
        ("code.bin", "code-ish 4 MiB", code),
    ];

    println!("decode benchmark: best of {reps} runs, RAR5 v50, level 3 (normal)");
    let mut grand_total = 0u64;
    let mut grand_best = 0.0f64;
    for (name, label, data) in &corpora {
        let dir = std::env::temp_dir().join("rar-rs-decode-bench");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(format!("{}.rar", name.trim_end_matches(".bin")));
        let _ = std::fs::remove_file(&path);
        let mut writer = ArchiveWriter::create(&path).expect("create archive");
        writer
            .add_bytes(
                name,
                data,
                EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL),
            )
            .expect("add member");
        writer.finish().expect("finish");
        let packed = std::fs::metadata(&path).expect("stat").len();

        let mut best = f64::MAX;
        let mut extracted = 0usize;
        for _ in 0..reps {
            let start = Instant::now();
            let mut reader = ArchiveReader::open(&path).expect("open");
            let ids: Vec<_> = reader.entries().map(|entry| entry.id()).collect();
            let mut total = 0usize;
            for id in &ids {
                total += reader.read_entry(*id).expect("read").len();
            }
            best = best.min(start.elapsed().as_secs_f64());
            extracted = total;
        }
        assert_eq!(extracted, data.len(), "{label}: extracted size");

        println!(
            "  {label:<16} {:>6.2} MiB packed from {:>6.2} MiB, crc32 {:08x}, {:>7.1} MiB/s ({:.3}s)",
            packed as f64 / (1 << 20) as f64,
            data.len() as f64 / (1 << 20) as f64,
            crc32(data),
            extracted as f64 / best / 1e6,
            best,
        );
        grand_total += extracted as u64;
        grand_best += best;
    }
    println!(
        "  {:<16} {:>7.1} MiB/s overall",
        "total",
        grand_total as f64 / grand_best / 1e6
    );
    let dir = std::env::temp_dir().join("rar-rs-decode-bench");
    if keep {
        println!("  archives kept in {}", dir.display());
    } else {
        let _ = std::fs::remove_dir_all(dir);
    }
}
