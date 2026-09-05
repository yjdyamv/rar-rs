//! WinRAR interoperability tests (Windows-friendly).
//!
//! Unlike `tests/interop.rs` (which uses the Linux `rar`/`unrar` console
//! tools through `SA_OFFICIAL_RAR` / `SA_OFFICIAL_UNRAR`), this file
//! locates an installed WinRAR (default `C:\Program Files\WinRAR`) and
//! drives its console binaries `Rar.exe` and `UnRAR.exe` directly, so it
//! compiles and runs on Windows out of the box.
//!
//! Every test skips itself (with a note) when no WinRAR installation is
//! found. Point `SA_WINRAR_DIR` at a directory containing `Rar.exe` and
//! `UnRAR.exe` to use a non-standard location.
//!
//! The >4 GiB sparse-file round trips are `#[ignore]`d: they take minutes
//! and multiple GiB of disk. Run them explicitly with
//! `cargo test --release --test winrar_interop -- --ignored`.

#![allow(deprecated)] // fixture archives built through the legacy write facade

use rar_rs::RarArchive;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory containing `Rar.exe` and `UnRAR.exe`, when WinRAR is
/// installed. `None` skips the tests.
fn winrar_dir() -> Option<PathBuf> {
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
    None
}

fn rar_bin() -> Option<PathBuf> {
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
fn rar4_623_bin() -> Option<PathBuf> {
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

fn unrar_bin() -> Option<PathBuf> {
    let dir = winrar_dir()?;
    let exe = if cfg!(windows) { "UnRAR.exe" } else { "unrar" };
    let bin = dir.join(exe);
    bin.exists().then_some(bin)
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Run a command, returning (status, stdout+stderr).
fn run(cmd: &mut Command) -> (bool, String) {
    let out = cmd.output().expect("run command");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// `UnRAR t <archive>` must report success (optionally with a password).
fn unrar_test(path: &Path, password: Option<&str>) -> (bool, String) {
    let mut cmd = Command::new(unrar_bin().expect("unrar"));
    cmd.arg("t").arg("-idq");
    if let Some(pw) = password {
        cmd.arg(format!("-p{pw}"));
    }
    cmd.arg(path);
    run(&mut cmd)
}

/// `UnRAR x <archive> <dest>/` must succeed; returns the output.
fn unrar_extract(path: &Path, dest: &Path, password: Option<&str>) -> (bool, String) {
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
fn write_pattern_file(path: &Path, size: u64, seed: u8) {
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

fn file_sha256(path: &Path) -> String {
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
fn file_sha256_bytes(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 96 MiB of compressible data — comfortably over the streaming
/// compression threshold (64 MiB) and over one small volume.
const STREAM_SIZE: u64 = 96 * 1024 * 1024;

// ── rar-rs creates, WinRAR validates ─────────────────────────────────────────

#[test]
fn winrar_validates_streamed_compressed_archives() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let _ = unrar;
    let dir = temp_dir();
    let src = dir.path().join("stream.bin");
    write_pattern_file(&src, STREAM_SIZE, 3);

    let cases: Vec<(&str, rar_rs::CreateOptions)> = vec![
        // Single-volume compressed streaming (spill file path).
        ("stream.rar", rar_rs::CreateOptions::default()),
        // Multi-volume compressed streaming (chunk splits mid-stream).
        (
            "stream-vol.rar",
            rar_rs::CreateOptions {
                volume_size: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
        // Encrypted streaming (single-volume, chained CBC).
        (
            "stream-enc.rar",
            rar_rs::CreateOptions {
                password: Some("s3cret".into()),
                ..Default::default()
            },
        ),
        // Encrypted streaming multi-volume: per-chunk ciphertext CRCs and
        // per-chunk encryption records.
        (
            "stream-enc-vol.rar",
            rar_rs::CreateOptions {
                password: Some("s3cret".into()),
                volume_size: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
        // Header-encrypted multi-volume + STORE (level 0): exercises the
        // on-disk header accounting in the streaming writer.
        (
            "stream-hp-vol.rar",
            rar_rs::CreateOptions {
                password: Some("s3cret".into()),
                encrypt_headers: true,
                volume_size: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
        // Header-encrypted multi-volume + compressed.
        (
            "stream-hp-vol-comp.rar",
            rar_rs::CreateOptions {
                password: Some("s3cret".into()),
                encrypt_headers: true,
                volume_size: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
    ];

    for (name, opts) in cases {
        let arc = dir.path().join(name);
        let level = if name.contains("hp-vol") && name.ends_with("comp.rar") {
            3
        } else if name.contains("hp-vol") {
            0
        } else {
            3
        };
        {
            let mut rar = RarArchive::create_with_options(&arc, opts.clone()).unwrap();
            rar.add(&src, level).unwrap();
            rar.close().unwrap();
        }
        let password = opts.password.as_deref();
        // Multi-volume archives live in `name.partN.rar` files; the base
        // path itself never exists.
        let first = rar_rs::discover_volumes(&arc)[0].clone();
        let (ok, out) = unrar_test(&first, password);
        assert!(ok, "WinRAR rejected {name}:\n{out}");

        // WinRAR extraction must produce byte-identical data.
        let dest = dir.path().join(format!("out-{name}"));
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
            Some(pw) => RarArchive::open_with_password(&first, pw).unwrap(),
            None => RarArchive::open(&first).unwrap(),
        };
        let out_path = dir.path().join(format!("ours-{name}"));
        std::fs::create_dir_all(&out_path).unwrap();
        rar.extract("stream.bin", &out_path).unwrap();
        assert_eq!(
            file_sha256(&out_path.join("stream.bin")),
            file_sha256(&src),
            "rar-rs round-trip mismatch for {name}"
        );
    }
}

/// Correlated multi-channel samples (small per-sample deltas) of the kind
/// WinRAR's delta filter targets. We emit a delta filter for this data and
/// the real UnRAR must decode it byte-for-byte.
fn write_correlated_pcm(path: &Path, channels: usize, samples: usize) {
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
fn write_wav(path: &Path, channels: u16, samples: u32) {
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

#[test]
fn unrar_reads_our_delta_filtered_output() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let _ = unrar;
    let dir = temp_dir();
    let src = dir.path().join("audio.bin");
    // 16-bit stereo interleaved correlated PCM — our auto-delta detector
    // should select channels=2 and emit a delta-filtered (non-solid) member.
    write_correlated_pcm(&src, 2, 120_000);

    let arc = dir.path().join("delta.rar");
    {
        let mut rar =
            rar_rs::RarArchive::create_with_options(&arc, rar_rs::CreateOptions::default())
                .unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }

    // The real UnRAR must accept and verify our delta-filtered archive.
    let (ok, out) = unrar_test(&arc, None);
    assert!(ok, "UnRAR rejected our delta-filtered archive:\n{out}");

    // And extract it byte-for-byte identical to the source.
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, None);
    assert!(
        ok,
        "UnRAR failed to extract our delta-filtered archive:\n{out}"
    );
    assert_eq!(
        file_sha256(&dest.join("audio.bin")),
        file_sha256(&src),
        "UnRAR extracted different bytes from our delta-filtered archive"
    );

    // rar-rs must read its own delta output back too.
    let ours = dir.path().join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    let mut rar = rar_rs::RarArchive::open(&arc).unwrap();
    rar.extract("audio.bin", &ours).unwrap();
    assert_eq!(
        file_sha256(&ours.join("audio.bin")),
        file_sha256(&src),
        "rar-rs round-trip mismatch for our delta-filtered archive"
    );
}

#[test]
fn rar_rs_reads_winrar_delta_filtered_wav() {
    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let _ = rar;
    let dir = temp_dir();
    let src = dir.path().join("sample.wav");
    // Real WinRAR applies its delta (audio) filter to WAV PCM by default.
    write_wav(&src, 2, 120_000);

    let arc = dir.path().join("winrar-delta.rar");
    let (ok, out) = run(Command::new(rar_bin().unwrap())
        .arg("a")
        .arg("-m5")
        .arg("-idq")
        .arg(&arc)
        .arg("sample.wav")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR failed to create the archive:\n{out}");

    // Our reader must extract WinRAR's delta-filtered WAV byte-for-byte. Look
    // the member up by suffix because WinRAR may store a path prefix.
    let mut r = rar_rs::RarArchive::open(&arc).unwrap();
    let names: Vec<String> = r.namelist().into_iter().map(|s| s.to_string()).collect();
    let member = names
        .iter()
        .find(|n| n.ends_with("sample.wav"))
        .unwrap_or_else(|| panic!("sample.wav not found in {names:?}"))
        .clone();
    let data = r.read(&member).unwrap();
    assert_eq!(
        data,
        std::fs::read(&src).unwrap(),
        "rar-rs read a different WAV than WinRAR archived"
    );
}

#[test]
fn winrar_validates_streamed_solid_archive() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let _ = unrar;
    let dir = temp_dir();
    // Solid chain with a > threshold member: encoder state must carry
    // across chunks and across members.
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    write_pattern_file(&a, STREAM_SIZE, 5);
    write_pattern_file(&b, STREAM_SIZE, 7);
    let arc = dir.path().join("solid.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &arc,
            rar_rs::CreateOptions {
                solid: true,
                blake2: true,
                quick_open: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&a, 3).unwrap();
        rar.add(&b, 3).unwrap();
        rar.close().unwrap();
    }
    let (ok, out) = unrar_test(&arc, None);
    assert!(ok, "WinRAR rejected the solid streaming archive:\n{out}");
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, None);
    assert!(
        ok,
        "WinRAR failed to extract the solid streaming archive:\n{out}"
    );
    assert_eq!(file_sha256(&dest.join("a.bin")), file_sha256(&a));
    assert_eq!(file_sha256(&dest.join("b.bin")), file_sha256(&b));
}

/// Volumes must be byte-exact (`volume_size`, except the last), matching
/// WinRAR's own behavior, for both plain and header-encrypted streaming
/// members.
#[test]
fn streamed_volumes_are_byte_exact() {
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, STREAM_SIZE, 9);
    let vol_size = 16 * 1024 * 1024;

    for (name, opts) in [
        (
            "plain.rar",
            rar_rs::CreateOptions {
                volume_size: Some(vol_size),
                ..Default::default()
            },
        ),
        (
            "hp.rar",
            rar_rs::CreateOptions {
                password: Some("pw".into()),
                encrypt_headers: true,
                volume_size: Some(vol_size),
                ..Default::default()
            },
        ),
        (
            "enc.rar",
            rar_rs::CreateOptions {
                password: Some("pw".into()),
                volume_size: Some(vol_size),
                ..Default::default()
            },
        ),
    ] {
        let arc = dir.path().join(name);
        {
            let mut rar = RarArchive::create_with_options(&arc, opts).unwrap();
            // STORE: the compressible pattern would fit one volume; stored
            // raw it actually fills the volumes.
            rar.add(&src, 0).unwrap();
            rar.close().unwrap();
        }
        let volumes = rar_rs::discover_volumes(&arc);
        assert!(volumes.len() > 2, "{name}: expected several volumes");
        for vol in &volumes[..volumes.len() - 1] {
            let len = std::fs::metadata(vol).unwrap().len();
            assert_eq!(
                len,
                vol_size,
                "{name}: non-final volume {} must be exactly {vol_size} bytes",
                vol.display()
            );
        }
        // Everything must still extract byte-identically (WinRAR-gated).
        if unrar_bin().is_some() {
            let password = opts_password(name);
            let (ok, out) = unrar_test(&volumes[0], password);
            assert!(ok, "WinRAR rejected {name}:\n{out}");
        }
    }
}

fn opts_password(name: &str) -> Option<&'static str> {
    if name.starts_with("hp") || name.starts_with("enc") {
        Some("pw")
    } else {
        None
    }
}

// ── Solid multi-volume (WinRAR 7.23 aligned) ────────────────────────────────

/// Solid + multi-volume creation: the LZ window carries across volume
/// boundaries; WinRAR must be able to test/extract both directions.
#[test]
fn solid_multivolume_interops_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("rand8.bin");
    let mut data = vec![0u8; 8 * 1024 * 1024];
    for chunk in data.chunks_mut(4096) {
        let mut seed = (chunk.as_ptr() as usize) as u64;
        for b in chunk.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 33) as u8;
        }
    }
    std::fs::write(&src, &data).unwrap();
    let small = dir.path().join("s.txt");
    std::fs::write(&small, b"solid volume second member ".repeat(500)).unwrap();

    // Ours -> WinRAR.
    let ours = dir.path().join("ours_sv.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &ours,
            rar_rs::CreateOptions {
                solid: true,
                volume_size: Some(2 * 1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 3).unwrap();
        rar.add(&small, 3).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&ours);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "WinRAR rejected our solid volume set:\n{out}");
        let win = dir.path().join("win_ours_sv");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, None);
        assert!(ok, "WinRAR failed to extract our solid volume set:\n{out}");
        assert_eq!(file_sha256(&win.join("rand8.bin")), file_sha256(&src));
    }

    // WinRAR -> ours.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_sv.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-v2m", "-idq"])
            .arg(&theirs)
            .arg(&src)
            .arg(&small));
        assert!(ok, "WinRAR solid volumes failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&theirs);
        assert!(
            volumes.len() >= 3,
            "expected several volumes, got {}",
            volumes.len()
        );
        let mut ar = RarArchive::open(&volumes[0]).unwrap();
        let names: Vec<String> = ar.namelist().into_iter().map(|s| s.to_string()).collect();
        let bin_name = names
            .iter()
            .find(|n| n.ends_with("rand8.bin"))
            .unwrap_or_else(|| panic!("rand8.bin not found in {names:?}"))
            .clone();
        let data = ar.read(&bin_name).unwrap();
        assert_eq!(data, std::fs::read(&src).unwrap());
    }
}

// ── rar-rs reads, WinRAR creates ────────────────────────────────────────────

#[test]
fn we_read_winrar_created_archives() {
    let Some(rar_bin) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    write_pattern_file(&a, STREAM_SIZE, 11);
    write_pattern_file(&b, 2 * 1024 * 1024, 13);

    let cases: Vec<(&str, Vec<&str>)> = vec![
        // (name, rar switches)
        ("plain.rar", vec!["-m3", "-idq"]),
        ("solid.rar", vec!["-m3", "-s", "-htb", "-idq"]),
        ("enc.rar", vec!["-m3", "-ppw", "-idq"]),
        ("hp.rar", vec!["-m3", "-ppw", "-hp", "-idq"]),
        ("vol.rar", vec!["-m0", "-v16m", "-idq"]),
        ("vol-enc.rar", vec!["-m0", "-v16m", "-ppw", "-idq"]),
        ("vol-hp.rar", vec!["-m0", "-v16m", "-ppw", "-hp", "-idq"]),
    ];
    for (name, switches) in cases {
        let arc = dir.path().join(name);
        let mut cmd = Command::new(&rar_bin);
        cmd.arg("a");
        for sw in &switches {
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
            Some(pw) => RarArchive::open_with_password(&first, pw).unwrap(),
            None => RarArchive::open(&first).unwrap(),
        };
        let names: Vec<String> = rar.namelist().into_iter().map(|s| s.to_string()).collect();
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
        let extracted_a = rar.read(&a_name).unwrap();
        let extracted_b = rar.read(&b_name).unwrap();
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
        let out_dir = dir.path().join(format!("out-{name}"));
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut rar = match password {
            Some(pw) => RarArchive::open_with_password(&first, pw).unwrap(),
            None => RarArchive::open(&first).unwrap(),
        };
        rar.extract(&a_name, &out_dir).unwrap();
        assert_eq!(
            file_sha256(&out_dir.join(&a_name)),
            file_sha256(&a),
            "{name}: extracted a.bin differs"
        );
    }
}

// ── >4 GiB single-file creation (P4 acceptance) ─────────────────────────────

/// Create a sparse file of `size` bytes (reads as zeros, allocates almost
/// nothing on disk).
fn create_sparse(path: &Path, size: u64) {
    let f = std::fs::File::create(path).expect("create sparse file");
    f.set_len(size).expect("extend sparse file");
}

#[test]
#[ignore = "slow: compresses >4 GiB and needs >4 GiB of temp space; the 512 MiB sibling runs in the default suite (crates/rar/tests/large_paths.rs)"]
fn huge_sparse_file_streamed_compression_roundtrips() {
    let dir = temp_dir();
    let size = 4 * 1024 * 1024 * 1024u64 + 4096; // > 4 GiB
    let src = dir.path().join("huge.bin");
    create_sparse(&src, size);

    // rar-rs creates a compressed single-volume archive (the all-zero
    // input compresses to a few MiB, but the encoder must stream all
    // 4 GiB through the spill file).
    let arc = dir.path().join("huge.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&arc, rar_rs::CreateOptions::default()).unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    assert!(
        std::fs::metadata(&arc).unwrap().len() < 64 * 1024 * 1024,
        "all-zero input must compress well"
    );

    // rar-rs round-trip: streamed extraction to disk (raise the default
    // per-member limit; extraction itself is streaming).
    let ours = dir.path().join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    {
        let mut rar = RarArchive::open(&arc).unwrap();
        rar.extract_with_options(
            "huge.bin",
            &ours,
            rar_rs::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    let meta = std::fs::metadata(ours.join("huge.bin")).unwrap();
    assert_eq!(meta.len(), size, "extracted size");
    assert_eq!(file_sha256(&ours.join("huge.bin")), file_sha256(&src));

    // WinRAR must test and extract it too.
    if unrar_bin().is_some() {
        let (ok, out) = unrar_test(&arc, None);
        assert!(ok, "WinRAR rejected the >4 GiB archive:\n{out}");
        let win = dir.path().join("win");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&arc, &win, None);
        assert!(ok, "WinRAR failed to extract the >4 GiB archive:\n{out}");
        let meta = std::fs::metadata(win.join("huge.bin")).unwrap();
        assert_eq!(meta.len(), size, "WinRAR extracted size");
        assert_eq!(
            file_sha256(&win.join("huge.bin")),
            file_sha256(&src),
            "WinRAR extracted different bytes"
        );
    }
}

#[test]
#[ignore = "slow: stores >4 GiB; the 256 MiB encrypted multi-volume sibling runs in the default suite (crates/rar/tests/large_paths.rs)"]
fn huge_sparse_file_streamed_encrypted_multivolume_roundtrips() {
    let dir = temp_dir();
    let size = 4 * 1024 * 1024 * 1024u64 + 8192; // > 4 GiB
    let src = dir.path().join("huge.bin");
    create_sparse(&src, size);

    // STORE (level 0): the all-zero input compresses to a few MiB and
    // would fit a single volume; stored raw it actually spans the volume
    // set. The compressed >4 GiB case is covered by the single-volume
    // test; this one exercises the streaming encrypted multi-volume path
    // (per-chunk ciphertext CRCs, per-chunk encryption records, CBC chain
    // across volume boundaries).
    let arc = dir.path().join("huge.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &arc,
            rar_rs::CreateOptions {
                password: Some("s3cret".into()),
                volume_size: Some(256 * 1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 0).unwrap();
        rar.close().unwrap();
    }
    // Many exact-sized volumes; each carries a ciphertext CRC and an
    // encryption record on every chunk.
    let volumes = rar_rs::discover_volumes(&arc);
    assert!(
        volumes.len() >= 4,
        "expected several volumes, got {}",
        volumes.len()
    );
    for vol in &volumes[..volumes.len() - 1] {
        assert_eq!(
            std::fs::metadata(vol).unwrap().len(),
            256 * 1024 * 1024,
            "non-final volume must be byte-exact"
        );
    }

    // rar-rs self round-trip (streamed extraction, raised limits).
    let ours = dir.path().join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    {
        let mut rar = RarArchive::open_with_password(&arc, "s3cret").unwrap();
        rar.extract_with_options(
            "huge.bin",
            &ours,
            rar_rs::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::metadata(ours.join("huge.bin")).unwrap().len(),
        size
    );
    assert_eq!(file_sha256(&ours.join("huge.bin")), file_sha256(&src));

    // WinRAR test + extract.
    if unrar_bin().is_some() {
        let (ok, out) = unrar_test(&volumes[0], Some("s3cret"));
        assert!(
            ok,
            "WinRAR rejected the >4 GiB encrypted volume set:\n{out}"
        );
        let win = dir.path().join("win");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, Some("s3cret"));
        assert!(ok, "WinRAR failed to extract the >4 GiB volume set:\n{out}");
        assert_eq!(std::fs::metadata(win.join("huge.bin")).unwrap().len(), size);
        assert_eq!(
            file_sha256(&win.join("huge.bin")),
            file_sha256(&src),
            "WinRAR extracted different bytes"
        );
    }
}

/// 32 MiB of repeated text: compressible, uniform head (the incompressible
/// probe must not misfire), large enough to exercise dictionary selection.
fn write_rep_text(path: &Path, size: usize) {
    let block = b"The quick brown fox jumps over the lazy dog 0123456789.\r\n";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        data.extend_from_slice(block);
    }
    data.truncate(size);
    std::fs::write(path, data).unwrap();
}

/// `-md` dictionaries interoperate with WinRAR in both directions.
#[test]
fn dictionary_size_md_interops_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("rep32t.bin");
    write_rep_text(&src, 32 * 1024 * 1024);

    // Our -md64m archive (dict log 9): WinRAR must test and extract it.
    let ours = dir.path().join("ours_md.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &ours,
            rar_rs::CreateOptions {
                dict_size_log: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    let ar = RarArchive::open(&ours).unwrap();
    let entry = ar.get_entry("rep32t.bin").unwrap();
    assert_eq!(entry.comp_dict_size(), 9);
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&ours, None);
        assert!(ok, "WinRAR rejected our -md64m archive:\n{out}");
        let win = dir.path().join("win_ours");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&ours, &win, None);
        assert!(ok, "WinRAR failed to extract our -md64m archive:\n{out}");
        assert_eq!(file_sha256(&win.join("rep32t.bin")), file_sha256(&src));
    }

    // WinRAR's -md64m archive: we must read it back byte-identically.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_md.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-md64m", "-idq"])
            .arg(&theirs)
            .arg(&src));
        assert!(ok, "WinRAR -md64m failed:\n{out}");
        let ar = RarArchive::open(&theirs).unwrap();
        // WinRAR may store the member under a path-derived name; find it.
        let name = ar.namelist()[0].to_string();
        let entry = ar.get_entry(&name).unwrap();
        assert_eq!(
            entry.comp_dict_size(),
            9,
            "WinRAR -md64m should record a 64 MiB dictionary"
        );
        let mut ar = RarArchive::open(&theirs).unwrap();
        let data = ar.read(&name).unwrap();
        assert_eq!(data, std::fs::read(&src).unwrap());
    }
}

// ── -ts file times (WinRAR 7.23 aligned) ───────────────────────────────────

fn created_time(path: &Path) -> Option<std::time::SystemTime> {
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

/// `-ts` timestamps interoperate in both directions: WinRAR restores our
/// stored ctime/atime on `x -ts`, and we parse + restore WinRAR's.
#[test]
fn ts_file_times_interop_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("ts.bin");
    std::fs::write(&src, b"timestamp interop payload ".repeat(100)).unwrap();
    let src_ctime = created_time(&src);

    // Ours -> WinRAR: WinRAR's `x -ts` must restore mtime and (Windows)
    // creation time from our FILE_TIME extra record.
    let ours = dir.path().join("ours_ts.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &ours,
            rar_rs::CreateOptions {
                save_ctime: true,
                save_atime: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    let ar = RarArchive::open(&ours).unwrap();
    let entry = ar.get_entry("ts.bin").unwrap();
    assert!(entry.ctime().is_some(), "our -ts archive must store ctime");
    assert!(entry.atime().is_some(), "our -ts archive must store atime");
    if let Some(unrar) = unrar_bin() {
        let win = dir.path().join("win_ours_ts");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-ts", "-y", "-idq"])
            .arg(&ours)
            .arg(&win));
        assert!(ok, "WinRAR x -ts failed on our archive:\n{out}");
        let extracted = win.join("ts.bin");
        let win_ctime = created_time(&extracted);
        if let (Some(a), Some(b)) = (src_ctime, win_ctime) {
            let diff = a
                .duration_since(b)
                .unwrap_or_else(|_| b.duration_since(a).unwrap());
            assert!(
                diff < std::time::Duration::from_secs(3),
                "WinRAR restored ctime {b:?}, source {a:?}"
            );
        }
    }

    // WinRAR -> ours: parse its FILE_TIME record and restore on extract.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_ts.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-ts", "-idq"])
            .arg(&theirs)
            .arg(&src));
        assert!(ok, "WinRAR -ts failed:\n{out}");
        let ar = RarArchive::open(&theirs).unwrap();
        let name = ar.namelist()[0].to_string();
        let entry = ar.get_entry(&name).unwrap();
        assert!(
            entry.ctime().is_some() && entry.atime().is_some(),
            "WinRAR -ts archive must carry ctime and atime"
        );
        let out_dir = dir.path().join("ours_from_winrar_ts");
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut ar = RarArchive::open(&theirs).unwrap();
        ar.extract_all_with_options(
            &out_dir,
            rar_rs::ExtractOptions {
                set_creation_time: true,
                set_access_time: true,
                ..Default::default()
            },
        )
        .unwrap();
        let extracted = out_dir.join(Path::new(&name).file_name().unwrap());
        let ours_ctime = created_time(&extracted);
        if let (Some(a), Some(b)) = (src_ctime, ours_ctime) {
            let diff = a
                .duration_since(b)
                .unwrap_or_else(|_| b.duration_since(a).unwrap());
            assert!(
                diff < std::time::Duration::from_secs(3),
                "we restored ctime {b:?}, source {a:?}"
            );
        }
    }
}

// ── -os NTFS alternate data streams (Windows only) ──────────────────────────

/// `-os` streams interoperate in both directions (Windows only: alternate
/// data streams are an NTFS concept).
#[cfg(windows)]
#[test]
fn os_streams_interop_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("ads.bin");
    std::fs::write(&src, b"main stream data").unwrap();
    let stream_name = ":custom1";
    let stream_data = b"alternate stream payload".to_vec();
    std::fs::write(format!("{}{}", src.display(), stream_name), &stream_data).unwrap();
    // Verify the stream exists before archiving.
    assert_eq!(
        std::fs::read(format!("{}{}", src.display(), stream_name)).unwrap(),
        stream_data
    );

    // Ours -> WinRAR: `UnRAR x -os` must restore the stream.
    let ours = dir.path().join("ours_os.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &ours,
            rar_rs::CreateOptions {
                save_streams: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    if let Some(unrar) = unrar_bin() {
        let win = dir.path().join("win_os");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-os", "-y", "-idq"])
            .arg(&ours)
            .arg(&win));
        assert!(ok, "WinRAR x -os failed on our archive:\n{out}");
        let restored = std::fs::read(format!("{}{}", win.join("ads.bin").display(), stream_name));
        assert_eq!(
            restored.unwrap(),
            stream_data,
            "WinRAR must restore our stream"
        );
    }

    // WinRAR -> ours: we restore the stream from its -os archive.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_os.rar");
        // Use a relative member path so the stored name stays relative.
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-os", "-idq"])
            .arg(&theirs)
            .arg("ads.bin")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -os failed:\n{out}");
        let out_dir = dir.path().join("ours_os");
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut ar = RarArchive::open(&theirs).unwrap();
        ar.extract_all(&out_dir).unwrap();
        let restored = std::fs::read(format!(
            "{}{}",
            out_dir.join("ads.bin").display(),
            stream_name
        ));
        assert_eq!(
            restored.unwrap(),
            stream_data,
            "we must restore WinRAR's stream"
        );
    }
}

// ── RAR7 (v70) archives: dictionary > 4 GiB ────────────────────────────────

/// WinRAR switches to the RAR7 compression algorithm (v70) when the
/// dictionary exceeds 4 GiB (here: `-md8g` with a >4 GiB source). We must
/// refuse such members by default (WinRAR's 4 GiB dictionary cap) and
/// decode them byte-identically once `-mdx` raises the cap.
#[test]
#[ignore] // slow: >4 GiB source and an 8 GiB dictionary window
fn rar7_v70_archives_decode_with_mdx() {
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    let size = 4 * 1024 * 1024 * 1024u64 + 4096; // > 4 GiB triggers v70 with -md8g
    write_pattern_file(&src, size, 3);

    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let arc = dir.path().join("v70.rar");
    let (ok, out) = run(Command::new(&rar)
        .args(["a", "-md8g", "-m3", "-idq"])
        .arg(&arc)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR -md8g failed:\n{out}");
    // Confirm the member really is v70 with a >4 GiB dictionary (WinRAR
    // encodes the exact size, possibly non-power-of-two).
    {
        let ar = RarArchive::open(&arc).unwrap();
        let name = ar.namelist()[0].to_string();
        let e = ar.get_entry(&name).unwrap();
        assert_eq!(e.comp_version(), 1, "expected RAR7 (v70) member");
        let bytes = e.dict_size_bytes().expect("v70 must carry the byte count");
        assert!(
            bytes > 4 * 1024 * 1024 * 1024,
            "expected a >4 GiB dictionary, got {bytes}"
        );
    }

    // Default extraction cap (4 GiB dictionary) refuses it (unpacked-size
    // limits raised so the dictionary cap is the one that trips).
    let out_dir = dir.path().join("out_default");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut ar = RarArchive::open(&arc).unwrap();
    let err = ar
        .extract_all_with_options(
            &out_dir,
            rar_rs::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("dictionary size"),
        "default cap must refuse the >4 GiB dictionary, got: {err}"
    );

    // -mdx semantics: raising the cap decodes it byte-identically.
    let out_dir = dir.path().join("out_mdx");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut ar = RarArchive::open(&arc).unwrap();
    ar.extract_all_with_options(
        &out_dir,
        rar_rs::ExtractOptions {
            max_unpacked_bytes: None,
            max_total_unpacked_bytes: None,
            max_dict_size: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(file_sha256(&out_dir.join("big.bin")), file_sha256(&src));
}

/// We create RAR7 (v70) archives ourselves: `-md8g` with a >4 GiB source
/// selects the v70 header (compression version 1) with the dictionary
/// capped at 2x the file size (8 GiB here), and the member payload is
/// encoded with the extended 80-entry distance table. Both our extractor
/// and WinRAR's UnRAR must decode it byte-identically.
#[test]
#[ignore] // slow: >4 GiB source and an 8 GiB dictionary window
fn we_create_v70_archives_decode_everywhere() {
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    let size = 4 * 1024 * 1024 * 1024u64 + 4096; // > 4 GiB triggers v70 with -md8g
    write_pattern_file(&src, size, 7);

    // Create with our rar CLI (relative member name, like WinRAR).
    let arc = dir.path().join("v70.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-md8g", "-m3", "-idq"])
        .arg(&arc)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -md8g failed:\n{out}");

    // Confirm the member is v70 with a >4 GiB dictionary.
    {
        let ar = RarArchive::open(&arc).unwrap();
        let name = ar.namelist()[0].to_string();
        let e = ar.get_entry(&name).unwrap();
        assert_eq!(e.comp_version(), 1, "expected RAR7 (v70) member");
        let bytes = e.dict_size_bytes().expect("v70 must carry the byte count");
        assert!(
            bytes > 4 * 1024 * 1024 * 1024,
            "expected a >4 GiB dictionary, got {bytes}"
        );
    }

    // Our extractor decodes it byte-identically.
    let out_dir = dir.path().join("out_ours");
    std::fs::create_dir_all(&out_dir).unwrap();
    {
        let mut ar = RarArchive::open(&arc).unwrap();
        ar.extract_all_with_options(
            &out_dir,
            rar_rs::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                max_dict_size: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(file_sha256(&out_dir.join("big.bin")), file_sha256(&src));

    // WinRAR's UnRAR decodes it byte-identically too (it needs `-mdx8g`
    // to allow the >4 GiB dictionary).
    if let Some(unrar) = unrar_bin() {
        let out_dir = dir.path().join("out_unrar");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(unrar)
            .args(["x", "-idq", "-o+", "-y", "-mdx8g"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR -mdx8g failed:\n{out}");
        assert_eq!(file_sha256(&out_dir.join("big.bin")), file_sha256(&src));
    }
}

/// Long-range matching (WinRAR `-mcl`, applied automatically for
/// -m2..-m5): a 128 MiB file whose second half copies its random first
/// half must compress almost as well as WinRAR's archive (the 64 MiB
/// match distance is far beyond the near window) and decode everywhere.
/// Correctness at reduced scale is locked into the default suite
/// (`crates/rar/tests/large_paths.rs`); this one gates the compression
/// ratio against WinRAR itself.
#[test]
#[ignore] // slow: 128 MiB source, compression + two extractions
fn long_range_matches_winrar_compression_ratio() {
    let dir = temp_dir();
    let src = dir.path().join("pair.bin");
    let half = 64 * 1024 * 1024usize;
    let mut data = vec![0u8; half * 2];
    {
        // Deterministic pseudo-random first half (LCG).
        let mut state = 42u64;
        for b in data[..half].iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (state >> 33) as u8;
        }
        let first = data[..half].to_vec();
        data[half..].copy_from_slice(&first);
    }
    std::fs::write(&src, &data).unwrap();

    // WinRAR reference archive (-md128m, long range search on by default).
    let win_arc = dir.path().join("win.rar");
    let (ok, out) = run(Command::new(rar_bin().expect("rar"))
        .args(["a", "-md128m", "-m3", "-idq"])
        .arg(&win_arc)
        .arg("pair.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR failed:\n{out}");
    let win_size = std::fs::metadata(&win_arc).unwrap().len();

    // Our archive.
    let our_arc = dir.path().join("ours.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-md128m", "-m3", "-idq"])
        .arg(&our_arc)
        .arg("pair.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar failed:\n{out}");
    let our_size = std::fs::metadata(&our_arc).unwrap().len();

    // Compression ratio must be close to WinRAR's (the 64 MiB distant
    // copy compresses); allow 5% slack for sampling-grid effects.
    assert!(
        our_size <= win_size * 105 / 100,
        "long-range ratio too far from WinRAR: ours {our_size} vs WinRAR {win_size}"
    );

    // Our extractor round-trips it.
    let out_dir = dir.path().join("out_ours");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut ar = RarArchive::open(&our_arc).unwrap();
    ar.extract_all_with_options(
        &out_dir,
        rar_rs::ExtractOptions {
            max_unpacked_bytes: None,
            max_total_unpacked_bytes: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(file_sha256(&out_dir.join("pair.bin")), file_sha256(&src));

    // WinRAR's UnRAR decodes it byte-identically too.
    if let Some(unrar) = unrar_bin() {
        let out_dir = dir.path().join("out_unrar");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&our_arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR failed:\n{out}");
        assert_eq!(file_sha256(&out_dir.join("pair.bin")), file_sha256(&src));
    }
}

// ── rv/rc recovery-volume cross-validation (Phase 2.1) ─────────────────────

/// Phase 2.1 cross-validation, direction 1: WinRAR builds the volume set
/// and its `.rev` recovery volumes — both `-rv2` at create time and the
/// standalone `rv` command — we delete a middle volume, and OUR `rc` must
/// rebuild it byte-identically. Both sets use >= 10 volumes, so WinRAR
/// zero-pads the part numbers (part01..partNN); discovery, `.rev`
/// probing and rebuild must handle WinRAR's real naming.
#[test]
fn winrar_rv_then_our_rc_rebuilds_byte_identical() {
    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 1_400_000, 7); // STORE: spans ~14 x 100k volumes

    // (a) Recovery volumes created at archive time with `-rv2`.
    let set_a = dir.path().join("seta.rar");
    let (ok, out) = run(Command::new(&rar)
        .args(["a", "-m0", "-v100k", "-rv2", "-idq"])
        .arg(&set_a)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR -rv2 creation failed:\n{out}");
    let first_a = dir.path().join("seta.part01.rar");
    let volumes_a = rar_rs::discover_volumes(&first_a);
    assert!(
        volumes_a.len() >= 10,
        "precondition: >= 10 volumes so WinRAR zero-pads, got {}",
        volumes_a.len()
    );
    assert!(
        dir.path().join("seta.part01.rev").exists(),
        "WinRAR -rv2 must create padded .rev files"
    );

    // Delete a middle volume; our `rc` rebuilds it byte-identically.
    let victim_a = dir.path().join("seta.part07.rar");
    let victim_bytes_a = std::fs::read(&victim_a).unwrap();
    std::fs::remove_file(&victim_a).unwrap();
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rc", "-idq"])
        .arg(&first_a));
    assert!(ok, "our rar rc failed on WinRAR's padded set:\n{out}");
    assert_eq!(
        std::fs::read(&victim_a).unwrap(),
        victim_bytes_a,
        "our rc must rebuild the WinRAR volume byte-identically"
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first_a, None);
        assert!(ok, "UnRAR rejected the set rebuilt by our rc:\n{out}");
    }

    // (b) Standalone `rv` command on an existing set (default 10%).
    let set_b = dir.path().join("setb.rar");
    let (ok, out) = run(Command::new(&rar)
        .args(["a", "-m0", "-v100k", "-idq"])
        .arg(&set_b)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR creation failed:\n{out}");
    let first_b = dir.path().join("setb.part01.rar");
    let (ok, out) = run(Command::new(&rar).args(["rv", "-idq"]).arg(&first_b));
    assert!(ok, "WinRAR rv failed:\n{out}");
    assert!(
        dir.path().join("setb.part01.rev").exists(),
        "WinRAR rv must create padded .rev files"
    );
    let victim_b = dir.path().join("setb.part05.rar");
    let victim_bytes_b = std::fs::read(&victim_b).unwrap();
    std::fs::remove_file(&victim_b).unwrap();
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rc", "-idq"])
        .arg(&first_b));
    assert!(ok, "our rar rc failed on WinRAR's rv set:\n{out}");
    assert_eq!(
        std::fs::read(&victim_b).unwrap(),
        victim_bytes_b,
        "our rc must rebuild byte-identically"
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first_b, None);
        assert!(ok, "UnRAR rejected the second rebuilt set:\n{out}");
    }

    // Both rebuilt sets must also read back with our own reader.
    for first in [&first_a, &first_b] {
        let mut ar = RarArchive::open(first).unwrap();
        let name = ar
            .namelist()
            .into_iter()
            .find(|n| n.ends_with("big.bin"))
            .unwrap()
            .to_string();
        assert_eq!(ar.read(&name).unwrap(), std::fs::read(&src).unwrap());
    }
}

/// Phase 2.1 cross-validation, direction 2: we build the volume set and
/// its `.rev` recovery volumes with our own `rv`, then WinRAR's `rc` must
/// reconstruct a deleted volume byte-identically (unpadded set: fewer
/// than 10 volumes).
#[test]
fn our_rv_then_winrar_rc_rebuilds_byte_identical() {
    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 700_000, 11); // STORE: 4 x 200k volumes

    // Our volume set (unpadded part1..part4).
    let set = dir.path().join("ours.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &set,
            rar_rs::CreateOptions {
                volume_size: Some(200_000),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 0).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&set);
    assert!(
        (3..10).contains(&volumes.len()),
        "precondition: a small unpadded set, got {}",
        volumes.len()
    );

    // Our `rv` command adds the .rev files (exact count 2).
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rv", "-idq"])
        .arg(&set)
        .arg("2"));
    assert!(ok, "our rar rv failed:\n{out}");
    assert!(dir.path().join("ours.part1.rev").exists());
    assert!(dir.path().join("ours.part2.rev").exists());

    // Delete a middle volume; WinRAR `rc` rebuilds it byte-identically.
    let victim = volumes[1].clone();
    let victim_bytes = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let (ok, out) = run(Command::new(&rar).args(["rc", "-idq"]).arg(&volumes[0]));
    assert!(ok, "WinRAR rc failed on our set:\n{out}");
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        victim_bytes,
        "WinRAR rc must rebuild our volume byte-identically"
    );

    // Both tools must read the rebuilt set.
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected the rebuilt set:\n{out}");
    }
    let mut ar = RarArchive::open(&volumes[0]).unwrap();
    let name = ar
        .namelist()
        .into_iter()
        .find(|n| n.ends_with("big.bin"))
        .unwrap()
        .to_string();
    assert_eq!(ar.read(&name).unwrap(), std::fs::read(&src).unwrap());
}

/// Phase 2.1 cross-validation, direction 3: zero-padded volume sets.
/// WinRAR creates a >= 10 volume set (part01..partNN); our `rv` adds
/// `.rev` files named with the set's zero-padding; then both WinRAR's and
/// our `rc` rebuild deleted volumes byte-identically from the same
/// `.rev` files.
#[test]
fn zero_padded_volume_sets_rv_rc_cross_validate() {
    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 1_500_000, 13); // STORE: ~15 x 100k volumes

    // WinRAR creates the padded set (>= 10 volumes -> part01..partNN).
    let set = dir.path().join("pad.rar");
    let (ok, out) = run(Command::new(&rar)
        .args(["a", "-m0", "-v100k", "-idq"])
        .arg(&set)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR creation failed:\n{out}");
    let first = dir.path().join("pad.part01.rar");
    let volumes = rar_rs::discover_volumes(&first);
    assert!(
        volumes.len() >= 10,
        "precondition: >= 10 volumes so WinRAR zero-pads, got {}",
        volumes.len()
    );

    // Our `rv` adds .rev files with the set's zero-padding.
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rv", "-idq"])
        .arg(&first));
    assert!(ok, "our rar rv failed on the padded set:\n{out}");
    assert!(
        dir.path().join("pad.part01.rev").exists() && dir.path().join("pad.part02.rev").exists(),
        "our .rev names must follow the set's zero-padding"
    );
    assert!(
        !dir.path().join("pad.part1.rev").exists(),
        "no unpadded .rev name may be created"
    );

    // WinRAR `rc` rebuilds a deleted volume from our .rev files.
    let victim = dir.path().join("pad.part09.rar");
    let victim_bytes = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let (ok, out) = run(Command::new(&rar).args(["rc", "-idq"]).arg(&first));
    assert!(ok, "WinRAR rc failed on our padded .rev files:\n{out}");
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        victim_bytes,
        "WinRAR rc must rebuild from our padded .rev byte-identically"
    );

    // Our `rc` rebuilds a different volume from the same .rev files.
    let victim2 = dir.path().join("pad.part12.rar");
    let victim2_bytes = std::fs::read(&victim2).unwrap();
    std::fs::remove_file(&victim2).unwrap();
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rc", "-idq"])
        .arg(&first));
    assert!(ok, "our rar rc failed on the padded set:\n{out}");
    assert_eq!(
        std::fs::read(&victim2).unwrap(),
        victim2_bytes,
        "our rc must rebuild from the same .rev files byte-identically"
    );

    // Both tools validate the final set.
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first, None);
        assert!(ok, "UnRAR rejected the rebuilt padded set:\n{out}");
    }
    let mut ar = RarArchive::open(&first).unwrap();
    let name = ar
        .namelist()
        .into_iter()
        .find(|n| n.ends_with("big.bin"))
        .unwrap()
        .to_string();
    assert_eq!(ar.read(&name).unwrap(), std::fs::read(&src).unwrap());
}

/// The writer zero-pads volume names for sets of 10+ volumes (like
/// WinRAR). WinRAR must test our padded set, our `rc` must rebuild a
/// deleted volume byte-identically, and both tools must read it back.
#[test]
fn our_padded_volume_sets_validate_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 600_000, 21); // STORE: ~12 x 50k volumes

    // Our 12-volume set: the writer emits part01..part12.
    let arc = dir.path().join("p.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &arc,
            rar_rs::CreateOptions {
                volume_size: Some(50_000),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 0).unwrap();
        rar.close().unwrap();
    }
    let first = dir.path().join("p.part01.rar");
    let volumes = rar_rs::discover_volumes(&first);
    assert!(
        volumes.len() >= 10,
        "precondition: >= 10 volumes so the writer zero-pads, got {}",
        volumes.len()
    );
    assert!(
        !dir.path().join("p.part1.rar").exists(),
        "the writer must not emit unpadded volume names"
    );

    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first, None);
        assert!(ok, "UnRAR rejected our padded volume set:\n{out}");
    }

    // Our `rv` adds .rev files with the set's padding, then our `rc`
    // rebuilds a deleted padded volume byte-identically.
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .arg("rv")
        .arg(&first)
        .args(["2", "-idq"]));
    assert!(ok, "our rar rv failed on our padded set:\n{out}");
    assert!(
        dir.path().join("p.part01.rev").exists(),
        "our .rev names must follow the set's padding"
    );
    let victim = dir.path().join("p.part05.rar");
    let victim_bytes = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rc", "-idq"])
        .arg(&first));
    assert!(ok, "our rar rc failed on our padded set:\n{out}");
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        victim_bytes,
        "our rc must rebuild our padded volume byte-identically"
    );

    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first, None);
        assert!(ok, "UnRAR rejected the rebuilt padded set:\n{out}");
    }
    let mut ar = RarArchive::open(&first).unwrap();
    let name = ar
        .namelist()
        .into_iter()
        .find(|n| n.ends_with("big.bin"))
        .unwrap()
        .to_string();
    assert_eq!(ar.read(&name).unwrap(), std::fs::read(&src).unwrap());
}

/// The `force_v70` seam writes legal RAR7 (v70) archives at small scale
/// (the real trigger needs a > 4 GiB source). WinRAR must test and
/// extract them byte-identically — no `-mdx` needed below the 4 GiB cap.
#[test]
fn our_small_dict_v70_archives_decode_with_winrar() {
    let Some(_unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("v70.bin");
    write_pattern_file(&src, 4 * 1024 * 1024, 17);

    let arc = dir.path().join("v70.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &arc,
            rar_rs::CreateOptions {
                dict_size_bytes: Some(6 * 1024 * 1024), // non-power: exercises the 1/32 bits
                force_v70: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    // Confirm the member really is v70.
    let ar = RarArchive::open(&arc).unwrap();
    let name = ar
        .namelist()
        .into_iter()
        .find(|n| n.ends_with("v70.bin"))
        .unwrap()
        .to_string();
    let e = ar.get_entry(&name).unwrap();
    assert_eq!(e.comp_version(), 1, "expected a v70 member");
    assert_eq!(e.dict_size_bytes(), Some(6 * 1024 * 1024));

    // WinRAR tests and extracts it byte-identically.
    let (ok, out) = unrar_test(&arc, None);
    assert!(ok, "UnRAR rejected our small-dict v70 archive:\n{out}");
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, None);
    assert!(ok, "UnRAR failed to extract our v70 archive:\n{out}");
    assert_eq!(
        file_sha256(&dest.join(&name)),
        file_sha256(&src),
        "WinRAR extracted different bytes from our v70 archive"
    );
}

/// `rar a -ma7` (our extension: force RAR7/v70 at any dictionary size)
/// must produce archives WinRAR tests and extracts byte-identically.
#[test]
fn cli_ma7_archives_decode_with_winrar() {
    let Some(_unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("ma7.bin");
    write_pattern_file(&src, 4 * 1024 * 1024, 19);
    let arc = dir.path().join("ma7.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma7", "-idq"])
        .arg(&arc)
        .arg("ma7.bin")
        .current_dir(dir.path()));
    assert!(ok, "rar a -ma7 failed:\n{out}");
    // The member really is v70 (per-member 2x-file cap floors the dict).
    let ar = RarArchive::open(&arc).unwrap();
    let e = ar.get_entry("ma7.bin").unwrap();
    assert_eq!(e.comp_version(), 1, "-ma7 must force v70");
    let (ok, out) = unrar_test(&arc, None);
    assert!(ok, "UnRAR rejected our -ma7 archive:\n{out}");
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, None);
    assert!(ok, "UnRAR failed to extract our -ma7 archive:\n{out}");
    assert_eq!(
        file_sha256(&dest.join("ma7.bin")),
        file_sha256(&src),
        "WinRAR extracted different bytes"
    );
}

// ── Phase 2.2: extended interaction matrix ───────────────────────────────
//
// Combinations called out as "untested but cheap to expose real byte-level
// deviations": filter + encrypted header, filter + multi-volume, RAR5 vs
// RAR7, recovery record + encryption, symlinks / ADS streams, >4 GiB single
// file, and the solid + filter boundary. Every direction is gated on a real
// WinRAR install so the default `cargo test` suite still runs anywhere.

/// Filter (delta/x86) + encrypted header (`-hp`): both directions.
///
/// WinRAR creates a delta-filtered WAV under a `-hp` archive; rar-rs must
/// decrypt the headers and decode the delta filter byte-for-byte. rar-rs
/// creates a delta-filtered member under header encryption; WinRAR's
/// `UnRAR` must test and extract it byte-for-byte (it needs the password).
#[test]
fn filtered_member_with_encrypted_header_interops() {
    let dir = temp_dir();
    let src = dir.path().join("audio.wav");
    write_wav(&src, 2, 120_000);

    // WinRAR -> ours (header-encrypted, delta-filtered).
    if let Some(rar) = rar_bin() {
        let arc = dir.path().join("win_hp.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-hpsecret", "-m5", "-idq"])
            .arg(&arc)
            .arg("audio.wav")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -hp delta failed:\n{out}");
        let mut ar = RarArchive::open_with_password(&arc, "secret").unwrap();
        let name = ar.namelist()[0].to_string();
        assert_eq!(
            ar.read(&name).unwrap(),
            std::fs::read(&src).unwrap(),
            "rar-rs read a different WAV from WinRAR's -hp archive"
        );
    }

    // Ours -> WinRAR (header-encrypted, auto-delta-filtered).
    let arc = dir.path().join("ours_hp.rar");
    {
        let mut rar = RarArchive::create_with_options(
            &arc,
            rar_rs::CreateOptions {
                password: Some("secret".into()),
                encrypt_headers: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 5).unwrap();
        rar.close().unwrap();
    }
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&arc, Some("secret"));
        assert!(ok, "UnRAR rejected our -hp delta archive:\n{out}");
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&arc, &dest, Some("secret"));
        assert!(ok, "UnRAR failed to extract our -hp delta archive:\n{out}");
        assert_eq!(
            file_sha256(&dest.join("audio.wav")),
            file_sha256(&src),
            "WinRAR extracted different bytes from our -hp delta archive"
        );
    }
}

/// Filter (delta/x86) + multi-volume (`-v`): both directions.
#[test]
fn filtered_member_with_multivolume_interops() {
    let dir = temp_dir();
    // A 20 MiB correlated-PCM WAV: compresses hard (delta filter) and spans
    // several 4 MiB volumes, exercising the filter + volume-boundary path.
    let src = dir.path().join("big.wav");
    write_wav(&src, 2, 2_500_000);
    let vol_size = 4 * 1024 * 1024;

    // WinRAR -> ours (delta-filtered, multi-volume).
    if let Some(rar) = rar_bin() {
        let arc = dir.path().join("win_fv.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-m5", "-v4m", "-idq"])
            .arg(&arc)
            .arg("big.wav")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -v delta failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&arc);
        let mut ar = RarArchive::open(&volumes[0]).unwrap();
        let name = ar.namelist()[0].to_string();
        assert_eq!(
            ar.read(&name).unwrap(),
            std::fs::read(&src).unwrap(),
            "rar-rs read a different WAV from WinRAR's multi-volume delta archive"
        );
    }

    // Ours -> WinRAR (auto-delta-filtered, multi-volume).
    let arc = dir.path().join("ours_fv.rar");
    {
        let mut rar = RarArchive::create_with_options(
            &arc,
            rar_rs::CreateOptions {
                volume_size: Some(vol_size),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 5).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&arc);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected our multi-volume delta archive:\n{out}");
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &dest, None);
        assert!(
            ok,
            "UnRAR failed to extract our multi-volume delta archive:\n{out}"
        );
        assert_eq!(
            file_sha256(&dest.join("big.wav")),
            file_sha256(&src),
            "WinRAR extracted different bytes from our multi-volume delta archive"
        );
    }
}

/// RAR5 (v50) vs RAR7 (v70) for the *same* data: both must decode
/// byte-for-byte with WinRAR, and the v70 member must carry `comp_version`
/// 1. This is the byte-level guarantee behind the "RAR5 vs RAR7" parity claim.
#[test]
fn rar5_vs_rar7_same_data_decode_everywhere() {
    let Some(_unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("cmp.bin");
    write_pattern_file(&src, 6 * 1024 * 1024, 23);

    // RAR5 (default).
    let v50 = dir.path().join("v50.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-idq"])
        .arg(&v50)
        .arg("cmp.bin")
        .current_dir(dir.path()));
    assert!(ok, "rar a (v50) failed:\n{out}");
    // RAR7 (v70) forced at small scale.
    let v70 = dir.path().join("v70.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma7", "-idq"])
        .arg(&v70)
        .arg("cmp.bin")
        .current_dir(dir.path()));
    assert!(ok, "rar a -ma7 failed:\n{out}");

    let e50 = {
        let mut ar = RarArchive::open(&v50).unwrap();
        let n = ar.namelist()[0].to_string();
        let e = ar.get_entry(&n).unwrap();
        assert_eq!(e.comp_version(), 0, "v50 must stay comp_version 0");
        ar.read(&n).unwrap()
    };
    let e70 = {
        let mut ar = RarArchive::open(&v70).unwrap();
        let n = ar.namelist()[0].to_string();
        let e = ar.get_entry(&n).unwrap();
        assert_eq!(e.comp_version(), 1, "v70 must be comp_version 1");
        ar.read(&n).unwrap()
    };
    // Both encode the same source; decoded bytes must match the source.
    assert_eq!(e50, std::fs::read(&src).unwrap(), "v50 decoded mismatch");
    assert_eq!(e70, std::fs::read(&src).unwrap(), "v70 decoded mismatch");

    // WinRAR must decode both byte-for-byte.
    let v50_out = dir.path().join("out_v50");
    let v70_out = dir.path().join("out_v70");
    std::fs::create_dir_all(&v50_out).unwrap();
    std::fs::create_dir_all(&v70_out).unwrap();
    for (arc, dest) in [(&v50, &v50_out), (&v70, &v70_out)] {
        let (ok, out) = unrar_extract(arc, dest, None);
        assert!(ok, "UnRAR failed on {arc:?}:\n{out}");
        // The member is stored flat as `cmp.bin`, so it extracts directly.
        let extracted = dest.join("cmp.bin");
        assert_eq!(
            file_sha256(&extracted),
            file_sha256(&src),
            "WinRAR extracted different bytes from {arc:?}"
        );
    }
}

/// Recovery record (`-rr`) + encryption (`-p`): both directions. Single
/// volume only (WinRAR forbids inline recovery records on multi-volume
/// sets, which use `.rev` instead).
#[test]
fn recovery_record_with_encryption_interops() {
    let dir = temp_dir();
    let src = dir.path().join("rr.bin");
    write_pattern_file(&src, 3 * 1024 * 1024, 31);

    // WinRAR -> ours: -rr + -p, then we read with the password.
    if let Some(rar) = rar_bin() {
        let arc = dir.path().join("win_rr.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-rr", "-psecret", "-idq"])
            .arg(&arc)
            .arg("rr.bin")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -rr -p failed:\n{out}");
        let mut ar = RarArchive::open_with_password(&arc, "secret").unwrap();
        let name = ar.namelist()[0].to_string();
        assert_eq!(
            ar.read(&name).unwrap(),
            std::fs::read(&src).unwrap(),
            "rar-rs read a different file from WinRAR's -rr -p archive"
        );
    }

    // Ours -> WinRAR: recovery_percent + password.
    let arc = dir.path().join("ours_rr.rar");
    {
        let mut rar = RarArchive::create_with_options(
            &arc,
            rar_rs::CreateOptions {
                password: Some("secret".into()),
                recovery_percent: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&arc, Some("secret"));
        assert!(ok, "UnRAR rejected our -rr -p archive:\n{out}");
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&arc, &dest, Some("secret"));
        assert!(ok, "UnRAR failed to extract our -rr -p archive:\n{out}");
        assert_eq!(
            file_sha256(&dest.join("rr.bin")),
            file_sha256(&src),
            "WinRAR extracted different bytes from our -rr -p archive"
        );
    }
}

/// Solid chain with a filtered (delta/x86) member at the boundary: both
/// directions. A filtered member must be written standalone (non-solid)
/// even inside a solid archive, so the neighbours must still decode
/// byte-for-byte.
#[test]
fn solid_chain_with_filtered_boundary_interops() {
    let dir = temp_dir();
    let code = dir.path().join("lib.dll");
    // Synthetic x86-ish code with E8/E8E9 patterns so our auto-x86 filter
    // (and WinRAR's) fires.
    let mut dll = Vec::with_capacity(2 * 1024 * 1024);
    let mut x = 0x1234_5678u32;
    while dll.len() < 2 * 1024 * 1024 {
        x = x.wrapping_mul(2654435761).wrapping_add(0x9E37_79B9);
        dll.push((x & 0xFF) as u8);
        if dll.len() % 37 == 0 {
            dll.push(0xE8); // CALL rel32
            dll.extend_from_slice(&(0x0010_2000u32).to_le_bytes());
        }
    }
    std::fs::write(&code, &dll).unwrap();
    let text = dir.path().join("doc.txt");
    std::fs::write(
        &text,
        b"plain text neighbour in the solid chain ".repeat(40_000),
    )
    .unwrap();

    // WinRAR -> ours: -s with mixed code + text.
    if let Some(rar) = rar_bin() {
        let arc = dir.path().join("win_solid.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-m5", "-idq"])
            .arg(&arc)
            .arg("lib.dll")
            .arg("doc.txt")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -s mixed failed:\n{out}");
        let mut ar = RarArchive::open(&arc).unwrap();
        let names: Vec<String> = ar.namelist().into_iter().map(|s| s.to_string()).collect();
        let code_name = names
            .iter()
            .find(|n| n.ends_with("lib.dll"))
            .unwrap()
            .clone();
        let text_name = names
            .iter()
            .find(|n| n.ends_with("doc.txt"))
            .unwrap()
            .clone();
        assert_eq!(
            ar.read(&code_name).unwrap(),
            std::fs::read(&code).unwrap(),
            "rar-rs read a different dll from WinRAR's solid archive"
        );
        assert_eq!(
            ar.read(&text_name).unwrap(),
            std::fs::read(&text).unwrap(),
            "rar-rs read a different txt from WinRAR's solid archive"
        );
    }

    // Ours -> WinRAR: -s with a filtered (.dll) member.
    let arc = dir.path().join("ours_solid.rar");
    {
        let mut rar = RarArchive::create_with_options(
            &arc,
            rar_rs::CreateOptions {
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&code, 5).unwrap();
        rar.add(&text, 5).unwrap();
        rar.close().unwrap();
    }
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&arc, None);
        assert!(
            ok,
            "UnRAR rejected our solid archive with a filtered member:\n{out}"
        );
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&arc, &dest, None);
        assert!(ok, "UnRAR failed to extract our solid archive:\n{out}");
        let code_out = std::fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().ends_with("lib.dll"))
            .unwrap()
            .path();
        assert_eq!(
            file_sha256(&code_out),
            file_sha256(&code),
            "WinRAR extracted a different dll from our solid archive"
        );
    }
}

/// Solid chain split modifiers (`-sv` / `-se`) interoperate with WinRAR in
/// both directions. `-sv` resets the solid statistics at every volume
/// boundary; `-se` resets them when the file extension changes.
#[test]
fn solid_reset_volume_interops_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("rand8.bin");
    let mut data = vec![0u8; 8 * 1024 * 1024];
    for chunk in data.chunks_mut(4096) {
        let mut seed = (chunk.as_ptr() as usize) as u64;
        for b in chunk.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 33) as u8;
        }
    }
    std::fs::write(&src, &data).unwrap();
    let small = dir.path().join("s.txt");
    std::fs::write(&small, b"solid volume second member ".repeat(500)).unwrap();

    // WinRAR -> ours: -s -sv multi-volume.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_sv.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-sv", "-v2m", "-idq"])
            .arg(&theirs)
            .arg("rand8.bin")
            .arg("s.txt")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -s -sv failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&theirs);
        assert!(
            volumes.len() >= 3,
            "expected several volumes, got {}",
            volumes.len()
        );
        let mut ar = RarArchive::open(&volumes[0]).unwrap();
        let names: Vec<String> = ar.namelist().into_iter().map(|s| s.to_string()).collect();
        let bin = names
            .iter()
            .find(|n| n.ends_with("rand8.bin"))
            .unwrap()
            .clone();
        assert_eq!(ar.read(&bin).unwrap(), std::fs::read(&src).unwrap());
    }

    // Ours -> WinRAR: -sv multi-volume.
    let ours = dir.path().join("ours_sv.rar");
    {
        let mut rar = RarArchive::create_with_options(
            &ours,
            rar_rs::CreateOptions {
                solid: true,
                solid_reset: rar_rs::SolidReset::PerVolume,
                volume_size: Some(2 * 1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 3).unwrap();
        rar.add(&small, 3).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&ours);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected our -sv volume set:\n{out}");
        let win = dir.path().join("win_ours_sv");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, None);
        assert!(ok, "UnRAR failed to extract our -sv volume set:\n{out}");
        assert_eq!(file_sha256(&win.join("rand8.bin")), file_sha256(&src));
    }
}

#[test]
fn solid_reset_extension_interops_with_winrar() {
    let dir = temp_dir();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.bin");
    let c = dir.path().join("c.txt");
    std::fs::write(&a, b"alpha text block ".repeat(20_000)).unwrap();
    std::fs::write(&b, vec![0xABu8; 1_000_000]).unwrap();
    std::fs::write(&c, b"gamma text block ".repeat(20_000)).unwrap();

    // WinRAR -> ours: -s -se multi-volume (groups reset on extension).
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_se.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-se", "-v1m", "-idq"])
            .arg(&theirs)
            .arg("a.txt")
            .arg("b.bin")
            .arg("c.txt")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -s -se failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&theirs);
        let mut ar = RarArchive::open(&volumes[0]).unwrap();
        let names: Vec<String> = ar.namelist().into_iter().map(|s| s.to_string()).collect();
        for (name, src) in [("a.txt", &a), ("b.bin", &b), ("c.txt", &c)] {
            let n = names.iter().find(|m| m.ends_with(name)).unwrap().clone();
            assert_eq!(
                ar.read(&n).unwrap(),
                std::fs::read(src).unwrap(),
                "rar-rs read a different {name} from WinRAR's -se archive"
            );
        }
    }

    // Ours -> WinRAR: -se (reset the solid chain on an extension change;
    // input order is preserved, we no longer sort by extension).
    let ours = dir.path().join("ours_se.rar");
    {
        let mut rar = RarArchive::create_with_options(
            &ours,
            rar_rs::CreateOptions {
                solid: true,
                solid_reset: rar_rs::SolidReset::PerExtension,
                volume_size: Some(1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&a, 3).unwrap();
        rar.add(&b, 3).unwrap();
        rar.add(&c, 3).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&ours);
    // Order must follow the call sequence (a.txt, b.bin, c.txt): -se resets
    // the solid chain on an extension change but must not reorder members by
    // extension, which would diverge from WinRAR.
    {
        let ar = RarArchive::open(&volumes[0]).unwrap();
        assert_eq!(
            ar.namelist(),
            vec!["a.txt", "b.bin", "c.txt"],
            "-se must preserve input order, not sort by extension"
        );
    }
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected our -se archive:\n{out}");
        let win = dir.path().join("win_ours_se");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, None);
        assert!(ok, "UnRAR failed to extract our -se archive:\n{out}");
        for (name, src) in [("a.txt", &a), ("b.bin", &b), ("c.txt", &c)] {
            assert_eq!(
                file_sha256(&win.join(name)),
                file_sha256(src),
                "WinRAR extracted a different {name} from our -se archive"
            );
        }
    }
}

/// CLI `-sd` (dependent solid volumes: keep the solid statistics across
/// volume boundaries, disabling the per-volume reset) interoperates with
/// WinRAR in both directions. Exercises the CLI `-sd` switch end-to-end
/// (its `normalize_switch` path maps `-sd` to `--solid-reset=continuous`),
/// which the library-API solid tests do not touch.
#[test]
fn cli_sd_dependent_volumes_interops_with_winrar() {
    let dir = temp_dir();
    // A few compressible files each sharing a long common text prefix plus
    // a deterministic pseudo-random tail: the common prefix gives the solid
    // chain something to share across volume boundaries (the observable
    // effect of `-sd`), while the random tail keeps the set big enough to
    // split into several volumes. Deterministic LCG keeps it cross-platform.
    let mut files = Vec::new();
    let mut data = Vec::new();
    for i in 0..4u32 {
        let p = dir.path().join(format!("m{i}.dat"));
        let mut body = Vec::new();
        let prefix =
            format!("COMMONPREFIX record {i}: the quick brown fox jumps over the lazy dog.\n");
        for _ in 0..20_000 {
            body.extend_from_slice(prefix.as_bytes());
        }
        let mut seed = (i as u64)
            .wrapping_mul(0x9E3779B97F4A7C15)
            .wrapping_add(0x1234567);
        for _ in 0..400_000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            body.push((seed >> 33) as u8);
        }
        std::fs::write(&p, &body).unwrap();
        data.push((p.file_name().unwrap().to_string_lossy().into_owned(), body));
        files.push(p);
    }

    // Ours -> WinRAR: our own `rar` binary with `-s -sd` multi-volume.
    let ours = dir.path().join("cli_sd.rar");
    {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_rar"));
        cmd.args(["a", "-s", "-sd", "-idq", "-v100k"])
            .arg(&ours)
            .args(files.iter().map(|p| p.file_name().unwrap()))
            .current_dir(dir.path());
        let (ok, out) = run(&mut cmd);
        assert!(ok, "our rar -s -sd failed:\n{out}");
    }
    let volumes = rar_rs::discover_volumes(&ours);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected our -sd dependent volume set:\n{out}");
        let win = dir.path().join("win_ours_cli_sd");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, None);
        assert!(ok, "UnRAR failed to extract our -sd volume set:\n{out}");
        for (name, src) in &data {
            assert_eq!(
                file_sha256(&win.join(name)),
                file_sha256_bytes(src),
                "WinRAR extracted a different {name} from our -sd set"
            );
        }
    }

    // WinRAR -> ours: `-s -sd` multi-volume dependent set read back.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_cli_sd.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-sd", "-v100k", "-idq"])
            .arg(&theirs)
            .args(files.iter().map(|p| p.file_name().unwrap()))
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -s -sd failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&theirs);
        let mut ar = RarArchive::open(&volumes[0]).unwrap();
        for (name, src) in &data {
            assert_eq!(
                ar.read(name).unwrap(),
                *src,
                "rar-rs read a different {name} from WinRAR's -sd set"
            );
        }
    }
}

/// Symlink members (`-ol`) round-trip with rar-rs and decode through WinRAR
/// as redirects (no data). Unix-only source symlinks; Windows runs WinRAR
/// to confirm the redirect archive validates (the symlink target is not
/// recreated by WinRAR, but the member must test cleanly with an empty
/// data stream).
#[cfg(unix)]
#[test]
fn symlink_member_roundtrips_and_winrar_reads_redirect() {
    let dir = temp_dir();
    let src = dir.path().join("lnk");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("target.txt"), b"target").unwrap();
    std::os::unix::fs::symlink("target.txt", src.join("lnk.txt")).unwrap();

    let arc = dir.path().join("ol.rar");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ol", "-idq"])
        .arg(&arc)
        .arg("lnk")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // rar-rs restores the symlink on extract.
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let mut rar = RarArchive::open(&arc).unwrap();
    rar.extract_all(&out).unwrap();
    assert_eq!(
        std::fs::read_link(out.join("lnk/lnk.txt")).unwrap(),
        std::path::Path::new("target.txt")
    );

    // WinRAR must test the redirect archive (empty data stream).
    if let Some(_unrar) = unrar_bin() {
        let (ok, out_log) = unrar_test(&arc, None);
        assert!(ok, "UnRAR rejected our -ol symlink archive:\n{out_log}");
    }
}

/// RAR5 creation of a >4 GiB single file: an all-zero source compresses to a few
/// MiB but the encoder must stream all 4 GiB through the spill file. WinRAR
/// must test and extract byte-for-byte. `#[ignore]`d: needs >4 GiB of temp
/// space and a few minutes.
#[test]
#[ignore = "slow: compresses >4 GiB and needs >4 GiB of temp space"]
fn rar5_huge_single_file_decodes_with_winrar() {
    let dir = temp_dir();
    let size = 4 * 1024 * 1024 * 1024u64 + 4096; // > 4 GiB
    let src = dir.path().join("huge.bin");
    create_sparse(&src, size);

    let arc = dir.path().join("huge.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&arc, rar_rs::CreateOptions::default()).unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    assert!(
        std::fs::metadata(&arc).unwrap().len() < 64 * 1024 * 1024,
        "all-zero input must compress well"
    );

    // rar-rs round-trip (streamed extraction, raised limits).
    let ours = dir.path().join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    {
        let mut rar = RarArchive::open(&arc).unwrap();
        rar.extract_with_options(
            "huge.bin",
            &ours,
            rar_rs::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::metadata(ours.join("huge.bin")).unwrap().len(),
        size
    );
    assert_eq!(file_sha256(&ours.join("huge.bin")), file_sha256(&src));

    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&arc, None);
        assert!(ok, "WinRAR rejected the >4 GiB archive:\n{out}");
        let win = dir.path().join("win");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&arc, &win, None);
        assert!(ok, "WinRAR failed to extract the >4 GiB archive:\n{out}");
        assert_eq!(std::fs::metadata(win.join("huge.bin")).unwrap().len(), size);
        assert_eq!(
            file_sha256(&win.join("huge.bin")),
            file_sha256(&src),
            "WinRAR extracted different bytes"
        );
    }
}

/// We create a RAR4 (`-ma4`) archive containing a directory tree — nested
/// directories, an empty directory, and a non-ASCII directory name — and
/// both our extractor and WinRAR's UnRAR must see the same tree: the empty
/// directory must come back as a real directory (RAR4 encodes directories
/// in the FILE_HEAD window bits, not just the host attribute), and every
/// member's bytes must match the source.
#[test]
fn we_create_rar4_directory_trees_winrar_valid() {
    let dir = temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("sub/deep")).unwrap();
    std::fs::create_dir(src.join("sub/emptydir")).unwrap();
    std::fs::create_dir(src.join("资料")).unwrap();
    std::fs::write(src.join("top.txt"), b"top-level file").unwrap();
    std::fs::write(src.join("sub/mid.txt"), b"mid level").unwrap();
    std::fs::write(src.join("sub/deep/leaf.txt"), b"leaf content here").unwrap();
    std::fs::write(src.join("资料/note.txt"), b"unicode note").unwrap();

    let arc = dir.path().join("tree4.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m3", "-idq"])
        .arg(&arc)
        .arg("src")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 (directory tree) failed:\n{out}");

    // Our own reader: exact UTF-8 names, directory flags, contents.
    {
        let mut ar = RarArchive::open(&arc).unwrap();
        let mut names: Vec<String> = ar.namelist().into_iter().map(str::to_string).collect();
        names.sort();
        let mut expected: Vec<String> = [
            "src",
            "src/sub",
            "src/sub/deep",
            "src/sub/emptydir",
            "src/sub/deep/leaf.txt",
            "src/sub/mid.txt",
            "src/top.txt",
            "src/资料",
            "src/资料/note.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        expected.sort();
        assert_eq!(names, expected);
        for name in ["src", "src/sub/emptydir", "src/资料"] {
            assert!(ar.get_entry(name).unwrap().is_dir(), "{name} must be a dir");
        }
        for (name, bytes) in [
            ("src/top.txt", b"top-level file".as_slice()),
            ("src/sub/deep/leaf.txt", b"leaf content here".as_slice()),
            ("src/资料/note.txt", b"unicode note".as_slice()),
        ] {
            assert_eq!(&ar.read(name).unwrap(), bytes);
        }
    }

    // WinRAR's UnRAR must accept the archive and reproduce the tree.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(ok, "UnRAR t rejected our -ma4 directory archive:\n{out}");

        let out_dir = dir.path().join("out_unrar");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x failed on our -ma4 directory archive:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("src/sub/deep/leaf.txt")),
            file_sha256(&src.join("sub/deep/leaf.txt"))
        );
        assert_eq!(
            file_sha256(&out_dir.join("src/资料/note.txt")),
            file_sha256(&src.join("资料/note.txt"))
        );
        assert!(
            out_dir.join("src/sub/emptydir").is_dir(),
            "the empty directory must extract as a directory"
        );
        assert!(
            out_dir.join("src/资料").is_dir(),
            "the unicode directory must extract as a directory"
        );
    }
}

/// We create a RAR4 archive with member-level encryption (`-ma4 -p`) and
/// WinRAR's UnRAR must decrypt it: `t` and `x` with the password succeed and
/// reproduce the source bytes, while a wrong or missing password fails.
#[test]
fn we_create_rar4_encrypted_members_winrar_valid() {
    let dir = temp_dir();
    let src = dir.path().join("secret.bin");
    let mut content = Vec::with_capacity(200_000);
    let mut seed = 12345u32;
    while content.len() < 200_000 {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        content.push((seed >> 16) as u8);
    }
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("enc.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-psecret", "-m3", "-idq"])
        .arg(&arc)
        .arg("secret.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -p failed:\n{out}");

    // Our own reader decrypts with the password and rejects the wrong one.
    {
        let mut ar = RarArchive::open_with_password(&arc, "secret").unwrap();
        assert_eq!(ar.read("secret.bin").unwrap(), content);
        let mut ar = RarArchive::open_with_password(&arc, "wrong").unwrap();
        assert!(ar.read("secret.bin").is_err(), "wrong password must fail");
    }

    // WinRAR's UnRAR must decrypt byte-identically.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar)
            .args(["t", "-idq", "-psecret"])
            .arg(&arc));
        assert!(ok, "UnRAR t -psecret failed:\n{out}");
        let (ok, _) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(!ok, "UnRAR t without password must fail");

        let out_dir = dir.path().join("out_unrar");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y", "-psecret"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x -psecret failed:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("secret.bin")),
            file_sha256(&src),
            "WinRAR decrypted different bytes"
        );
    }
}

/// We create a RAR4 archive with header encryption (`-ma4 -hp`) and WinRAR's
/// UnRAR must decrypt the headers: `t`/`x` with the password succeed and
/// reproduce the source bytes, while a wrong or missing password fails even
/// to list (the member headers are encrypted, so the scan needs the key).
#[test]
fn we_create_rar4_header_encrypted_winrar_valid() {
    let dir = temp_dir();
    let src = dir.path().join("classified.bin");
    let content = b"top-secret payload guarded by -hp header encryption\n".repeat(4000);
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("hpenctest.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-hpsword", "-m3", "-idq"])
        .arg(&arc)
        .arg("classified.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -hp failed:\n{out}");

    // Our own reader decrypts the headers with the password.
    {
        let mut ar = RarArchive::open_with_password(&arc, "sword").unwrap();
        assert_eq!(ar.read("classified.bin").unwrap(), content);
        // Wrong password fails at open (the header scan cannot decrypt).
        assert!(RarArchive::open_with_password(&arc, "wrong").is_err());
        assert!(RarArchive::open(&arc).is_err());
    }

    // WinRAR's UnRAR must decrypt and verify byte-identically.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar)
            .args(["t", "-idq", "-psword"])
            .arg(&arc));
        assert!(ok, "UnRAR t -hp -psword failed:\n{out}");
        let (ok, _) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(!ok, "UnRAR t -hp without password must fail");

        let out_dir = dir.path().join("out_unrar_hp");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y", "-psword"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x -hp -psword failed:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("classified.bin")),
            file_sha256(&src),
            "WinRAR decrypted different bytes"
        );
    }
}

/// We create a RAR4 m5 archive on word-random text and WinRAR's UnRAR must
/// decode the PPMd blocks: modern WinRAR (5.x/6.x) no longer *produces*
/// RAR4 PPMd, but its RAR3 decoder still reads it, so this is the one-way
/// interop check that our PPMd member encoding is real RAR3 PPMd. The m5
/// member must also be markedly smaller than the m3 LZ-only member on the
/// same text (the PPMd pass wins on context-rich data).
#[test]
fn we_create_rar4_ppmd_text_winrar_valid() {
    let dir = temp_dir();
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
    ];
    let mut content = Vec::with_capacity(500_000);
    let mut seed = 12345u32;
    let mut n = 0u32;
    while content.len() < 450_000 {
        let mut line = format!("record {n:06}: ").into_bytes();
        for _ in 0..10 {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            line.extend_from_slice(words[(seed >> 27) as usize % words.len()].as_bytes());
            line.push(b' ');
        }
        line.push(b'\n');
        content.extend_from_slice(&line);
        n += 1;
    }
    let src = dir.path().join("textmix.txt");
    std::fs::write(&src, &content).unwrap();

    let lz_arc = dir.path().join("lz3.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m3", "-idq"])
        .arg(&lz_arc)
        .arg("textmix.txt")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -m3 failed:\n{out}");

    let arc = dir.path().join("ppmd5.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m5", "-idq"])
        .arg(&arc)
        .arg("textmix.txt")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -m5 failed:\n{out}");

    let lz_size = std::fs::metadata(&lz_arc).unwrap().len();
    let m5_size = std::fs::metadata(&arc).unwrap().len();
    assert!(
        m5_size * 3 < lz_size * 2,
        "m5 PPMd must beat m3 LZSS on text: LZ={lz_size} m5={m5_size}"
    );

    // Our own reader round-trips the PPMd member.
    {
        let mut ar = RarArchive::open(&arc).unwrap();
        assert_eq!(ar.list()[0].method(), 5);
        assert_eq!(ar.read("textmix.txt").unwrap(), content);
    }

    // WinRAR's UnRAR decodes the PPMd blocks byte-identically.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(ok, "UnRAR t rejected our PPMd archive:\n{out}");

        let out_dir = dir.path().join("out_ppmd");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x failed on our PPMd archive:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("textmix.txt")),
            file_sha256(&src),
            "WinRAR decoded different bytes from our PPMd member"
        );
    }
}

/// RAR4 inline recovery records (`-ma4 -rr`) interoperate both ways with
/// WinRAR 6.23 (the last RAR4 writer): WinRAR's `rar r` repairs damage
/// from OUR NEWSUB (0x7a) record byte-identically, and OUR repair path
/// rebuilds a damaged WinRAR-made RAR4 RR archive.
#[test]
fn rar4_recovery_record_interops_with_winrar() {
    let dir = temp_dir();
    // Pseudo-random payload (NOT the periodic pattern: WinRAR 6.23's RAR4
    // repair mis-rebuilds periodic data whether the record is its own or
    // ours, so a byte-identical repair check needs aperiodic bytes).
    let src = dir.path().join("rr4.bin");
    let mut content = Vec::with_capacity(500 * 1024);
    let mut seed = 41u32;
    while content.len() < 500 * 1024 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        content.push((seed >> 24) as u8);
    }
    std::fs::write(&src, &content).unwrap();

    // A stored RAR4 member's payload starts right after the file header,
    // which sits after the 7-byte signature + 13-byte main header. The
    // file header's own head_size field (offset +5 within it) tells where.
    fn payload_offset(raw: &[u8]) -> usize {
        let fh = 7 + 13;
        assert_eq!(&raw[..7], b"Rar!\x1a\x07\x00");
        let hsize = u16::from_le_bytes([raw[fh + 5], raw[fh + 6]]) as usize;
        fh + hsize
    }

    // Ours -> WinRAR: create with -ma4 -rr10%, damage a payload sector, and
    // both OUR repair and WinRAR's `rar r` rebuild it.
    let arc = dir.path().join("our_rr4.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m0", "-rr10%", "-idq"])
        .arg(&arc)
        .arg("rr4.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -rr10% failed:\n{out}");

    // Our own repair round-trip: damage -> fix -> byte-identical.
    let mut damaged = std::fs::read(&arc).unwrap();
    let start = payload_offset(&damaged);
    damaged[start + 8000..start + 8000 + 128].fill(0x5a);
    let damaged_path = dir.path().join("our_rr4_damaged.rar");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let fixed_path = dir.path().join("our_rr4_fixed.rar");
    let repaired =
        rar_rs::repair_legacy_archive_path(&damaged_path, &fixed_path).expect("our repair");
    assert!(repaired, "our repair must find and fix the damage");
    assert_eq!(
        std::fs::read(&fixed_path).unwrap(),
        std::fs::read(&arc).unwrap(),
        "our repair must restore the archive byte-identically"
    );

    // WinRAR repairs the same damage from our recovery record (6.23 only:
    // 7.23 cannot repair RAR4 recovery records).
    if let Some(rar) = rar4_623_bin() {
        let (ok, out) = run(Command::new(&rar)
            .args(["r", "-y", "-idq"])
            .arg("our_rr4_damaged.rar")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR r failed on our RAR4 RR archive:\n{out}");
        let win_fixed = dir.path().join("fixed.our_rr4_damaged.rar");
        assert!(
            win_fixed.exists(),
            "WinRAR must write fixed.our_rr4_damaged.rar"
        );
        let out_dir = dir.path().join("win_rr4_out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(unrar_bin().unwrap())
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&win_fixed)
            .arg(&out_dir));
        assert!(ok, "UnRAR x of the WinRAR-fixed archive failed:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("rr4.bin")),
            file_sha256(&src),
            "WinRAR rebuilt different bytes from our RAR4 RR record"
        );
    }

    // WinRAR -> ours: WinRAR 6.23 creates the record, we repair damage.
    if let Some(rar) = rar4_623_bin() {
        let warc = dir.path().join("win_rr4.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-ma4", "-m0", "-rr10%", "-idq"])
            .arg(&warc)
            .arg("rr4.bin")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -ma4 -rr10% failed:\n{out}");
        let mut wdamaged = std::fs::read(&warc).unwrap();
        let wstart = payload_offset(&wdamaged);
        wdamaged[wstart + 12345..wstart + 12345 + 96].fill(0xa5);
        let wdamaged_path = dir.path().join("win_rr4_damaged.rar");
        std::fs::write(&wdamaged_path, &wdamaged).unwrap();
        let wfixed_path = dir.path().join("win_rr4_fixed.rar");
        let repaired =
            rar_rs::repair_legacy_archive_path(&wdamaged_path, &wfixed_path).expect("our repair");
        assert!(repaired, "our repair must fix the WinRAR RAR4 RR archive");
        let mut ar = RarArchive::open(&wfixed_path).unwrap();
        assert_eq!(
            ar.read("rr4.bin").unwrap(),
            std::fs::read(&src).unwrap(),
            "we repaired WinRAR's RAR4 RR archive to different bytes"
        );
    }
}

/// RAR4 auto filter (`-ma4`): a delta-transformable member (16-bit stereo
/// samples) must fire the RAR3 DELTA filter record, and WinRAR's UnRAR must
/// decode the filtered member byte-identically. (WinRAR 6.23's RAR4 writer
/// no longer emits VM filters, so this is a one-way interop check.)
#[test]
fn we_create_rar4_delta_filtered_member_winrar_valid() {
    let dir = temp_dir();
    // 16-bit stereo random-walk samples: channels = 4 (2 ch x 2 bytes).
    let mut content = Vec::with_capacity(600_000);
    let mut l = 0i16;
    let mut r = 0i16;
    let mut seed = 42u32;
    while content.len() < 600_000 {
        l = l.wrapping_add(((seed >> 16) & 0x3f) as i16 - 30);
        r = r.wrapping_add(((seed >> 8) & 0x3f) as i16 - 20);
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        content.extend_from_slice(&l.to_le_bytes());
        content.extend_from_slice(&r.to_le_bytes());
    }
    let src = dir.path().join("audio.bin");
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("delta.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m3", "-idq"])
        .arg(&arc)
        .arg("audio.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 (delta) failed:\n{out}");

    // The filter must have won decisively: the archive is far smaller than
    // the raw samples (the filter-record bytes sit unaligned in the
    // bitstream, so size is the reliable fingerprint).
    let raw = std::fs::read(&arc).unwrap();
    assert!(
        (raw.len() as u64) * 3 < content.len() as u64,
        "auto delta filter must have fired (member barely compressed)"
    );

    // Our own reader round-trips the filtered member.
    {
        let mut ar = RarArchive::open(&arc).unwrap();
        assert_eq!(ar.read("audio.bin").unwrap(), content);
    }

    // WinRAR decodes it byte-identically.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(
            ok,
            "UnRAR t rejected our delta-filtered RAR4 archive:\n{out}"
        );
        let out_dir = dir.path().join("out_delta");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x failed on our delta-filtered archive:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("audio.bin")),
            file_sha256(&src),
            "WinRAR decoded different bytes from our delta-filtered member"
        );
    }
}

/// RAR4 solid m5 on a run of near-identical text files: the run is coded
/// with a shared PPMd model (members 2.. continue it), and WinRAR's UnRAR
/// must decode every member byte-identically. WinRAR 6.23's RAR4 writer
/// never produced PPMd, so this is one-way interop.
#[test]
fn we_create_rar4_solid_ppmd_text_winrar_valid() {
    let dir = temp_dir();
    let mut content = Vec::new();
    for chapter in 1..=4u8 {
        let mut body = Vec::with_capacity(240_000);
        for line in 0..2200u32 {
            body.extend_from_slice(
                format!(
                    "chapter {chapter} line {line:05}: shared boilerplate that repeats across every chapter of this archive body body body tail tail\n"
                )
                .as_bytes(),
            );
        }
        let src = dir.path().join(format!("chap{chapter}.txt"));
        std::fs::write(&src, &body).unwrap();
        content.push((format!("chap{chapter}.txt"), body));
    }

    let arc = dir.path().join("solidppmd.rar");
    let args = vec!["a", "-s", "-ma4", "-m5", "-idq"];
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rar"));
    cmd.args(&args).arg(&arc).current_dir(dir.path());
    for i in 1..=4 {
        cmd.arg(format!("chap{i}.txt"));
    }
    let (ok, out) = run(&mut cmd);
    assert!(ok, "our rar -s -ma4 -m5 failed:\n{out}");

    // Our own reader round-trips the chain.
    {
        let mut ar = RarArchive::open(&arc).unwrap();
        for (name, body) in &content {
            assert_eq!(&ar.read(name).unwrap(), body, "{name} solid-PPMd mismatch");
        }
    }

    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(ok, "UnRAR t rejected our solid-PPMd RAR4 archive:\n{out}");
        let out_dir = dir.path().join("out_solidppmd");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x failed on our solid-PPMd archive:\n{out}");
        for (name, body) in &content {
            let got = std::fs::read(out_dir.join(name)).unwrap();
            assert_eq!(&got, body, "WinRAR decoded {name} differently");
        }
    }
}
