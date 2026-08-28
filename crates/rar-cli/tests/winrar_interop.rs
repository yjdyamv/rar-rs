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
        let mut rar = rar5::RarArchive::create_with_options(
            &ours,
            rar5::CreateOptions {
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
    let volumes = rar5::discover_volumes(&ours);
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
        let volumes = rar5::discover_volumes(&theirs);
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
            rar5::ExtractOptions {
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
        let mut rar = rar5::RarArchive::create_with_options(
            &ours,
            rar5::CreateOptions {
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
    assert_eq!(entry.header.comp_dict_size, 9);
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
            entry.header.comp_dict_size, 9,
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
        let mut rar = rar5::RarArchive::create_with_options(
            &ours,
            rar5::CreateOptions {
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
    assert!(
        entry.header.ctime.is_some(),
        "our -ts archive must store ctime"
    );
    assert!(
        entry.header.atime.is_some(),
        "our -ts archive must store atime"
    );
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
            entry.header.ctime.is_some() && entry.header.atime.is_some(),
            "WinRAR -ts archive must carry ctime and atime"
        );
        let out_dir = dir.path().join("ours_from_winrar_ts");
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut ar = RarArchive::open(&theirs).unwrap();
        ar.extract_all_with_options(
            &out_dir,
            rar5::ExtractOptions {
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
        let mut rar = rar5::RarArchive::create_with_options(
            &ours,
            rar5::CreateOptions {
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
        assert_eq!(e.header.comp_version, 1, "expected RAR7 (v70) member");
        let bytes = e
            .header
            .dict_size_bytes
            .expect("v70 must carry the byte count");
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
            rar5::ExtractOptions {
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
        rar5::ExtractOptions {
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
        assert_eq!(e.header.comp_version, 1, "expected RAR7 (v70) member");
        let bytes = e
            .header
            .dict_size_bytes
            .expect("v70 must carry the byte count");
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
            rar5::ExtractOptions {
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
        rar5::ExtractOptions {
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
    let volumes_a = rar5::discover_volumes(&first_a);
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
        let mut rar = rar5::RarArchive::create_with_options(
            &set,
            rar5::CreateOptions {
                volume_size: Some(200_000),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 0).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar5::discover_volumes(&set);
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
    let volumes = rar5::discover_volumes(&first);
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
        let mut rar = rar5::RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
                volume_size: Some(50_000),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 0).unwrap();
        rar.close().unwrap();
    }
    let first = dir.path().join("p.part01.rar");
    let volumes = rar5::discover_volumes(&first);
    assert!(
        volumes.len() >= 10,
        "precondition: >= 10 volumes so the writer zero-pads, got {}",
        volumes.len()
    );
    assert!(
        !dir.path().join("p.part1.rar").exists(),
        "the writer must not emit unpadded volume names"
    );

    if let Some(unrar) = unrar_bin() {
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

    if let Some(unrar) = unrar_bin() {
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
