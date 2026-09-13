//! Targeted correctness fixes: `t` size caps, extraction skip reporting,
//! archive-name inference and UTF-8 BOM list files.
//!
//! The `t` fix itself is asserted by `ops.rs`'s `verify_options` unit test
//! (the CLI has no switch to lower the caps); these tests drive the built
//! binaries end-to-end.

use crate::support::{RAR_CLI, UNRAR_CLI, cli_names, make_temp_dir};
use std::process::Command;

fn run(binary: &str, args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

/// `t` streams every member to a sink; the plain and filtered paths both
/// succeed on a healthy archive.
#[test]
fn cli_test_runs_with_sink_options() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.bin"), b"payload").unwrap();
    let create = run(RAR_CLI, &["a", "-idq", "t.rar", "a.bin"], dir.path());
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    for args in [
        ["t", "t.rar"].as_slice(),
        ["t", "t.rar", "a.bin"].as_slice(),
    ] {
        let out = run(RAR_CLI, args, dir.path());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("OK"),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// Extraction reports the number of files actually written (directories do
/// not inflate it) and names the files the skip-existing policy leaves
/// untouched; an all-skipped run says "No files to extract" like WinRAR.
#[test]
fn cli_extraction_reports_skipped_files() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("one.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("sub/two.txt"), b"two").unwrap();
    let create = run(
        RAR_CLI,
        &["a", "-idq", "e.rar", "one.txt", "sub"],
        dir.path(),
    );
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    let first = run(RAR_CLI, &["x", "e.rar", "--dest", "out"], dir.path());
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let text = String::from_utf8_lossy(&first.stdout);
    assert!(
        text.contains("Extracted 2 file(s)"),
        "directories must not inflate the count: {text}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("out/one.txt")).unwrap(),
        b"one"
    );
    assert_eq!(
        std::fs::read(dir.path().join("out/sub/two.txt")).unwrap(),
        b"two"
    );

    let second = run(RAR_CLI, &["x", "e.rar", "--dest", "out"], dir.path());
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let text = String::from_utf8_lossy(&second.stdout);
    assert!(text.contains("Skipping"), "no skip report: {text}");
    assert!(
        !text.contains("Extracted 2 file(s)") && !text.contains("Extracted 1 file(s)"),
        "false extracted count: {text}"
    );
    assert!(text.contains("No files to extract"), "{text}");
    assert_eq!(
        std::fs::read(dir.path().join("out/one.txt")).unwrap(),
        b"one"
    );
}

/// A read request without an extension falls back to `<name>.rar` and the
/// `<name>.part1.rar` first volume; a genuinely missing archive keeps the
/// old I/O error.
#[test]
fn cli_infers_the_archive_extension_on_read() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"data").unwrap();
    let create = run(RAR_CLI, &["a", "-idq", "exa.rar", "f.txt"], dir.path());
    assert!(create.status.success());

    let list = run(RAR_CLI, &["l", "exa"], dir.path());
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(String::from_utf8_lossy(&list.stdout).contains("f.txt"));

    let extract = run(RAR_CLI, &["x", "exa", "--dest", "out"], dir.path());
    assert!(
        extract.status.success(),
        "{}",
        String::from_utf8_lossy(&extract.stderr)
    );
    assert_eq!(
        std::fs::read(dir.path().join("out/f.txt")).unwrap(),
        b"data"
    );

    let unrar = run(UNRAR_CLI, &["l", "exa"], dir.path());
    assert!(
        unrar.status.success(),
        "{}",
        String::from_utf8_lossy(&unrar.stderr)
    );

    // The `.part1.rar` first volume is inferred too.
    std::fs::rename(dir.path().join("exa.rar"), dir.path().join("exa.part1.rar")).unwrap();
    let part = run(RAR_CLI, &["l", "exa"], dir.path());
    assert!(
        part.status.success(),
        "{}",
        String::from_utf8_lossy(&part.stderr)
    );

    // Genuinely missing: unchanged I/O error (exit 2, no `.rar` retry).
    let missing = run(RAR_CLI, &["l", "nope"], dir.path());
    assert_eq!(
        missing.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

/// The reproduced > 4 GiB case end-to-end: an all-zero sparse source
/// compresses to a few MiB, but the member is above the default 4 GiB read
/// cap, which `t` must not apply. Ignored (needs > 4 GiB of temp space and
/// minutes of streaming); run with
/// `cargo test -p rar-cli --test cli_behavior -- --ignored cli_test_above_the_read_cap`.
#[test]
#[ignore = "slow: needs >4 GiB of temp space; verifies `t` above the 4 GiB read cap"]
fn cli_test_above_the_read_cap() {
    let dir = make_temp_dir();
    let src = dir.path().join("huge.bin");
    let file = std::fs::File::create(&src).unwrap();
    file.set_len(4 * 1024 * 1024 * 1024 + 4096).unwrap();
    drop(file);

    let create = run(
        RAR_CLI,
        &["a", "-m3", "-idq", "huge.rar", "huge.bin"],
        dir.path(),
    );
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let test = run(RAR_CLI, &["t", "huge.rar"], dir.path());
    assert!(
        test.status.success(),
        "{}",
        String::from_utf8_lossy(&test.stderr)
    );
}

/// A Notepad-style UTF-8 BOM list file must not corrupt the first entry;
/// `-x@` mask files use the same decoder.
#[test]
fn cli_utf8_bom_listfiles_decode() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"aa").unwrap();

    let mut names = vec![0xEF, 0xBB, 0xBF];
    names.extend_from_slice(b"a.txt\r\n");
    std::fs::write(dir.path().join("names.lst"), &names).unwrap();
    let out = run(RAR_CLI, &["a", "-idq", "bom.rar", "@names.lst"], dir.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(cli_names(&dir.path().join("bom.rar")), ["a.txt"]);

    std::fs::write(dir.path().join("keep.bin"), b"bb").unwrap();
    let mut masks = vec![0xEF, 0xBB, 0xBF];
    masks.extend_from_slice(b"*.txt\r\n");
    let mask_path = dir.path().join("masks.lst");
    std::fs::write(&mask_path, &masks).unwrap();
    let status = Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(format!("-x@{}", mask_path.display()))
        .arg("xat.rar")
        .args(["a.txt", "keep.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&dir.path().join("xat.rar")), ["keep.bin"]);
}
