use std::io::Write;

use rar_rs::{CompressionLevel, EntryWriteOptions};

use crate::support::{
    RAR_CLI, UNRAR_CLI, cli_names, make_temp_dir, pseudo_random_bytes, set_mtime_ago,
};
// ── WinRAR CLI parity batch 3: -sl/-sm/-ed, -tn/-to, -si, -tk, -p-/-c-, ──
// ── -ierr, -ad ────────────────────────────────────────────────────────────

#[test]
fn cli_size_and_empty_dir_filters() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("small.txt"), b"12345").unwrap();
    std::fs::write(dir.path().join("big.txt"), vec![b'x'; 5000]).unwrap();
    std::fs::create_dir_all(dir.path().join("emptydir")).unwrap();
    std::fs::create_dir_all(dir.path().join("fulldir")).unwrap();
    std::fs::write(dir.path().join("fulldir").join("inner.txt"), b"i").unwrap();

    // -sl1k: only files smaller than 1 KiB (directories always pass).
    let archive = dir.path().join("sl.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-sl1k", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        ["emptydir", "fulldir", "fulldir/inner.txt", "small.txt"]
    );

    // -sm1k: only files larger than 1 KiB.
    let archive = dir.path().join("sm.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-sm1k", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["big.txt", "emptydir", "fulldir"]);

    // -ed: empty directories are not stored.
    let archive = dir.path().join("ed.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ed", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        ["big.txt", "fulldir", "fulldir/inner.txt", "small.txt"]
    );
}

#[test]
fn cli_period_filters_tn_to_match_winrar() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("new.txt"), b"n").unwrap();
    std::fs::write(dir.path().join("old.txt"), b"o").unwrap();
    set_mtime_ago(&dir.path().join("old.txt"), 5400); // 1.5 hours ago

    // -tn1h: only files newer than 1 hour.
    let archive = dir.path().join("tn.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt"]);

    // -to1h: only files older than 1 hour.
    let archive = dir.path().join("to.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-to1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["old.txt"]);

    // Multiple filters combine with AND: 1h < age <= 2h.
    let archive = dir.path().join("tnandto.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn2h", "-to1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["old.txt"]);

    // Compound period and modifier: -tnc1h30m parses and filters on the
    // creation time (both files were just created, so both match).
    let archive = dir.path().join("tnc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tnc1h30m", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt", "old.txt"]);

    // -to with an empty period matches everything (age >= 0).
    let archive = dir.path().join("to0.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-to", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt", "old.txt"]);

    // No match: WinRAR exits 10 and does not create the archive. old.txt
    // (1.5 h old) deterministically fails the 1 s filter.
    let archive = dir.path().join("none.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn1s", "-idq"])
        .arg(&archive)
        .arg("old.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(10), "no-match must exit with code 10");
    assert!(!archive.exists(), "no-match must not create the archive");
}

/// Exit codes must distinguish the failure categories scripts care about:
/// wrong password (11), CRC mismatch (3) and a missing archive (2).
#[test]
fn cli_exit_codes_distinguish_failure_categories() {
    let dir = make_temp_dir();

    // Wrong password on an encrypted member: exit 11.
    let encrypted = dir.path().join("enc.rar");
    let mut writer = rar_rs::ArchiveWriter::create_with(
        &encrypted,
        rar_rs::WriterOptions::new().password("hunter2"),
    )
    .unwrap();
    writer
        .add_bytes(
            "secret.bin",
            b"top secret payload",
            EntryWriteOptions::new().compression_level(CompressionLevel::STORE),
        )
        .unwrap();
    writer.finish().unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["t", "-pwrong", "-idq"])
        .arg(&encrypted)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(11), "wrong password must exit 11");

    // A damaged STORE payload: exit 3 (CRC).
    let damaged = dir.path().join("crc.rar");
    let payload = b"known payload for the crc damage test".to_vec();
    let mut writer = rar_rs::ArchiveWriter::create(&damaged).unwrap();
    writer
        .add_bytes(
            "d.bin",
            &payload,
            EntryWriteOptions::new().compression_level(CompressionLevel::STORE),
        )
        .unwrap();
    writer.finish().unwrap();
    let mut bytes = std::fs::read(&damaged).unwrap();
    let pos = bytes
        .windows(payload.len())
        .position(|window| window == payload.as_slice())
        .expect("payload bytes in archive");
    bytes[pos] ^= 0xFF;
    std::fs::write(&damaged, &bytes).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["t", "-idq"])
        .arg(&damaged)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(3), "damaged payload must exit 3");

    // Missing archive: generic fatal (2).
    let missing = dir.path().join("missing.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["t", "-idq"])
        .arg(&missing)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2), "missing archive must exit 2");
}

#[test]
fn cli_stdin_name_reads_stdin() {
    let dir = make_temp_dir();
    let archive = dir.path().join("si.rar");
    let mut child = std::process::Command::new(RAR_CLI)
        .args(["a", "-siin.txt", "-idq"])
        .arg(&archive)
        .stdin(std::process::Stdio::piped())
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello-stdin")
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<String>>(),
        ["in.txt"]
    );
    let in_id = rar.unique_entry("in.txt").unwrap();
    assert_eq!(rar.read_entry(in_id).unwrap(), b"hello-stdin");
}

#[test]
fn cli_keep_time_preserves_archive_mtime() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("tk.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    // Pin the archive mtime instead of sleeping: a broken -tk (which would
    // stamp "now") then differs by years, not by a sub-second sliver.
    let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    std::fs::File::options()
        .write(true)
        .open(&archive)
        .unwrap()
        .set_modified(past)
        .unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tk", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let after = std::fs::metadata(&archive).unwrap().modified().unwrap();
    assert_eq!(
        after, past,
        "-tk must keep the archive mtime unchanged, got {after:?}"
    );
}

#[test]
fn cli_clear_password_and_no_comment_switches() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    // Bare -p must not silently create an unencrypted archive when secure
    // no-echo prompting is unavailable.
    let bare_archive = dir.path().join("bare-p.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-p", "-idq"])
        .arg(&bare_archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!bare_archive.exists());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("secure no-echo password prompt"),
        "unexpected bare -p error: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // unrar rejects the same unsafe prompt form before attempting to open.
    let out = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-p"])
        .arg("missing.rar")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("secure no-echo password prompt"));

    // A separated long-option value is a real password, not a bare prompt.
    let long_password_archive = dir.path().join("long-password.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "--password", "secret", "-idq"])
        .arg(&long_password_archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open_with(
        &long_password_archive,
        rar_rs::OpenOptions::new().password("secret"),
    )
    .unwrap();
    let f_id = rar.unique_entry("f.txt").unwrap();
    assert_eq!(rar.read_entry(f_id).unwrap(), b"x");

    // The attached WinRAR form remains supported.
    let attached_password_archive = dir.path().join("attached-password.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-psecret", "-idq"])
        .arg(&attached_password_archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open_with(
        &attached_password_archive,
        rar_rs::OpenOptions::new().password("secret"),
    )
    .unwrap();
    let f_id = rar.unique_entry("f.txt").unwrap();
    assert_eq!(rar.read_entry(f_id).unwrap(), b"x");

    // A long password option with no following value is still rejected.
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(dir.path().join("bare-long.rar"))
        .arg("f.txt")
        .arg("--password")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("secure no-echo password prompt"));

    // -p- creates an unencrypted archive (readable without a password).
    let archive = dir.path().join("pm.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-p-", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let f_id = rar.unique_entry("f.txt").unwrap();
    assert_eq!(rar.read_entry(f_id).unwrap(), b"x");

    // -c- is accepted on create.
    let archive = dir.path().join("nc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-c-", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["f.txt"]);
}

#[test]
fn cli_err_switch_routes_messages_to_stderr() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("ierr.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-ierr"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "-ierr must send messages to stderr, stdout had: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Created"),
        "-ierr must send the status message to stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_append_dir_extracts_under_archive_name() {
    let dir = make_temp_dir();
    let archive = dir.path().join("ad.rar");
    {
        let mut rar =
            rar_rs::ArchiveWriter::create_with(&archive, rar_rs::WriterOptions::default()).unwrap();
        rar.add_bytes(
            "f.txt",
            b"x",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-ad", "-idq"])
        .arg(&archive)
        .args(["--dest"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        out.join("ad").join("f.txt").exists(),
        "-ad must extract under a subdirectory named after the archive"
    );
}

// ── -os NTFS alternate data streams (Windows only) ──────────────────────────

/// `rar a -os` stores a file's NTFS streams and `rar x -os` restores them.
#[cfg(windows)]
#[test]
fn cli_save_streams_roundtrips_ntfs_ads() {
    let dir = make_temp_dir();
    let src = dir.path().join("ads.bin");
    std::fs::write(&src, b"main stream data").unwrap();
    let stream = format!("{}{}", src.display(), ":meta");
    std::fs::write(&stream, b"alternate payload").unwrap();

    let archive = dir.path().join("ads.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-os", "-idq"])
        .arg(&archive)
        .arg("ads.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-os", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    let restored = format!("{}{}", out.join("ads.bin").display(), ":meta");
    assert_eq!(std::fs::read(restored).unwrap(), b"alternate payload");
}

/// `-p` streams are encrypted individually (`-os -ppw`); extraction with the
/// password restores them, extraction without it fails instead of writing
/// plaintext.
#[cfg(windows)]
#[test]
fn cli_save_streams_encrypts_with_password() {
    let dir = make_temp_dir();
    let src = dir.path().join("ads.bin");
    std::fs::write(&src, b"main stream data").unwrap();
    let stream = format!("{}{}", src.display(), ":secret");
    std::fs::write(&stream, b"encrypted alternate payload").unwrap();

    let archive = dir.path().join("ads_p.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-os", "-ppw", "-idq"])
        .arg(&archive)
        .arg("ads.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-os", "-ppw", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    let restored = format!("{}{}", out.join("ads.bin").display(), ":secret");
    assert_eq!(
        std::fs::read(restored).unwrap(),
        b"encrypted alternate payload"
    );

    let no_pw = dir.path().join("out_no_pw");
    std::fs::create_dir_all(&no_pw).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-os", "-idq", "--dest"])
        .arg(&no_pw)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "extraction without the password must fail"
    );
    assert!(
        !no_pw.join("ads.bin").exists()
            || std::fs::read(format!("{}{}", no_pw.join("ads.bin").display(), ":secret")).is_err(),
        "no plaintext stream may be written without the password"
    );
}

// ── -oh hard links ──────────────────────────────────────────────────────────

/// `rar a -oh` stores the second path of a hard-link group as a redirect
/// (zero packed bytes) and extraction recreates one on-disk file; `-ma4`
/// has no redirect records and stores both files in full, like WinRAR.
#[cfg(any(unix, windows))]
#[test]
fn cli_hardlink_flag_stores_redirects() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("h1.txt"), b"hardlink body").unwrap();
    std::fs::hard_link(dir.path().join("h1.txt"), dir.path().join("h2.txt")).unwrap();

    let archive = dir.path().join("oh.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-oh", "-idq"])
        .arg(&archive)
        .args(["h1.txt", "h2.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let reader = rar_rs::ArchiveReader::open(&archive).unwrap();
    let packed = |name: &str| {
        reader
            .entry(reader.unique_entry(name).unwrap())
            .unwrap()
            .compressed_size()
    };
    assert!(packed("h1.txt") > 0);
    assert_eq!(
        packed("h2.txt"),
        0,
        "the second hard link must be a redirect"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(out.join("h1.txt"))
            .unwrap();
        f.write_all(b"!").unwrap();
    }
    assert_eq!(
        std::fs::read(out.join("h2.txt")).unwrap(),
        b"hardlink body!",
        "extraction must recreate the hard link"
    );

    // RAR4 has no redirect records: `-ma4 -oh` stores both members fully.
    let archive4 = dir.path().join("oh4.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-oh", "-idq"])
        .arg(&archive4)
        .args(["h1.txt", "h2.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let reader4 = rar_rs::ArchiveReader::open(&archive4).unwrap();
    let packed4 = |name: &str| {
        reader4
            .entry(reader4.unique_entry(name).unwrap())
            .unwrap()
            .compressed_size()
    };
    assert!(packed4("h1.txt") > 0 && packed4("h2.txt") > 0);
}

// ── -oi identical files ─────────────────────────────────────────────────────

/// `rar a -oi` stores the first identical file and a reference for the
/// rest (zero packed bytes); extraction restores the exact bytes. The
/// default 64 KiB threshold, `-oi1:<size>` overrides, `-oi-` and RAR4
/// (which has no redirect records) are covered too.
#[cfg(any(unix, windows))]
#[test]
fn cli_identical_flag_stores_references() {
    let dir = make_temp_dir();
    let data = pseudo_random_bytes(64 * 1024, 11);
    std::fs::write(dir.path().join("i1.bin"), &data).unwrap();
    std::fs::write(dir.path().join("i2.bin"), &data).unwrap();
    let other = pseudo_random_bytes(64 * 1024, 12);
    std::fs::write(dir.path().join("other.bin"), &other).unwrap();

    let archive = dir.path().join("oi.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-oi", "-idq"])
        .arg(&archive)
        .args(["i1.bin", "i2.bin", "other.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let reader = rar_rs::ArchiveReader::open(&archive).unwrap();
    let packed = |name: &str| {
        reader
            .entry(reader.unique_entry(name).unwrap())
            .unwrap()
            .compressed_size()
    };
    assert!(packed("i1.bin") > 0);
    assert_eq!(
        packed("i2.bin"),
        0,
        "the second identical file is a reference"
    );
    assert!(
        packed("other.bin") > 0,
        "same-size different bytes are not referenced"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(out.join("i2.bin")).unwrap(), data);

    // -oi- disables the dedup.
    let off = dir.path().join("off.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-oi-", "-idq"])
        .arg(&off)
        .args(["i1.bin", "i2.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let reader = rar_rs::ArchiveReader::open(&off).unwrap();
    assert!(
        reader
            .entry(reader.unique_entry("i2.bin").unwrap())
            .unwrap()
            .compressed_size()
            > 0
    );

    // RAR4 has no redirect records: -ma4 -oi stores both files in full.
    let rar4 = dir.path().join("oi4.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-oi", "-idq"])
        .arg(&rar4)
        .args(["i1.bin", "i2.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let reader = rar_rs::ArchiveReader::open(&rar4).unwrap();
    assert!(
        reader
            .entry(reader.unique_entry("i2.bin").unwrap())
            .unwrap()
            .compressed_size()
            > 0
    );

    // -oi1:<minsize> lowers the 64 KiB default threshold.
    let small = pseudo_random_bytes(4096, 13);
    std::fs::write(dir.path().join("s1.bin"), &small).unwrap();
    std::fs::write(dir.path().join("s2.bin"), &small).unwrap();
    let lowered = dir.path().join("lowered.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-oi1:1k", "-idq"])
        .arg(&lowered)
        .args(["s1.bin", "s2.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let reader = rar_rs::ArchiveReader::open(&lowered).unwrap();
    assert_eq!(
        reader
            .entry(reader.unique_entry("s2.bin").unwrap())
            .unwrap()
            .compressed_size(),
        0
    );
}

/// `-oi3` lists identical groups and creates no archive; `-oi4` lists the
/// bare duplicate names.
#[cfg(any(unix, windows))]
#[test]
fn cli_identical_listing_modes() {
    let dir = make_temp_dir();
    let data = pseudo_random_bytes(64 * 1024, 21);
    std::fs::write(dir.path().join("g1.bin"), &data).unwrap();
    std::fs::write(dir.path().join("g2.bin"), &data).unwrap();
    std::fs::write(dir.path().join("g3.bin"), &data).unwrap();
    std::fs::write(
        dir.path().join("unique.bin"),
        pseudo_random_bytes(64 * 1024, 22),
    )
    .unwrap();

    let dummy = dir.path().join("dummy.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-oi3"])
        .arg(&dummy)
        .args(["g1.bin", "g2.bin", "g3.bin", "unique.bin"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("g1.bin") && text.contains("g3.bin"), "{text}");
    assert!(!text.contains("unique.bin"), "{text}");
    assert!(text.contains("2 found."), "{text}");
    assert!(!dummy.exists(), "-oi3 must not create an archive");

    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-oi4"])
        .arg(&dummy)
        .args(["g1.bin", "g2.bin", "g3.bin", "unique.bin"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("g2.bin") && text.contains("g3.bin"), "{text}");
    assert!(
        !text.contains("g1.bin"),
        "-oi4 skips the first file: {text}"
    );
    assert!(!dummy.exists(), "-oi4 must not create an archive");
}

// ── -om Mark of the Web, -me switch surface ─────────────────────────────────

/// `rar x -om` copies the archive's Zone.Identifier stream to extracted
/// files (zone only by default, every field with `-om1`, filtered by
/// `-om=<ext>`), and `unrar x` does the same.
#[cfg(windows)]
#[test]
fn cli_mark_of_the_web_propagation() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"body").unwrap();
    let archive = dir.path().join("motw.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let motw: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/p\r\n";
    let archive_stream = format!("{}{}", archive.display(), ":Zone.Identifier");
    std::fs::write(&archive_stream, motw).unwrap();

    let extract = |switch: Option<&str>, name: &str| {
        let out = dir.path().join(name);
        std::fs::create_dir_all(&out).unwrap();
        let mut command = std::process::Command::new(RAR_CLI);
        command.arg("x");
        if let Some(switch) = switch {
            command.arg(switch);
        }
        let status = command
            .args(["-idq", "--dest"])
            .arg(&out)
            .arg(&archive)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(format!(
            "{}{}",
            out.join("f.txt").display(),
            ":Zone.Identifier"
        ))
        .ok()
    };

    assert_eq!(
        extract(Some("-om"), "zone").as_deref(),
        Some(b"[ZoneTransfer]\r\nZoneId=3\r\n".as_slice()),
        "-om propagates only the security zone"
    );
    assert_eq!(
        extract(Some("-om1"), "full").as_deref(),
        Some(motw),
        "-om1 copies every field"
    );
    assert_eq!(extract(None, "off"), None, "no switch, no propagation");
    assert_eq!(
        extract(Some("-om=txt"), "txt").as_deref(),
        Some(b"[ZoneTransfer]\r\nZoneId=3\r\n".as_slice()),
        "-om=txt matches the extension"
    );
    assert_eq!(
        extract(Some("-om=exe"), "exe"),
        None,
        "-om=exe must not match .txt"
    );

    let out = dir.path().join("unrar");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-om", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read(format!(
            "{}{}",
            out.join("f.txt").display(),
            ":Zone.Identifier"
        ))
        .unwrap(),
        b"[ZoneTransfer]\r\nZoneId=3\r\n"
    );
}

/// `-me<par>` (including the undocumented `-mes`) is accepted by every
/// command in both binaries, like WinRAR.
#[cfg(any(unix, windows))]
#[test]
fn cli_me_switch_is_accepted_everywhere() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"me").unwrap();
    let archive = dir.path().join("me.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-mes", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    for bin in [RAR_CLI, UNRAR_CLI] {
        let status = std::process::Command::new(bin)
            .args(["t", "-mes", "-idq"])
            .arg(&archive)
            .status()
            .unwrap();
        assert!(status.success(), "{bin} t -mes must be accepted");
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-mes", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
}

// ── -log name logs ──────────────────────────────────────────────────────────

/// `-log` writes archive and/or member names (default `rarinfo.log`), `P`
/// appends, `U` writes UTF-16LE, and it works for create, list, extract
/// and delete.
#[cfg(any(unix, windows))]
#[test]
fn cli_log_writes_archive_and_file_names() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("f2.txt"), b"two").unwrap();
    let run = |args: &[&str]| {
        let status = std::process::Command::new(RAR_CLI)
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    };
    let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();

    run(&["a", "-log=arc.txt", "-idq", "log.rar", "f1.txt", "f2.txt"]);
    assert_eq!(read("arc.txt"), "log.rar\r\n");

    run(&[
        "a",
        "-logf=files.txt",
        "-idq",
        "log2.rar",
        "f1.txt",
        "f2.txt",
    ]);
    assert_eq!(read("files.txt"), "f1.txt\r\nf2.txt\r\n");

    run(&["a", "-logAF=both.txt", "-idq", "log3.rar", "f1.txt"]);
    assert_eq!(read("both.txt"), "log3.rar\r\nf1.txt\r\n");

    run(&["a", "-log", "-idq", "log4.rar", "f1.txt"]);
    assert_eq!(read("rarinfo.log"), "log4.rar\r\n");

    // P appends instead of truncating.
    run(&["a", "-logP=arc.txt", "-idq", "log5.rar", "f2.txt"]);
    assert_eq!(read("arc.txt"), "log.rar\r\nlog5.rar\r\n");

    // U writes UTF-16LE (no BOM), like WinRAR.
    run(&["a", "-logU=utf16.txt", "-idq", "log6.rar", "f1.txt"]);
    let bytes = std::fs::read(dir.path().join("utf16.txt")).unwrap();
    let expected: Vec<u8> = "log6.rar\r\n"
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    assert_eq!(bytes, expected);

    run(&["l", "-logf=listed.txt", "-idq", "log.rar"]);
    assert_eq!(read("listed.txt"), "f1.txt\r\nf2.txt\r\n");

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    run(&[
        "x",
        "-logf=xf.txt",
        "-idq",
        "--dest",
        out.to_str().unwrap(),
        "log.rar",
    ]);
    assert_eq!(read("xf.txt"), "f1.txt\r\nf2.txt\r\n");

    run(&["d", "-logf=df.txt", "-idq", "log2.rar", "f2.txt"]);
    assert_eq!(read("df.txt"), "f2.txt\r\n");
}

/// UnRAR rejects `-log` like the official binary (exit 7), and an
/// unwritable log path fails with the create error code (9).
#[cfg(any(unix, windows))]
#[test]
fn cli_log_is_rejected_by_unrar_and_reports_unwritable_paths() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq", "log.rar", "f1.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let out = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-log=ux.txt", "-idq", "log.rar"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7), "unrar must reject -log");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("Unknown option: log=ux.txt"), "{text}");

    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-log=no-such-dir/x.txt", "-idq", "log2.rar", "f1.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(9), "unwritable log paths use exit 9");
}

// ── -mc filter policy ───────────────────────────────────────────────────────

/// `-mc-` disables the automatic delta/x86 filters, `-mcd+` forces delta
/// (matching what auto picks on ramp data), and the switch parser stays as
/// lenient as WinRAR's.
#[cfg(any(unix, windows))]
#[test]
fn cli_mc_filter_policy_controls_filters() {
    let dir = make_temp_dir();
    let mut ramp = vec![0u8; 64 * 1024];
    for (i, byte) in ramp.iter_mut().enumerate() {
        *byte = ((i / 64) % 256) as u8;
    }
    std::fs::write(dir.path().join("ramp.bin"), &ramp).unwrap();

    let create = |spec: Option<&str>, archive: &str| {
        let mut command = std::process::Command::new(RAR_CLI);
        command.arg("a");
        if let Some(spec) = spec {
            command.arg(spec);
        }
        let status = command
            .args(["-m3", "-idq", archive, "ramp.bin"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{spec:?}");
        let reader = rar_rs::ArchiveReader::open(dir.path().join(archive)).unwrap();
        reader
            .entry(reader.unique_entry("ramp.bin").unwrap())
            .unwrap()
            .compressed_size()
    };

    let auto = create(None, "auto.rar");
    let disabled = create(Some("-mc-"), "disabled.rar");
    let forced = create(Some("-mcd+"), "forced.rar");
    assert!(
        disabled > auto,
        "auto delta must beat disabled filters ({auto} vs {disabled})"
    );
    assert!(
        forced <= auto,
        "forced delta must be at least as good as auto on ramp data ({auto} vs {forced})"
    );

    // lenient forms stay accepted, like WinRAR's parser
    for spec in ["-mc5", "-mcz", "-mcl-", "-mcx", "-mcd6+", "-mc6d+", "-mce+"] {
        let packed = create(Some(spec), "lenient.rar");
        assert!(packed > 0, "{spec}");
    }

    // Forced x86 must still round-trip.
    let archive = dir.path().join("x86.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-mce+", "-idq"])
        .arg(&archive)
        .arg("ramp.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(out.join("ramp.bin")).unwrap(), ramp);

    // RAR4 honors the same policy: forced delta beats disabled filters.
    let disabled4 = {
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", "-ma4", "-mc-", "-m3", "-idq", "r4off.rar", "ramp.bin"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let reader = rar_rs::ArchiveReader::open(dir.path().join("r4off.rar")).unwrap();
        reader
            .entry(reader.unique_entry("ramp.bin").unwrap())
            .unwrap()
            .compressed_size()
    };
    let forced4 = {
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", "-ma4", "-mcd+", "-m3", "-idq", "r4on.rar", "ramp.bin"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let reader = rar_rs::ArchiveReader::open(dir.path().join("r4on.rar")).unwrap();
        reader
            .entry(reader.unique_entry("ramp.bin").unwrap())
            .unwrap()
            .compressed_size()
    };
    assert!(
        forced4 < disabled4,
        "RAR4 forced delta must beat disabled filters ({disabled4} vs {forced4})"
    );

    // UnRAR accepts the switch as a no-op, like the official binary.
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-mc-", "-idq"])
        .arg(dir.path().join("auto.rar"))
        .status()
        .unwrap();
    assert!(status.success());
}

// ── command shapes: mf, lta/vta, extract -op/-kb/-or, -qo+/- ────────────────

/// `rar mf` archives the tree like `m` but leaves directories on disk and
/// removes only the files.
#[test]
fn cli_mf_moves_files_and_keeps_directories() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("sub").join("f2.txt"), b"two").unwrap();

    let archive = dir.path().join("mf.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["mf", "-idq"])
        .arg(&archive)
        .args(["sub", "f1.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(dir.path().join("sub").is_dir(), "directories stay on disk");
    assert!(!dir.path().join("sub").join("f2.txt").exists());
    assert!(!dir.path().join("f1.txt").exists());
    assert_eq!(cli_names(&archive), ["f1.txt", "sub", "sub/f2.txt"]);
}

/// `lta` / `vta` are accepted aliases of `lt` / `vt`.
#[test]
fn cli_lta_vta_aliases_accepted() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    let archive = dir.path().join("lt.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f1.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    for command in ["lt", "lta", "vt", "vta", "lb", "vb"] {
        let out = std::process::Command::new(RAR_CLI)
            .args([command])
            .arg(&archive)
            .output()
            .unwrap();
        assert!(out.status.success(), "{command}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("f1.txt"), "{command}: {text}");
    }
}

/// `rar x` honors `-op<path>` (output path), `-kb` (keep broken) and `-or`
/// (auto-rename), like UnRAR.
#[test]
fn cli_extract_op_kb_or_switches() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    let archive = dir.path().join("x.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f1.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // -op<path> overrides --dest.
    let out = dir.path().join("by-op");
    let status = std::process::Command::new(RAR_CLI)
        .arg("x")
        .arg(format!("-op{}", out.display()))
        .args(["-idq", "--dest"])
        .arg(dir.path().join("unused"))
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(out.join("f1.txt").exists());

    // -or renames an existing destination, -kb is accepted.
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-or", "-kb", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(out.join("f1(1).txt").exists(), "-or must create f1(1).txt");
}

/// `-qo+` / `-qo-` are accepted (quick-open on/off).
#[test]
fn cli_qo_plus_and_minus_accepted() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    for (switch, name) in [("-qo+", "qo.rar"), ("-qo-", "noqo.rar")] {
        let archive = dir.path().join(name);
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", switch, "-idq"])
            .arg(&archive)
            .arg("f1.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{switch}");
        assert_eq!(cli_names(&archive), ["f1.txt"]);
    }
}

// ── a -f / a -u / a -k / -z, -ol- ───────────────────────────────────────────

/// `rar a -f` freshens (existing members only) and `rar a -u` updates (adds
/// missing members too), like the `f` / `u` commands.
#[test]
fn cli_create_freshen_update_switches() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    let base = dir.path().join("base.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&base)
        .arg("f1.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(dir.path().join("f1.txt"), b"one-newer").unwrap();
    std::fs::write(dir.path().join("f2.txt"), b"two").unwrap();

    // -f: only the existing f1 is replaced, f2 stays out.
    let freshened = dir.path().join("freshen.rar");
    std::fs::copy(&base, &freshened).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-f", "-idq"])
        .arg(&freshened)
        .args(["f1.txt", "f2.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&freshened), ["f1.txt"]);

    // -u: f2 is added and f1 updated.
    let updated = dir.path().join("update.rar");
    std::fs::copy(&base, &updated).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-u", "-idq"])
        .arg(&updated)
        .args(["f1.txt", "f2.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&updated), ["f1.txt", "f2.txt"]);
}

/// `rar a -k` locks the new archive (delete then fails with exit 4) and
/// `-z<file>` attaches an archive comment.
#[test]
fn cli_create_lock_and_comment_switches() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("comment.txt"), b"archive comment").unwrap();
    let archive = dir.path().join("locked.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-k", "-zcomment.txt", "-idq"])
        .arg(&archive)
        .arg("f1.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let out = std::process::Command::new(RAR_CLI)
        .args(["cw", "locked.rar"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"archive comment");

    let status = std::process::Command::new(RAR_CLI)
        .args(["d", "-idq", "locked.rar", "f1.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(4), "locked archives use exit 4");
}

/// `-ol-` skips symbolic links when archiving and when extracting.
#[cfg(unix)]
#[test]
fn cli_skip_links_switch() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("target.txt"), b"target").unwrap();
    std::os::unix::fs::symlink("target.txt", dir.path().join("lnk.txt")).unwrap();

    let with_links = dir.path().join("links.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ol", "-idq"])
        .arg(&with_links)
        .args(["target.txt", "lnk.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&with_links), ["lnk.txt", "target.txt"]);

    let without = dir.path().join("nolinks.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ol-", "-idq"])
        .arg(&without)
        .args(["target.txt", "lnk.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&without), ["target.txt"]);

    // Extracting the -ol archive with -ol- skips the link member.
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-ol-", "-idq", "--dest"])
        .arg(&out)
        .arg(&with_links)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        !out.join("lnk.txt").exists(),
        "-ol- must skip link extraction"
    );
    assert!(out.join("target.txt").exists());
}

/// Windows/interactive no-op switches are accepted by every command of
/// both binaries, like WinRAR's parser (`-vd` is the documented exception:
/// it is rejected because it would erase removable media).
#[test]
fn cli_noop_switches_accepted_everywhere() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    let archive = dir.path().join("noop.rar");
    let switches = [
        "-ac", "-ai", "-ao", "-e+h", "-e-h", "-dh", "-oc", "-oni", "-ri1:1", "-vp", "-ioff",
        "-isnd", "-ieml", "-mlp", "-ams", "-amr", "-scuc",
    ];

    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .args(switches)
        .arg(&archive)
        .arg("f1.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "create with the no-op switches");

    for switch in switches {
        for bin in [RAR_CLI, UNRAR_CLI] {
            let status = std::process::Command::new(bin)
                .args(["t", "-idq", switch])
                .arg(&archive)
                .status()
                .unwrap();
            assert!(status.success(), "{bin} t {switch}");
        }
    }
}
