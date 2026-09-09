//! Attribute compression time: the raw codec (delta+x86 attempts) or the
//! full typed writer path, to separate container/pipeline cost (paid by CLI
//! and library callers alike) from any CLI-specific overhead. Pass `codec`
//! to time only the encode; pass `writer` (default) for the full
//! create + add_batch + finish.
//!
//! Run:  cargo run --release --features parallel --example clioverhead -- writer <file> <level> <threads> <out.rar>

use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, rest): (String, Vec<String>) = match args.first().map(String::as_str) {
        Some("codec") | Some("writer") => (args.first().unwrap().clone(), args[1..].to_vec()),
        _ => ("writer".into(), args),
    };
    let file = &rest[0];
    let level: u8 = rest.get(1).map(|s| s.parse().unwrap()).unwrap_or(3);
    let threads: usize = rest.get(2).map(|s| s.parse().unwrap()).unwrap_or(8);
    let out = rest.get(3).cloned().unwrap_or_else(|| "out.rar".into());

    // Mirror the CLI's global thread setup (`rar -mt<N>`).
    rar_rs::set_compression_threads(threads);

    if mode == "codec" {
        run_codec(file, level, threads);
        return;
    }

    let t1 = Instant::now();
    let writer = (|| -> Result<usize, String> {
        let opts = rar_rs::WriterOptions::new();
        let entry_opt = rar_rs::EntryWriteOptions::new().compression_level(
            rar_rs::CompressionLevel::try_from(level).map_err(|e| format!("level: {e}"))?,
        );
        let mut w =
            rar_rs::ArchiveWriter::create_with(&out, opts).map_err(|e| format!("create: {e}"))?;
        w.add_batch(&[rar_rs::WriteEntry::File {
            path: Path::new(file),
            name: None,
            options: entry_opt,
        }])
        .map_err(|e| format!("add: {e}"))?;
        w.finish().map_err(|e| format!("finish: {e}"))?;
        Ok(std::fs::metadata(&out)
            .map(|m| m.len() as usize)
            .unwrap_or(0))
    })();
    let writer_ms = t1.elapsed().as_secs_f64() * 1000.0;
    match writer {
        Ok(bytes) => println!("writer {writer_ms:.0} ms ({bytes} B)"),
        Err(e) => println!("writer ERROR {e}"),
    }
}

/// Raw codec path, exactly as the batch writer's file_origin branch runs
/// it: auto delta attempt first, then auto x86 attempt.
fn run_codec(file: &str, level: u8, threads: usize) {
    use rar_rs::codec::lzss_huff::{encode_with_auto_delta_filter, encode_with_auto_x86_filter};

    let data = std::fs::read(file).expect("read corpus");
    let method = level.min(5);
    let dsl = dict_log_for(data.len());
    let variant = rar_rs::ArchiveVersion::V50;

    let t0 = Instant::now();
    let codec = (|| -> Result<usize, String> {
        let packed = match encode_with_auto_delta_filter(&data, method, dsl, variant, threads, None)
            .map_err(|e| e.to_string())?
        {
            Some(filtered) if filtered.len() < data.len() => filtered,
            _ => match encode_with_auto_x86_filter(&data, method, dsl, variant, threads, None)
                .map_err(|e| e.to_string())?
            {
                Some(filtered) if filtered.len() < data.len() => filtered,
                _ => {
                    // The writer's other fallback: windowed MT chunk encode.
                    rar_rs::codec::lzss_huff::encode_chunked_mt(
                        &data,
                        method,
                        dsl,
                        rar_rs::codec::lzss_huff::DEFAULT_CHUNK_SIZE,
                        &mut rar_rs::codec::EncoderState::default(),
                        threads,
                        true,
                        variant,
                    )
                }
            },
        };
        Ok(packed.len())
    })();
    let codec_ms = t0.elapsed().as_secs_f64() * 1000.0;
    match codec {
        Ok(bytes) => println!("codec {codec_ms:.0} ms ({bytes} B)"),
        Err(e) => println!("codec ERROR {e}"),
    }
}

/// Auto dictionary log for a member, mirroring `dict_log_for` in
/// `format/rar5/write/layout.rs` (default 32 MiB request ceiling).
fn dict_log_for(data_size: usize) -> u8 {
    let base = 128 * 1024;
    let file_pow2 = 1usize << (usize::BITS - 1 - data_size.max(1).leading_zeros());
    let auto_cap = (file_pow2 * 2).max(base);
    let target = auto_cap.min(32 * 1024 * 1024);
    let mut log = 0u8;
    while (base << log) < target && log < 15 {
        log += 1;
    }
    log
}
