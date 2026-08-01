//! Compression benchmark — full archive-creation path (sample-probe + codec).
//!
//! Run:  cargo run --release --example bench [size_mb]

use std::time::Instant;

use rar5::RarArchive;

fn text_data(size: usize) -> Vec<u8> {
    let lorem = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam.\n";
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        out.extend_from_slice(lorem);
    }
    out.truncate(size);
    out
}

fn binary_data(size: usize) -> Vec<u8> {
    // Pseudo-random with a fixed seed (xorshift64*) — incompressible.
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut out = Vec::with_capacity(size);
    for _ in 0..size {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        out.push((state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8);
    }
    out
}

fn mixed_data(size: usize) -> Vec<u8> {
    let mut out = text_data(size / 2);
    out.extend_from_slice(&binary_data(size / 2));
    out
}

fn bench(name: &str, data: &[u8]) {
    let dir = std::env::temp_dir().join("rar5-bench");
    std::fs::create_dir_all(&dir).unwrap();
    println!("== {name}: {} bytes ==", data.len());
    for level in 1..=5u8 {
        let out = dir.join(format!("{name}-l{level}.rar"));
        let t0 = Instant::now();
        let result = (|| -> rar5::RarResult<()> {
            let mut rar = RarArchive::create(&out)?;
            rar.add_bytes("data.bin", data, level)?;
            rar.close()?;
            Ok(())
        })();
        let elapsed = t0.elapsed();
        match result {
            Ok(()) => {
                let packed = std::fs::metadata(&out).unwrap().len();
                let mb = data.len() as f64 / 1048576.0;
                println!(
                    "  level {level}: {:>6} ms  {:>8.1} MiB/s  ratio {:>6.2}%",
                    elapsed.as_millis(),
                    mb / elapsed.as_secs_f64(),
                    packed as f64 * 100.0 / data.len() as f64
                );
                let _ = std::fs::remove_file(&out);
            }
            Err(e) => println!("  level {level}: ERROR {e}"),
        }
    }
}

fn main() {
    let size_mb: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let size = size_mb * 1024 * 1024;

    let t = text_data(size);
    let r = binary_data(size);
    let m = mixed_data(size);

    bench("text (compressible)", &t);
    bench("random (incompressible)", &r);
    bench("mixed", &m);
}
