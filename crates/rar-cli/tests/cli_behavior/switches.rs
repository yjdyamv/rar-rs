use std::io::Write;

use rar_rs::{CompressionLevel, EntryWriteOptions};

use crate::support::{RAR_CLI, UNRAR_CLI, cli_names, make_temp_dir, set_mtime_ago};
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
