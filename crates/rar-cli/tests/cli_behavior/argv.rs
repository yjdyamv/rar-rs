//! WinRAR argument-order parity: switches before the command, repeated
//! switches (last wins) and configuration-source default insertion.

use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir};

/// A sign or stray character in a date/period filter is rejected instead of
/// being silently dropped (`-ta-2020` must not mean the year 2020). Valid
/// forms still work.
#[test]
fn cli_negative_and_stray_filter_values_are_rejected() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    for sw in ["-ta-2020", "-tb-2020", "-tk-2020", "-tn-5d", "-to-5d"] {
        let out = std::process::Command::new(RAR_CLI)
            .args(["a", sw, "-idq"])
            .arg(dir.path().join("rejected.rar"))
            .arg(dir.path().join("f.txt"))
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{sw} must be rejected");
    }
    for sw in ["-ta2020-01-01", "-tn30d"] {
        let out = std::process::Command::new(RAR_CLI)
            .args(["a", sw, "-idq"])
            .arg(dir.path().join("accepted.rar"))
            .arg(dir.path().join("f.txt"))
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{sw} must be accepted: {out:?}");
    }
}

/// Hostile / malformed values for the numeric and time-filter switches must be
/// rejected cleanly: a real exit code (never a signal/abort) and no panic, and
/// the negative forms specifically must not be read as valid input.
#[test]
fn cli_hostile_switch_values_exit_cleanly() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let cases = [
        "-mt-1",
        "-mt0",
        "-mt",
        "-mt1.5",
        "-mt99999999999999999999",
        "-m-1",
        "-m6",
        "-m9",
        "-v-1",
        "-v0",
        "-v1x",
        "-v99999999999999999999",
        "-sl-1",
        "-sm-1",
        "-rr-1",
        "-rv-1",
        "-ta-2020",
        "-tb-2020",
        "-tk-2020",
        "-tn-1d",
        "-to-1d",
        "-tnabc",
        "-ta1.5",
        "-md-1",
        "-mdabc",
        "-ma-1",
    ];
    for sw in cases {
        let out = std::process::Command::new(RAR_CLI)
            .args(["a", sw, "-idq"])
            .arg(dir.path().join("h.rar"))
            .arg(dir.path().join("f.txt"))
            .current_dir(dir.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.code().is_some(), "{sw} crashed: {out:?}");
        assert!(
            !stderr.contains("panicked") && !stderr.contains("RUST_BACKTRACE"),
            "{sw} panicked: {stderr}"
        );
        // `-mdx` is an extraction-only dictionary cap; WinRAR ignores it on a
        // create, so it is the one value that may be accepted here.
        assert!(!out.status.success(), "{sw} must be rejected");
    }

    // Extraction-only switches are ignored on `a` (like WinRAR) but parsed on
    // `x`, where a hostile value must still be rejected cleanly.
    let archive = dir.path().join("base.rar");
    std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg(dir.path().join("f.txt"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    for sw in ["-mdx-1", "--max-unpacked=-1", "--max-total-unpacked=0"] {
        let out = std::process::Command::new(RAR_CLI)
            .args(["x", sw, "-idq"])
            .arg(&archive)
            .current_dir(dir.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.code().is_some(), "{sw} crashed: {out:?}");
        assert!(
            !stderr.contains("panicked") && !stderr.contains("RUST_BACKTRACE"),
            "{sw} panicked: {stderr}"
        );
        assert!(!out.status.success(), "{sw} must be rejected on x");
    }
}

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

    // Repeated single-value switches do not hard-fail; the last one wins. A
    // repeated numeric option is accepted...
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-md1m", "-md2m", "-idq"])
        .arg(dir.path().join("rep-md.rar"))
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar a -md1m -md2m must succeed");

    // ...and the -o order decides the overwrite policy (the last one wins),
    // like WinRAR: `-o+ -o-` keeps the existing file, `-o- -o+` replaces it.
    for (flags, kept_stale) in [(["-o+", "-o-"], true), (["-o-", "-o+"], false)] {
        let dest = dir
            .path()
            .join(if kept_stale { "o-never" } else { "o-always" });
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("f.txt"), b"STALE").unwrap();
        let status = std::process::Command::new(RAR_CLI)
            .args(["x"])
            .args(flags)
            .arg("-idq")
            .arg(&solid)
            .arg(format!("{}{}", dest.display(), std::path::MAIN_SEPARATOR))
            .status()
            .unwrap();
        let got = std::fs::read(dest.join("f.txt")).unwrap();
        if kept_stale {
            // The last -o is `-o-` (never): the existing file is kept and the
            // run reports "no files" (10), exactly like `Rar x -o-`.
            assert_eq!(got, b"STALE", "{flags:?}: the last -o (never) must win");
            assert_eq!(status.code(), Some(10), "{flags:?}: a full skip exits 10");
        } else {
            // The last -o is `-o+` (always): the existing file is replaced.
            assert_eq!(got, b"x", "{flags:?}: the last -o (always) must win");
            assert!(status.success(), "{flags:?}: an overwrite succeeds");
        }
    }
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

    // A genuinely new member: a bare re-run would be a no-op, which now exits
    // 10 like `Rar.exe`.
    std::fs::write(dir.path().join("f2b.txt"), b"two-b").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-s", "-idq"])
        .arg(&archive)
        .arg("f2b.txt")
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

/// A switch value is whatever the user types after the switch letter: it can
/// overflow, be non-numeric, have the wrong shape or be empty. Every such form
/// must come back as a clean rejection -- a non-zero exit and a normal
/// message -- never a panic, an index-out-of-bounds, an arithmetic overflow or
/// a hang. The broad sweep only asks "did it terminate cleanly"; the curated
/// list pins that a value reaching a strict parser really is rejected.
#[test]
fn cli_malformed_switch_values_are_rejected_without_panicking() {
    use std::process::Stdio;

    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"probe").unwrap();
    let archive = dir.path().join("t.rar");
    let created = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(created.success(), "fixture archive must build");

    // A child that reads stdin (a bare `-p`/`-z`/`@`) must see EOF, not the
    // test runner's terminal, so a prompt cannot hang the suite.
    let run = |token: &str| -> std::process::Output {
        std::process::Command::new(RAR_CLI)
            .arg("a")
            .arg(token)
            .arg(&archive)
            .arg("f.txt")
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let assert_clean = |token: &str, output: &std::process::Output| {
        assert!(
            output.status.code().is_some(),
            "{token}: child was killed by a signal"
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for marker in [
            "panicked",
            "RUST_BACKTRACE",
            "index out of bounds",
            "attempt to",
            "slice index",
        ] {
            assert!(
                !combined.contains(marker),
                "{token}: `{marker}` leaked: {combined}"
            );
        }
    };

    let long = "9".repeat(200);
    let junk = [
        "",
        "-",
        "%",
        "1.5",
        "abc",
        "0x10",
        "18446744073709551616",
        long.as_str(),
    ];
    let prefixes = [
        "-m", "-v", "-ta", "-tb", "-tk", "-tn", "-to", "-ts", "-sl", "-sm", "-mc", "-ma", "-md",
        "-mdx", "-mt", "-ms", "-ap", "-ag", "-hp", "-rv", "-sc", "-e", "-x", "-n", "-z", "-p",
    ];
    let mut swept = 0usize;
    for prefix in prefixes {
        for value in junk {
            let token = format!("{prefix}{value}");
            let output = run(&token);
            assert_clean(&token, &output);
            swept += 1;
        }
    }
    assert!(swept >= 200, "battery shrank unexpectedly: {swept}");

    // Values no create-scoped switch of this shape can accept must be refused,
    // proving the value reaches the parser instead of being silently dropped
    // (a switch an unrelated command does not declare is dropped by design).
    for token in [
        "-m9",
        "-mabc",
        "-m-1",
        "-m99999999999999999999",
        "-vabc",
        "-v1x2",
        "-v99999999999999999999999",
        "-taabc",
        "-ta99999999999999999999",
        "-tbabc",
        "-mt0",
        "-mtabc",
        "-mt999999999999999999999",
        "-mdabc",
        "-md1x",
        "-sl1x",
        "-sm1x",
        "-rv1x",
        "-tk1x",
    ] {
        let output = run(token);
        assert_clean(token, &output);
        assert!(
            !output.status.success(),
            "{token}: a malformed value must be rejected, not accepted: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
