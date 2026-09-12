//! Shared fixtures for the WinRAR interop suites: tool lookup, process
//! helpers, payload writers and the per-case runners.

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, OpenOptions, WriterOptions,
};

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory containing `Rar.exe` and `UnRAR.exe`, when WinRAR is
/// installed. `None` skips the tests; the skip prints a visible marker and
/// `SA_REQUIRE_WINRAR=1` turns a missing installation into a hard failure so
/// a CI job can demand the suite instead of silently skipping it.
pub(crate) fn winrar_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SA_WINRAR_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        for dir in [
            "C:\\Program Files\\WinRAR",
            "C:\\Program Files (x86)\\WinRAR",
        ] {
            let dir = PathBuf::from(dir);
            if dir.join("UnRAR.exe").exists() {
                return Some(dir);
            }
        }
    }
    assert!(
        std::env::var_os("SA_REQUIRE_WINRAR").is_none(),
        "WinRAR is required (SA_REQUIRE_WINRAR is set): set SA_WINRAR_DIR"
    );
    eprintln!("SKIP: WinRAR not found (set SA_WINRAR_DIR)");
    None
}

pub(crate) fn rar_bin() -> Option<PathBuf> {
    let dir = winrar_dir()?;
    let exe = if cfg!(windows) { "Rar.exe" } else { "rar" };
    let bin = dir.join(exe);
    bin.exists().then_some(bin)
}

/// The WinRAR 6.23 console writer from the project's tool cache — the last
/// release whose `Rar.exe` can both create (`-ma4`) and REPAIR RAR4
/// archives. The default-install 7.23 reads RAR4 but neither writes it nor
/// repairs its recovery records, so RAR4 write/repair interop must drive
/// 6.23 explicitly. `None` skips those tests (e.g. on CI without the cache).
pub(crate) fn rar4_623_bin() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "Rar.exe" } else { "rar" };
    [
        "../../.cache/winrar/6-23",
        "../.cache/winrar/6-23",
        ".cache/winrar/6-23",
    ]
    .iter()
    .map(|dir| Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(exe))
    .find(|bin| bin.exists())
}

pub(crate) fn unrar_bin() -> Option<PathBuf> {
    let dir = winrar_dir()?;
    let exe = if cfg!(windows) { "UnRAR.exe" } else { "unrar" };
    let bin = dir.join(exe);
    bin.exists().then_some(bin)
}

pub(crate) fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Run a command, returning (status, stdout+stderr).
pub(crate) fn run(cmd: &mut Command) -> (bool, String) {
    let out = cmd.output().expect("run command");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// `UnRAR t <archive>` must report success (optionally with a password).
pub(crate) fn unrar_test(path: &Path, password: Option<&str>) -> (bool, String) {
    let mut cmd = Command::new(unrar_bin().expect("unrar"));
    cmd.arg("t").arg("-idq");
    if let Some(pw) = password {
        cmd.arg(format!("-p{pw}"));
    }
    cmd.arg(path);
    run(&mut cmd)
}

/// `UnRAR x <archive> <dest>/` must succeed; returns the output.
pub(crate) fn unrar_extract(path: &Path, dest: &Path, password: Option<&str>) -> (bool, String) {
    let mut cmd = Command::new(unrar_bin().expect("unrar"));
    cmd.arg("x").arg("-idq").arg("-o+").arg("-y");
    if let Some(pw) = password {
        cmd.arg(format!("-p{pw}"));
    }
    cmd.arg(path).arg(dest);
    run(&mut cmd)
}

/// Create a file of exactly `size` bytes holding a deterministic
/// compressible pattern (fast to generate, exercises the compressed path).
pub(crate) fn write_pattern_file(path: &Path, size: u64, seed: u8) {
    let mut f = std::fs::File::create(path).expect("create file");
    let mut chunk = Vec::with_capacity(1 << 20);
    let pat: Vec<u8> = (0..64u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(seed))
        .collect();
    while (chunk.len() as u64) < (1 << 20) {
        chunk.extend_from_slice(&pat);
    }
    let mut left = size;
    while left > 0 {
        let n = left.min(chunk.len() as u64) as usize;
        f.write_all(&chunk[..n]).expect("write file");
        left -= n as u64;
    }
}

pub(crate) fn file_sha256(path: &Path) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut f = std::fs::File::open(path).expect("open");
    let mut buf = vec![0u8; 1 << 20];
    loop {
        use std::io::Read;
        let n = f.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of an in-memory byte slice.
pub(crate) fn file_sha256_bytes(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 72 MiB of compressible data — comfortably over the streaming
/// compression threshold (64 MiB) and over four 16 MiB volumes. Kept just
/// past both boundaries: the interop cases run WinRAR over this file
/// several times, so size directly bounds the test runtime.
pub(crate) const STREAM_SIZE: u64 = 72 * 1024 * 1024;

/// Correlated multi-channel samples (small per-sample deltas) of the kind
/// WinRAR's delta filter targets. We emit a delta filter for this data and
/// the real UnRAR must decode it byte-for-byte.
pub(crate) fn write_correlated_pcm(path: &Path, channels: usize, samples: usize) {
    let mut val = vec![0i32; channels];
    let mut state = 0xABCDEF01u64;
    let mut buf = Vec::with_capacity(channels * samples * 2);
    for _ in 0..samples {
        for v in &mut val {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            *v += ((state >> 33) as u32 % 8) as i32 - 4;
            buf.extend_from_slice(&(*v as i16).to_le_bytes());
        }
    }
    std::fs::write(path, &buf).unwrap();
}

/// Synthesize a minimal 16-bit PCM WAV so that real WinRAR applies its own
/// delta (audio) filter when archiving it.
pub(crate) fn write_wav(path: &Path, channels: u16, samples: u32) {
    let byte_rate = 44100u32 * channels as u32 * 2;
    let data_len = channels as u32 * samples * 2;
    let mut hdr = Vec::with_capacity(44);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&(36 + data_len).to_le_bytes());
    hdr.extend_from_slice(b"WAVE");
    hdr.extend_from_slice(b"fmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes()); // PCM
    hdr.extend_from_slice(&channels.to_le_bytes());
    hdr.extend_from_slice(&44100u32.to_le_bytes());
    hdr.extend_from_slice(&byte_rate.to_le_bytes());
    hdr.extend_from_slice(&(channels * 2).to_le_bytes());
    hdr.extend_from_slice(&16u16.to_le_bytes());
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&data_len.to_le_bytes());
    write_correlated_pcm(path, channels as usize, samples as usize);
    let mut full = hdr;
    let pcm = std::fs::read(path).unwrap();
    full.extend_from_slice(&pcm);
    std::fs::write(path, &full).unwrap();
}

pub(crate) fn opts_password(name: &str) -> Option<&'static str> {
    if name.starts_with("hp") || name.starts_with("enc") {
        Some("pw")
    } else {
        None
    }
}

// ── rar-rs reads, WinRAR creates ────────────────────────────────────────────

/// One `we_read_winrar_created_*` case: WinRAR creates an archive with
/// `switches`, then rar-rs must list it, read both members and stream
/// `a.bin` back byte-identically. Split per case so the harness can run
/// them in parallel — each one spends most of its time inside WinRAR.
pub(crate) fn winrar_created_case(switches: &[&str]) {
    let Some(rar_bin) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    write_pattern_file(&a, STREAM_SIZE, 11);
    write_pattern_file(&b, 2 * 1024 * 1024, 13);

    let name = "case.rar";
    let arc = dir.path().join(name);
    let mut cmd = Command::new(&rar_bin);
    cmd.arg("a");
    for sw in switches {
        cmd.arg(sw);
    }
    cmd.arg(&arc).arg(&a).arg(&b);
    cmd.current_dir(dir.path());
    let (ok, out) = run(&mut cmd);
    assert!(ok, "WinRAR failed to create {name}:\n{out}");

    // Read back with rar-rs (password when the switches set one).
    let password = switches.iter().any(|s| s.starts_with("-p")).then_some("pw");
    let first = rar_rs::discover_volumes(&arc)[0].clone();
    let mut rar = match password {
        Some(pw) => ArchiveReader::open_with(&first, OpenOptions::new().password(pw)).unwrap(),
        None => ArchiveReader::open(&first).unwrap(),
    };
    let names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    let a_name = names
        .iter()
        .find(|n| n.ends_with("a.bin"))
        .unwrap_or_else(|| panic!("{name}: member a.bin missing from {names:?}"))
        .clone();
    let b_name = names
        .iter()
        .find(|n| n.ends_with("b.bin"))
        .unwrap_or_else(|| panic!("{name}: member b.bin missing from {names:?}"))
        .clone();
    let extracted_a = rar.read_entry(rar.unique_entry(&a_name).unwrap()).unwrap();
    let extracted_b = rar.read_entry(rar.unique_entry(&b_name).unwrap()).unwrap();
    assert_eq!(
        extracted_a.len(),
        STREAM_SIZE as usize,
        "{name}: a.bin size"
    );
    assert_eq!(
        extracted_b,
        std::fs::read(&b).unwrap(),
        "{name}: b.bin bytes"
    );
    // Verify a.bin content without loading it fully: compare a streamed
    // extraction hash.
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut rar = match password {
        Some(pw) => ArchiveReader::open_with(&first, OpenOptions::new().password(pw)).unwrap(),
        None => ArchiveReader::open(&first).unwrap(),
    };
    rar.extract_entry(rar.unique_entry(&a_name).unwrap(), &out_dir)
        .unwrap();
    assert_eq!(
        file_sha256(&out_dir.join(&a_name)),
        file_sha256(&a),
        "{name}: extracted a.bin differs"
    );
}

// ── >4 GiB single-file creation (P4 acceptance) ─────────────────────────────

/// Create a sparse file of `size` bytes (reads as zeros, allocates almost
/// nothing on disk).
pub(crate) fn create_sparse(path: &Path, size: u64) {
    let f = std::fs::File::create(path).expect("create sparse file");
    f.set_len(size).expect("extend sparse file");
}

/// 32 MiB of repeated text: compressible, uniform head (the incompressible
/// probe must not misfire), large enough to exercise dictionary selection.
pub(crate) fn write_rep_text(path: &Path, size: usize) {
    let block = b"The quick brown fox jumps over the lazy dog 0123456789.\r\n";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        data.extend_from_slice(block);
    }
    data.truncate(size);
    std::fs::write(path, data).unwrap();
}

// ── -ts file times (WinRAR 7.23 aligned) ───────────────────────────────────

pub(crate) fn created_time(path: &Path) -> Option<std::time::SystemTime> {
    #[cfg(windows)]
    {
        std::fs::metadata(path).ok()?.created().ok()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// One streamed-compression interop case: write `stream.bin` with `opts` at
/// `level`, then WinRAR must test and extract it byte-identically and our
/// own reader must round-trip it too. Split per case so the harness can run
/// the cases in parallel — each one spends most of its time inside WinRAR.
pub(crate) fn streamed_compressed_case(opts: WriterOptions, password: Option<&str>, level: u8) {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let _ = unrar;
    let dir = temp_dir();
    let src = dir.path().join("stream.bin");
    write_pattern_file(&src, STREAM_SIZE, 3);
    let name = "stream.rar";
    let arc = dir.path().join(name);
    {
        let mut rar = ArchiveWriter::create_with(&arc, opts).unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(level).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    // Multi-volume archives live in `name.partN.rar` files; the base path
    // itself never exists.
    let first = rar_rs::discover_volumes(&arc)[0].clone();
    let (ok, out) = unrar_test(&first, password);
    assert!(ok, "WinRAR rejected {name}:\n{out}");

    // WinRAR extraction must produce byte-identical data.
    let dest = dir.path().join("winrar-out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&first, &dest, password);
    assert!(ok, "WinRAR failed to extract {name}:\n{out}");
    assert_eq!(
        file_sha256(&dest.join("stream.bin")),
        file_sha256(&src),
        "WinRAR extracted different bytes for {name}"
    );

    // rar-rs must read its own streaming output back too.
    let mut rar = match password {
        Some(pw) => ArchiveReader::open_with(&first, OpenOptions::new().password(pw)).unwrap(),
        None => ArchiveReader::open(&first).unwrap(),
    };
    let out_path = dir.path().join("ours-out");
    std::fs::create_dir_all(&out_path).unwrap();
    rar.extract_entry(rar.unique_entry("stream.bin").unwrap(), &out_path)
        .unwrap();
    assert_eq!(
        file_sha256(&out_path.join("stream.bin")),
        file_sha256(&src),
        "rar-rs round-trip mismatch for {name}"
    );
}
