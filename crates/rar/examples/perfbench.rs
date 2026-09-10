//! Reproducible compression baseline for the RAR5/RAR7 (v50/v70) path.
//!
//! Why this exists: `bench.rs` measures synthetic corpora ad hoc, while
//! `collectbench`/`mtbench` need an external corpus file *and* the `parallel`
//! feature — neither can reproduce the numbers quoted in `PLAN.md` ("RAR5
//! （压缩面）") on a clean checkout. Every corpus here is generated from a
//! fixed seed, so two runs on two machines see byte-identical input; the CRC32
//! printed per corpus is the proof.
//!
//! It is the reference for the three open PLAN items:
//!   * **dll m3 parse speed** — `dll-like` corpus, single-threaded,
//!     `codec` column vs WinRAR's 1.8 s on a ~5.75 MB DLL.
//!   * **incompressible ~800 ms overhead** — `random` corpus; the
//!     `overhead` column is `archive - codec`, i.e. everything spent outside
//!     the codec (I/O, headers, CRC/hash, file write).
//!   * **solid baseline** — `solid` column vs `archive`, the starting point
//!     for the solid MT lever (solid is serial today).
//!
//! `codec` runs the raw codec in memory (`rar_rs::encode`, no filters);
//! `archive` runs the full create + `add_file` on prepared spill inputs
//! (delta/x86 filters active, as a real CLI `rar a` run) + `finish`; `solid`
//! is the same data split into 4 members with `WriterOptions::solid_mode`.
//!
//! Run:
//!   cargo run --release --example perfbench [--size-mb N] [--level L]
//!        [--repeats R] [--only KIND[,KIND]] [--file PATH] [--no-solid]

use std::path::{Path, PathBuf};
use std::time::Instant;

/// Default dictionary ceiling (`-md32m`), matching WinRAR's default; the
/// effective dictionary is clipped per corpus — see `dict_log_for`.
const SOLID_MEMBERS: usize = 4;
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

// ── Deterministic corpora ──────────────────────────────────────────────────

/// xorshift64* — the same generator `bench.rs` uses, so the `random` corpus
/// matches the existing harness byte for byte.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() >> 32) as u8
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.next_u64() as usize % xs.len()]
    }
}

const WORDS: &[&str] = &[
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
];

const TAGS: &[&str] = &[
    "record", "entry", "item", "node", "row", "cell", "field", "value",
];

const STRINGS: &[&str] = &[
    "kernel32.dll",
    "CreateFileW",
    "ReadFile",
    "WriteFile",
    "ERROR_NOT_ENOUGH_MEMORY",
    "invalid parameter",
    "buffer overflow",
    "unicode string",
    "resource section",
    "import table",
];

/// The same lorem corpus `bench.rs`/`mtbench.rs` use — one line repeated. It
/// is *far* more compressible than prose (ratio ~0.02%) because every line is
/// an identical match at a fixed period, which is what PLAN's "text 32 MiB/s"
/// was measured on. Kept bit-identical so this harness reproduces those
/// numbers instead of inventing a new baseline.
fn text_data(size: usize) -> Vec<u8> {
    let lorem = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam.\n";
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        out.extend_from_slice(lorem);
    }
    out.truncate(size);
    out
}

/// A harder prose-like corpus: a 20-word vocabulary in pseudo-random order
/// with a line counter. Realistic (~17% at m3) and dramatically slower to
/// parse than `text` — no fixed-period matches, so the match finder and the
/// optimal parser do full work. This is the corpus that exposes parse cost.
fn wordtext_data(size: usize) -> Vec<u8> {
    let mut rng = Rng(SEED);
    let mut out = Vec::with_capacity(size);
    let mut line = 0usize;
    while out.len() < size {
        for _ in 0..12 {
            out.extend_from_slice(rng.pick(WORDS).as_bytes());
            out.push(b' ');
        }
        out.extend_from_slice(line.to_string().as_bytes());
        out.push(b'\n');
        line += 1;
    }
    out.truncate(size);
    out
}

fn xml_data(size: usize) -> Vec<u8> {
    let mut rng = Rng(SEED ^ 0xABCD);
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root>\n");
    let mut id = 0usize;
    while out.len() < size {
        let tag = rng.pick(TAGS);
        out.extend_from_slice(format!("  <{tag} id=\"{id}\">\n").as_bytes());
        out.extend_from_slice(format!("    <name>{}</name>\n", rng.pick(WORDS)).as_bytes());
        out.extend_from_slice(format!("    <value>{}</value>\n", rng.pick(WORDS)).as_bytes());
        out.extend_from_slice(format!("  </{tag}>\n").as_bytes());
        id += 1;
    }
    out.extend_from_slice(b"</root>\n");
    out.truncate(size);
    out
}

/// Incompressible: xorshift64* bytes (identical to `bench.rs::binary_data`).
fn random_data(size: usize) -> Vec<u8> {
    let mut rng = Rng(SEED);
    let mut out = Vec::with_capacity(size);
    for _ in 0..size {
        out.push(rng.next_byte());
    }
    out
}

fn mixed_data(size: usize) -> Vec<u8> {
    let mut out = text_data(size / 2);
    let mut rng = Rng(SEED);
    while out.len() < size {
        out.push(rng.next_byte());
    }
    out
}

/// The existing synthetic x86 corpus (`bench.rs::x86_data`): dense E8 call
/// sites with NOP/INC filler.
fn x86_data(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut pos = 0u32;
    while out.len() < size {
        out.extend_from_slice(&[0x90; 64]); // NOP
        pos += 64;
        out.push(0xe8); // CALL rel32
        out.extend_from_slice(&(pos.wrapping_mul(7) & 0x00FF_FFFF).to_le_bytes());
        pos += 5;
        out.extend_from_slice(&[0x41; 16]); // INC ECX
        pos += 16;
    }
    out.truncate(size);
    out
}

/// A PE-shaped corpus standing in for a real DLL: MZ/PE headers with long zero
/// runs, a `.text` section dense in E8/E9 relative call/jump sites (the
/// x86 filter's target), a `.rdata` section of repeated strings, short
/// incompressible blobs, and file-alignment zero padding. Deterministic — a
/// real DLL is machine-specific and cannot be a shared baseline.
fn dll_like_data(size: usize) -> Vec<u8> {
    let mut rng = Rng(SEED ^ 0x5EED);
    let mut out = Vec::with_capacity(size);
    // DOS stub: a few fields, then long zero runs.
    out.extend_from_slice(b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00\xff\xff\x00\x00");
    out.extend_from_slice(&[0u8; 58]);
    out.extend_from_slice(b"\x0e\x1f\xba\x0e\x00\xb4\x09\xcd\x21\xb8\x01\x4c\xcd\x21");
    out.extend_from_slice(b"This program cannot be run in DOS mode.\r\r\n$");
    out.extend_from_slice(&[0u8; 64]);
    // PE signature + COFF/optional headers (mostly zeros and repeated fields).
    out.extend_from_slice(b"PE\x00\x00");
    out.extend_from_slice(&[0x64, 0x86, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    out.extend_from_slice(&[0u8; 24]);
    out.extend_from_slice(b"\x0b\x02\x0e\x14");
    out.extend_from_slice(&[0u8; 112]);
    // Four 40-byte section headers.
    for name in [
        b".text\0\0\0",
        b".rdata\0\0",
        b".data\0\0\0",
        b".rsrc\0\0\0",
    ] {
        out.extend_from_slice(name);
        out.extend_from_slice(&[0u8; 32]);
    }

    let mut rel = 0u32;
    while out.len() < size {
        // .text: dense relative call/jump sites.
        for _ in 0..48 {
            match rng.next_u64() % 8 {
                0..=4 => {
                    out.push(0xe8); // CALL rel32
                    out.extend_from_slice(&rel.wrapping_mul(7).to_le_bytes());
                }
                5..=6 => {
                    out.push(0xe9); // JMP rel32
                    out.extend_from_slice(&rel.wrapping_mul(11).to_le_bytes());
                }
                _ => {
                    // MOV EAX,[EBP-4]; TEST EAX,EAX; JZ +0x10
                    out.extend_from_slice(&[0x8b, 0x45, 0xfc, 0x85, 0xc0, 0x74, 0x10]);
                }
            }
            rel = rel.wrapping_add(6);
        }
        // .rdata: repeated NUL-terminated strings.
        for _ in 0..16 {
            out.extend_from_slice(rng.pick(STRINGS).as_bytes());
            out.push(0);
        }
        // .data: a short incompressible run (embedded blob / checksummed
        // resource) — real DLLs carry these and they pin the floor on ratio.
        for _ in 0..64 {
            out.push(rng.next_byte());
        }
        // File-alignment padding.
        out.extend_from_slice(&[0u8; 128]);
    }
    out.truncate(size);
    out
}

// ── Measurement ────────────────────────────────────────────────────────────

struct Stats {
    min: f64,
    median: f64,
}

fn stats(mut xs: Vec<f64>) -> Stats {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let min = *xs.first().unwrap_or(&0.0);
    let median = xs[xs.len() / 2];
    Stats { min, median }
}

/// The dictionary the *archive* path would pick for `size`
/// (`min(-md, 2 * floor_pow2(size))`, PLAN "字典"). The raw codec path does no
/// such clipping, so the benchmark must pass the same dictionary or the
/// `codec` column is charged for building a 32 MiB match-finder tree over a
/// 2 MiB input — which showed up as negative `overhead` on `random`.
fn dict_log_for(size: usize) -> u8 {
    const MAX: usize = 32 * 1024 * 1024;
    const UNIT: usize = 128 * 1024;
    let floor_pow2 = if size == 0 {
        UNIT
    } else {
        1usize << (63 - (size as u64).leading_zeros())
    };
    let want = (floor_pow2.saturating_mul(2)).clamp(UNIT, MAX);
    ((want / UNIT).ilog2() as u8).min(15)
}

/// Raw codec encode, in memory: isolates parse + entropy coding from every
/// archive-level cost. `dict_override` pins the dictionary (log2(size/128 KiB))
/// instead of letting it follow the corpus size — used to attribute the cost of
/// a large match-finder tree on incompressible data.
fn time_codec(data: &[u8], level: u8, dict_override: Option<u8>) -> (f64, usize) {
    let log = dict_override.unwrap_or_else(|| dict_log_for(data.len()));
    let t = Instant::now();
    let packed =
        rar_rs::encode(data, rar_rs::EncodeOptions::new(level, log)).expect("codec encode");
    (t.elapsed().as_secs_f64() * 1000.0, packed.len())
}

/// Full archive creation for one member set. `inputs` maps archive member
/// names to spill files written by [`prepare_inputs`]; the archive is created
/// with `add_file` semantics (the automatic delta/x86 filters run and are
/// kept when they beat plain LZSS), so the `archive` column measures the real
/// product path — not the unfiltered `add_bytes` shortcut. The spill-file
/// write happens before the clock starts; archive *reading* of the prepared
/// input is part of the measured cost, exactly as a real `rar a` run.
fn time_archive(
    dir: &Path,
    tag: &str,
    inputs: &[(String, PathBuf)],
    level: u8,
    solid: bool,
) -> (f64, u64) {
    let path = dir.join(format!("perfbench-{tag}.rar"));
    let t = Instant::now();
    {
        let mode = if solid {
            rar_rs::SolidMode::Continuous
        } else {
            rar_rs::SolidMode::Disabled
        };
        let mut ar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().solid_mode(mode),
        )
        .expect("create");
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(level).expect("level"));
        for (name, src) in inputs {
            ar.add_path_as(src, name, opts).expect("add");
        }
        ar.finish().expect("close");
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let packed = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&path);
    (ms, packed)
}

/// Write `data` to a spill file under `dir`, returning `(arcname, path)`.
/// Called once per corpus (outside the timed window); repeated archive passes
/// then re-read the same prepared input, matching a real CLI run.
fn prepare_input(dir: &Path, sub: &str, arcname: &str, data: &[u8]) -> (String, PathBuf) {
    let subdir = dir.join(format!("input-{sub}"));
    std::fs::create_dir_all(&subdir).expect("input dir");
    let path = subdir.join(format!("{arcname}.bin"));
    std::fs::write(&path, data).expect("write input");
    (arcname.to_string(), path)
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

fn run_corpus(
    dir: &Path,
    name: &str,
    data: &[u8],
    level: u8,
    repeats: usize,
    with_solid: bool,
    dict_override: Option<u8>,
) {
    let mb = data.len() as f64 / 1048576.0;
    let mut codec = Vec::with_capacity(repeats);
    let mut archive = Vec::with_capacity(repeats);
    let mut solid = Vec::with_capacity(repeats);
    let mut packed = 0usize;

    // Prepare the spill inputs once; the archive column measures add_file on
    // these (filters active), re-reading the same files across repeats.
    let input_one = prepare_input(dir, name, "data", data);
    let input_members: Vec<(String, PathBuf)> = if with_solid {
        let chunk = (data.len() / SOLID_MEMBERS).max(1);
        data.chunks(chunk)
            .enumerate()
            .map(|(i, part)| prepare_input(dir, name, &format!("m{i}"), part))
            .collect()
    } else {
        Vec::new()
    };

    for _ in 0..repeats {
        let (ms, _n) = time_codec(data, level, dict_override);
        codec.push(ms);
        let (ms_a, n_a) = time_archive(
            dir,
            &format!("{name}-s"),
            std::slice::from_ref(&input_one),
            level,
            false,
        );
        archive.push(ms_a);
        packed = n_a as usize;
        if with_solid {
            solid.push(time_archive(dir, &format!("{name}-d"), &input_members, level, true).0);
        }
    }

    let _ = std::fs::remove_dir_all(dir.join(format!("input-{name}")));

    let c = stats(codec);
    let a = stats(archive);
    let ratio = packed as f64 * 100.0 / data.len() as f64;
    let throughput = mb / (c.median / 1000.0);

    let solid_part = if with_solid {
        let s = stats(solid);
        format!("  solid {:>6.0}/{:>6.0}", s.min, s.median)
    } else {
        String::new()
    };

    println!(
        "{name:<10} {:>8.1}  {:08x}  {:>8.0} {:>8.0}  {:>8.0} {:>8.0}  {:>8.0}  {:>6.2}%  {:>7.1}{}",
        mb,
        crc32(data),
        c.min,
        c.median,
        a.min,
        a.median,
        a.median - c.median,
        ratio,
        throughput,
        solid_part,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut size_mb: usize = 8;
    let mut level: u8 = 3;
    let mut repeats: usize = 3;
    let mut only: Option<Vec<String>> = None;
    let mut extra: Option<PathBuf> = None;
    let mut with_solid = true;
    let mut dict_override: Option<u8> = None;

    let value = |i: usize, args: &[String]| -> Option<String> { args.get(i + 1).cloned() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--size-mb" | "--level" | "--repeats" | "--only" | "--file" | "--dict-log" => {
                let Some(v) = value(i, &args) else {
                    i += 1;
                    continue;
                };
                match args[i].as_str() {
                    "--size-mb" => size_mb = v.parse().unwrap_or(size_mb),
                    "--level" => level = v.parse().unwrap_or(level),
                    "--repeats" => repeats = v.parse().unwrap_or(repeats),
                    "--only" => only = Some(v.split(',').map(|p| p.trim().to_string()).collect()),
                    "--dict-log" => {
                        dict_override = v.parse::<usize>().ok().map(|n| n.clamp(0, 15) as u8)
                    }
                    _ => extra = Some(PathBuf::from(v)),
                }
                i += 2;
            }
            "--no-solid" => {
                with_solid = false;
                i += 1;
            }
            other => {
                if let Ok(n) = other.parse::<usize>() {
                    size_mb = n;
                }
                i += 1;
            }
        }
    }

    let size = size_mb * 1024 * 1024;
    let dir = std::env::temp_dir().join("rar5-perfbench");
    std::fs::create_dir_all(&dir).expect("temp dir");

    println!(
        "rar-rs perf baseline — level m{level}, dict {dict}, \
         {size_mb} MiB corpora, {repeats} repeats (min/median ms)",
        dict = if let Some(d) = dict_override {
            format!("pinned 128KiB<<{d}")
        } else {
            "min(32 MiB, 2*floor_pow2(size))".into()
        }
    );
    println!(
        "{:<10} {:>8}  {:>8}  {:>8} {:>8}  {:>8} {:>8}  {:>8}  {:>7}  {:>7}",
        "corpus",
        "MiB",
        "crc32",
        "codec.min",
        "codec.med",
        "arch.min",
        "arch.med",
        "delta",
        "ratio",
        "MiB/s"
    );

    let mut corpora: Vec<(String, Vec<u8>)> = vec![
        ("text".into(), text_data(size)),
        ("wordtext".into(), wordtext_data(size)),
        ("xml".into(), xml_data(size)),
        ("random".into(), random_data(size)),
        ("mixed".into(), mixed_data(size)),
        ("x86syn".into(), x86_data(size)),
        ("dll-like".into(), dll_like_data(size)),
    ];
    if let Some(path) = extra {
        match std::fs::read(&path) {
            Ok(bytes) => {
                println!("# external corpus: {}", path.display());
                corpora.push((
                    path.file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".into()),
                    bytes,
                ));
            }
            Err(e) => eprintln!("skip {}: {e}", path.display()),
        }
    }

    if let Some(wanted) = only {
        corpora.retain(|(name, _)| wanted.iter().any(|w| w == name));
    }

    for (name, data) in &corpora {
        run_corpus(&dir, name, data, level, repeats, with_solid, dict_override);
    }

    println!(
        "\n# codec  = raw rar_rs::encode (parse + coding only, no filters)\n\
         # archive = full create + add_file on spill inputs (delta/x86 filters\n\
         #           active, kept when they beat plain LZSS) + close (RAR5 v50)\n\
         # solid   = same data split into {SOLID_MEMBERS} members, solid_mode(Continuous)\n\
         # delta   = archive.med - codec.med. NOT pure I/O overhead: the archive path also\n\
         #           (a) spends time on filter candidate probing the raw codec may skip and\n\
         #           (b) falls back to STORE for incompressible data, so `random` shows a\n\
         #           large NEGATIVE delta (compression skipped, not work avoided).\n\
         #           Read it on compressible corpora (xml/wordtext/dll-like) only.\n\
         # ratio   = archive packed / input. MiB/s is from the codec median.\n\
         # crc32    = corpus fingerprint; identical across machines if the seed holds\n\
         # --dict-log N pins the dictionary to 128 KiB << N for every corpus (incl. the\n\
         #           raw codec column) — use it to attribute a large match-finder tree."
    );
}
