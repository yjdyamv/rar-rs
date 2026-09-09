//! BT4-collect bench: times the sequential and MT encodes on a FILE corpus
//! (the real DLL/x86 path), verifying the packed output decodes back
//! byte-identically. Collect work dominates these encodes, so this is the
//! harness for the pipelined-descent lever (`.scratch/compression-perf/
//! issues/11-batch-descent.md`).
//!
//! Requires the `parallel` feature.
//! Run: cargo run --release --features parallel --example collectbench -- <file> <level> <threads>

use std::time::Instant;

fn main() {
    let file = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "corpus.bin".into());
    let level: u8 = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "3".into())
        .parse()
        .unwrap();
    let threads: usize = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "8".into())
        .parse()
        .unwrap();
    let data = std::fs::read(&file).expect("read corpus");
    let mb = data.len() as f64 / (1024.0 * 1024.0);
    const DICT: u8 = 8; // 32 MiB, WinRAR default

    let t = Instant::now();
    let packed_seq = rar_rs::encode(&data, rar_rs::EncodeOptions::new(level, DICT)).unwrap();
    let seq_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "seq   l{level} {:>6.1} MiB {}: {:>7.0} ms  {:>5.1} MiB/s  ratio {:>6.2}%",
        mb,
        file,
        seq_ms,
        mb / (seq_ms / 1000.0),
        packed_seq.len() as f64 * 100.0 / data.len() as f64,
    );

    rar_rs::set_compression_threads(threads);
    let t = Instant::now();
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
    let mt_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "mt{threads} l{level} {:>6.1} MiB {}: {:>7.0} ms  {:>5.1} MiB/s  ratio {:>6.2}%  (+{:.1}%)",
        mb,
        file,
        mt_ms,
        mb / (mt_ms / 1000.0),
        packed.len() as f64 * 100.0 / data.len() as f64,
        (packed.len() as f64 / packed_seq.len() as f64 - 1.0) * 100.0,
    );
    let out = rar_rs::decode(&packed, level, data.len() as u64, DICT, None).unwrap();
    assert_eq!(out, data, "mt decode mismatch");
    println!("decode OK (byte-identical)");
}
