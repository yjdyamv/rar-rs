#![allow(deprecated)] // legacy constructor family; use create_with_options
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

use rar5::RarArchive;
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
        for dir in ["C:\\Program Files\\WinRAR", "C:\\Program Files (x86)\\WinRAR"] {
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
    let pat: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(7).wrapping_add(seed)).collect();
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

    let cases: Vec<(&str, rar5::CreateOptions)> = vec![
        // Single-volume compressed streaming (spill file path).
        ("stream.rar", rar5::CreateOptions::default()),
        // Multi-volume compressed streaming (chunk splits mid-stream).
        (
            "stream-vol.rar",
            rar5::CreateOptions {
                volume_size: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
        // Encrypted streaming (single-volume, chained CBC).
        (
            "stream-enc.rar",
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                ..Default::default()
            },
        ),
        // Encrypted streaming multi-volume: per-chunk ciphertext CRCs and
        // per-chunk encryption records.
        (
            "stream-enc-vol.rar",
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                volume_size: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
        // Header-encrypted multi-volume + STORE (level 0): exercises the
        // on-disk header accounting in the streaming writer.
        (
            "stream-hp-vol.rar",
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                encrypt_headers: true,
                volume_size: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
        // Header-encrypted multi-volume + compressed.
        (
            "stream-hp-vol-comp.rar",
            rar5::CreateOptions {
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
        let first = rar5::discover_volumes(&arc)[0].clone();
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
        let mut rar = rar5::RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
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
    assert!(ok, "WinRAR failed to extract the solid streaming archive:\n{out}");
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
            rar5::CreateOptions {
                volume_size: Some(vol_size),
                ..Default::default()
            },
        ),
        (
            "hp.rar",
            rar5::CreateOptions {
                password: Some("pw".into()),
                encrypt_headers: true,
                volume_size: Some(vol_size),
                ..Default::default()
            },
        ),
        (
            "enc.rar",
            rar5::CreateOptions {
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
        let volumes = rar5::discover_volumes(&arc);
        assert!(volumes.len() > 2, "{name}: expected several volumes");
        for vol in &volumes[..volumes.len() - 1] {
            let len = std::fs::metadata(vol).unwrap().len();
            assert_eq!(
                len, vol_size,
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
        let first = rar5::discover_volumes(&arc)[0].clone();
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
        assert_eq!(extracted_a.len(), STREAM_SIZE as usize, "{name}: a.bin size");
        assert_eq!(extracted_b, std::fs::read(&b).unwrap(), "{name}: b.bin bytes");
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
#[ignore = "slow: compresses >4 GiB and needs >4 GiB of temp space; run with cargo test --release -- --ignored"]
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
        let mut rar = RarArchive::create(&arc).unwrap();
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
            rar5::ExtractOptions {
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
#[ignore = "slow: stores >4 GiB; run with cargo test --release -- --ignored"]
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
        let mut rar = rar5::RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
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
    let volumes = rar5::discover_volumes(&arc);
    assert!(volumes.len() >= 4, "expected several volumes, got {}", volumes.len());
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
            rar5::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(std::fs::metadata(ours.join("huge.bin")).unwrap().len(), size);
    assert_eq!(file_sha256(&ours.join("huge.bin")), file_sha256(&src));

    // WinRAR test + extract.
    if unrar_bin().is_some() {
        let (ok, out) = unrar_test(&volumes[0], Some("s3cret"));
        assert!(ok, "WinRAR rejected the >4 GiB encrypted volume set:\n{out}");
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
