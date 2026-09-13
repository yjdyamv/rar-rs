//! WinRAR argument-order parity: switches before the command, repeated
//! switches (last wins) and configuration-source default insertion.

use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir};

/// `-p<pwd>` / `-m<N>` given before the command work like WinRAR, and the
/// `unrar` binary keeps its own pre-command switch handling.
#[test]
fn cli_switches_before_command_apply() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    // -m0 before the command applies the store level ("level 0" message).
    let archive = dir.path().join("before-m.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["-m0", "a"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "rar -m0 a must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("level 0"),
        "-m0 before the command must apply, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // -p<password> before the command encrypts the archive.
    let encrypted = dir.path().join("before-p.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["-psecret", "a", "-idq"])
        .arg(&encrypted)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar -psecret a must succeed");
    let mut rar =
        rar_rs::ArchiveReader::open_with(&encrypted, rar_rs::OpenOptions::new().password("secret"))
            .unwrap();
    let id = rar.unique_entry("f.txt").unwrap();
    assert_eq!(rar.read_entry(id).unwrap(), b"x");

    // unrar accepts the same pre-command password form.
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["-psecret", "l", "-idq"])
        .arg(&encrypted)
        .status()
        .unwrap();
    assert!(status.success(), "unrar -psecret l must succeed");
}

/// Repeated switches are tolerated with last-wins semantics, like WinRAR.
#[test]
fn cli_repeated_switches_last_wins() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    // The last -m wins in both directions.
    let level = dir.path().join("rep-level.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-m5", "-m0"])
        .arg(&level)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("level 0"),
        "the last -m must win, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "-m5"])
        .arg(dir.path().join("rep-level2.rar"))
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("level 5"));

    // -s -s stays solid, -y -y extracts.
    let solid = dir.path().join("rep-solid.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-s", "-s", "-idq"])
        .arg(&solid)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar a -s -s must succeed");
    assert!(
        rar_rs::ArchiveReader::open(&solid).unwrap().is_solid(),
        "-s -s must still create a solid archive"
    );

    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-y", "-y", "-idq", "--dest"])
        .arg(&out_dir)
        .arg(&solid)
        .status()
        .unwrap();
    assert!(status.success(), "rar x -y -y must succeed");
    assert_eq!(std::fs::read(out_dir.join("f.txt")).unwrap(), b"x");
}

/// `--no-config` disables the configuration sources exactly like `-cfg-`,
/// and duplicated defaults collapse instead of failing every command.
#[test]
fn cli_no_config_and_duplicate_defaults() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    // A duplicated default switch is deduplicated (last wins).
    let duplicated = dir.path().join("dup-env.rar");
    let out = std::process::Command::new(RAR_CLI)
        .env("RARINISWITCHES", "-s -s")
        .args(["a", "-idq"])
        .arg(&duplicated)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "a duplicated RARINISWITCHES switch must not fail: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(rar_rs::ArchiveReader::open(&duplicated).unwrap().is_solid());

    // --no-config disables RARINISWITCHES (default level 3, not 0).
    for flag in ["--no-config", "-cfg-"] {
        let archive = dir.path().join(format!("noconf{}.rar", flag.len()));
        let out = std::process::Command::new(RAR_CLI)
            .env("RARINISWITCHES", "-m0")
            .args([flag, "a"])
            .arg(&archive)
            .arg("f.txt")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("level 3"),
            "{flag} must ignore RARINISWITCHES, got: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// Defaults from configuration sources land after the real command even
/// when an option value precedes it (`--work-dir . a`).
#[test]
fn cli_default_switch_insertion_skips_option_values() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    let archive = dir.path().join("merge.rar");
    let out = std::process::Command::new(RAR_CLI)
        .env("RARINISWITCHES", "-m0")
        .args(["--work-dir", ".", "a"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "defaults must be inserted after the command: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("level 0"),
        "the default -m0 must apply, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(archive.exists());
}

/// `u` / `m` accept the compression switches their create sibling takes:
/// `-m<N>` reaches the appended members and `-s` is accepted.
#[test]
fn cli_update_and_move_accept_compression_switches() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    let archive = dir.path().join("up.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f1.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    std::fs::write(dir.path().join("f2.txt"), b"two").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-m0", "-idq"])
        .arg(&archive)
        .arg("f2.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar u -m0 must succeed");
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let f2 = rar.entry(rar.unique_entry("f2.txt").unwrap()).unwrap();
    assert_eq!(
        f2.compressed_size(),
        f2.size(),
        "-m0 must store the appended member"
    );

    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-s", "-idq"])
        .arg(&archive)
        .arg("f2.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar u -s must be accepted");

    // `a -u` delegates through the same args, so -m reaches it too.
    std::fs::write(dir.path().join("f3.txt"), b"three").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-u", "-m0", "-idq"])
        .arg(&archive)
        .arg("f3.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar a -u -m0 must succeed");
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let f3 = rar.entry(rar.unique_entry("f3.txt").unwrap()).unwrap();
    assert_eq!(f3.compressed_size(), f3.size());

    // A move that creates the archive honors -m and -s.
    std::fs::write(dir.path().join("m1.txt"), b"move").unwrap();
    let moved = dir.path().join("move.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["m", "-m0", "-s", "-idq"])
        .arg(&moved)
        .arg("m1.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar m -m0 -s must succeed");
    assert!(!dir.path().join("m1.txt").exists(), "the source is removed");
    let rar = rar_rs::ArchiveReader::open(&moved).unwrap();
    assert!(rar.is_solid(), "m -s must create a solid archive");
    let m1 = rar.entry(rar.unique_entry("m1.txt").unwrap()).unwrap();
    assert_eq!(m1.compressed_size(), m1.size());
}

/// `rar x -mdx<size>` parses and extracts (the cap only matters for
/// dictionaries above 4 GiB).
#[test]
fn cli_extract_accepts_mdx_dictionary_cap() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("mdx.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-mdx8g", "-idq", "--dest"])
        .arg(&out_dir)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success(), "rar x -mdx8g must be accepted");
    assert_eq!(std::fs::read(out_dir.join("f.txt")).unwrap(), b"x");
}

/// Non-Unicode argv must produce a clean error, not a panic (`std::env::args`
/// panics on invalid UTF-8).
#[cfg(unix)]
#[test]
fn cli_non_unicode_arguments_do_not_panic() {
    use std::os::unix::ffi::OsStringExt;

    let dir = make_temp_dir();
    let out = std::process::Command::new(RAR_CLI)
        .arg("a")
        .arg(dir.path().join("nonutf.rar"))
        .arg(std::ffi::OsString::from_vec(vec![0xff, 0xfe]))
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked"),
        "invalid UTF-8 argv must not panic: {stderr}"
    );
    assert!(
        !out.status.success(),
        "a missing source file must fail cleanly"
    );
}
