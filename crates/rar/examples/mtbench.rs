//! Requires the `parallel` feature: cargo run --release --features parallel --example mtbench ...
//! MT (encode_chunked_mt) vs sequential (compress) — speed + ratio.
//! Run:  cargo run --release --features parallel --example mtbench <kind> <size_mb> <level>
use std::time::Instant;

fn text_data(size: usize) -> Vec<u8> {
    let lorem = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam.\n";
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        out.extend_from_slice(lorem);
    }
    out.truncate(size);
    out
}

fn x86_data(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut pos = 0u32;
    while out.len() < size {
        out.extend_from_slice(&[0x90; 64]);
        pos += 64;
        out.push(0xe8);
        out.extend_from_slice(&(pos.wrapping_mul(7) & 0x00FF_FFFF).to_le_bytes());
        pos += 5;
        out.extend_from_slice(&[0x41; 16]);
        pos += 16;
    }
    out.truncate(size);
    out
}

fn mixed_data(size: usize) -> Vec<u8> {
    let mut out = text_data(size / 2);
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
    let kind = std::env::args().nth(1).unwrap_or_else(|| "mixed".into());
    let size_mb: usize = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "64".into())
        .parse()
        .unwrap();
    let level: u8 = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "3".into())
        .parse()
        .unwrap();
    let data = match kind.as_str() {
        "x86" => x86_data(size_mb * 1024 * 1024),
        "text" => text_data(size_mb * 1024 * 1024),
        "mixed" => mixed_data(size_mb * 1024 * 1024),
        _ => panic!("unknown kind"),
    };
    let mb = size_mb as f64;
    const DICT: u8 = 8; // 32 MiB, WinRAR default

    // Sequential (optimal parse)
    let t0 = Instant::now();
    let packed_seq = rar_rs::encode(&data, rar_rs::EncodeOptions::new(level, DICT)).unwrap();
    let seq_ms = t0.elapsed().as_millis() as f64;
    println!(
        "seq   l{level} {} MiB: {:>7.0} ms  {:>5.1} MiB/s  ratio {:>6.2}%",
        size_mb,
        seq_ms,
        mb / (seq_ms / 1000.0),
        packed_seq.len() as f64 * 100.0 / data.len() as f64,
    );

    // Multi-threaded (optimal parse per slice)
    for threads in [2usize, 4, 8] {
        rar_rs::set_compression_threads(threads);
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
        println!(
            "mt{threads} l{level} {} MiB: {:>7.0} ms  {:>5.1} MiB/s  ratio {:>6.2}%  (+{:.1}% vs seq)",
            size_mb,
            mt_ms,
            mb / (mt_ms / 1000.0),
            packed.len() as f64 * 100.0 / data.len() as f64,
            (packed.len() as f64 / packed_seq.len() as f64 - 1.0) * 100.0,
        );
        let out = rar_rs::decode(&packed, level, data.len() as u64, DICT, None).unwrap();
        assert_eq!(out.len(), data.len(), "mt length mismatch");
        assert_eq!(out, data, "mt decode mismatch");
    }
    rar_rs::set_compression_threads(0);
    println!("all MT outputs decode byte-identically");
}
