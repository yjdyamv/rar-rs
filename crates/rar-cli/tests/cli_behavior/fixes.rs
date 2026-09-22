//! Targeted correctness fixes: `t` size caps, extraction skip reporting,
//! archive-name inference, UTF-8 BOM list files, selector path semantics,
//! extraction-count destination resolution and add-time name dedupe.
//!
//! The `t` fix itself is asserted by `ops.rs`'s `verify_options` unit test
//! (the CLI has no switch to lower the caps); these tests drive the built
//! binaries end-to-end.

use crate::support::{RAR_CLI, UNRAR_CLI, cli_names, make_temp_dir};
use rar_rs::{CompressionLevel, EntryWriteOptions};
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
    assert_eq!(
        second.status.code(),
        Some(10),
        "an all-skipped run exits 10 like WinRAR: {}",
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

    // Genuinely missing: "no files found" (exit 10, no `.rar` retry), like
    // WinRAR.
    let missing = run(RAR_CLI, &["l", "nope"], dir.path());
    assert_eq!(
        missing.status.code(),
        Some(10),
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

/// A non-mask selector matches the stored path (or a directory prefix), not
/// a nested basename: both official tools report "No files to extract" (exit
/// 10) for `x arc a.txt` when the member is stored as `sel/sub/a.txt`.
/// Masks still match basenames (`*a.txt`) and the full path keeps working.
#[test]
fn cli_selector_requires_the_stored_path_or_a_mask() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sel/sub")).unwrap();
    std::fs::write(dir.path().join("sel/sub/a.txt"), b"nested").unwrap();
    std::fs::write(dir.path().join("sel/top.txt"), b"top").unwrap();
    let create = run(RAR_CLI, &["a", "-idq", "sel.rar", "sel"], dir.path());
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    for (index, binary) in [RAR_CLI, UNRAR_CLI].into_iter().enumerate() {
        let out_dir = format!("out-{index}");

        // x / e / t / p all reject the bare nested basename (exit 10).
        let out = run(
            binary,
            &["x", "-idq", "--dest", &out_dir, "sel.rar", "a.txt"],
            dir.path(),
        );
        assert_eq!(out.status.code(), Some(10), "{binary} x: {out:?}");
        assert!(
            !dir.path().join(&out_dir).exists(),
            "{binary} x must extract nothing for a nested basename"
        );
        let out = run(
            binary,
            &["e", "-idq", "--dest", &out_dir, "sel.rar", "a.txt"],
            dir.path(),
        );
        assert_eq!(out.status.code(), Some(10), "{binary} e: {out:?}");
        let out = run(binary, &["t", "-idq", "sel.rar", "a.txt"], dir.path());
        assert_eq!(out.status.code(), Some(10), "{binary} t: {out:?}");
        let out = run(binary, &["p", "-idq", "sel.rar", "a.txt"], dir.path());
        assert_eq!(out.status.code(), Some(10), "{binary} p: {out:?}");
        assert!(out.stdout.is_empty(), "{binary} p printed data");

        // A mask matches the basename anywhere in the tree.
        let mask_dir = format!("mask-{index}");
        let out = run(
            binary,
            &["x", "-idq", "--dest", &mask_dir, "sel.rar", "*a.txt"],
            dir.path(),
        );
        assert!(
            out.status.success(),
            "{binary} mask selection: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            dir.path().join(&mask_dir).join("sel/sub/a.txt").exists(),
            "{binary} mask selection missed the nested member"
        );

        // The full stored path selects the nested member.
        let full_dir = format!("full-{index}");
        let out = run(
            binary,
            &["x", "-idq", "--dest", &full_dir, "sel.rar", "sel/sub/a.txt"],
            dir.path(),
        );
        assert!(
            out.status.success(),
            "{binary} full-path selection: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dir.path().join(&full_dir).join("sel/sub/a.txt").exists());
    }

    // A directory prefix selects the whole subtree.
    let out = run(
        RAR_CLI,
        &["x", "-idq", "--dest", "subtree", "sel.rar", "sel"],
        dir.path(),
    );
    assert!(out.status.success());
    assert!(dir.path().join("subtree/sel/sub/a.txt").exists());
    assert!(dir.path().join("subtree/sel/top.txt").exists());
}

/// The written-file count comes from the library's own destination
/// resolution: two members whose names collide under Windows case folding
/// (`a.txt` / `A.txt`) are reported as one written file under the
/// non-interactive skip-existing default, with the second reported as
/// `Skipping` (the old `dest.join(name)` prediction said two).
#[test]
fn cli_extraction_count_uses_library_destination_resolution() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("one")).unwrap();
    std::fs::create_dir_all(dir.path().join("two")).unwrap();
    std::fs::write(dir.path().join("one/a.txt"), b"lower").unwrap();
    std::fs::write(dir.path().join("two/A.txt"), b"upper").unwrap();
    let create = run(
        RAR_CLI,
        &["a", "-ep", "-idq", "case.rar", "one/a.txt", "two/A.txt"],
        dir.path(),
    );
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    assert_eq!(cli_names(&dir.path().join("case.rar")), ["A.txt", "a.txt"]);

    let out = run(RAR_CLI, &["x", "--dest", "out", "case.rar"], dir.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let files = std::fs::read_dir(dir.path().join("out")).unwrap().count();
    if cfg!(windows) {
        assert_eq!(
            files, 1,
            "case-folded collision must write one file: {text}"
        );
        assert!(text.contains("Extracted 1 file(s)"), "{text}");
        assert!(text.contains("Skipping"), "{text}");
    } else {
        assert_eq!(files, 2, "{text}");
        assert!(text.contains("Extracted 2 file(s)"), "{text}");
    }
}

/// A hostile stored name fails extraction with the library's security error
/// and never escapes the destination; the count resolves through the same
/// policy, so the member cannot be reported as written.
#[test]
fn cli_extraction_rejects_unsafe_member_names() {
    let dir = make_temp_dir();
    let archive = dir.path().join("evil.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create(&archive).unwrap();
        let stored =
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap());
        rar.add_bytes("good.txt", b"ok", stored).unwrap();
        let escaping =
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap());
        rar.add_bytes("../escape.txt", b"nope", escaping).unwrap();
        rar.finish().unwrap();
    }

    let out = run(RAR_CLI, &["x", "--dest", "out", "evil.rar"], dir.path());
    assert!(!out.status.success(), "unsafe member must fail extraction");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("security"), "{stderr}");
    assert!(
        !dir.path().join("escape.txt").exists(),
        "traversal member escaped the destination"
    );
    assert!(!dir.path().join("out").join("escape.txt").exists());
}

/// Repeated directory arguments store every directory and file once, like
/// the official `rar a dup.rar sel sel`.
#[test]
fn cli_repeated_directory_args_store_each_entry_once() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sel/deep")).unwrap();
    std::fs::write(dir.path().join("sel/deep/x.bin"), b"x").unwrap();
    std::fs::write(dir.path().join("sel/root.txt"), b"root").unwrap();
    let expected = ["sel", "sel/deep", "sel/deep/x.bin", "sel/root.txt"];

    let out = run(RAR_CLI, &["a", "-idq", "dup.rar", "sel", "sel"], dir.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(cli_names(&dir.path().join("dup.rar")), expected);
}

/// A directory argument overlapping a wildcard argument shares the
/// source-path dedupe, in either order, and the wildcard's own directory
/// match is deduped too.
#[test]
fn cli_directory_and_wildcard_args_share_the_source_dedupe() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sel/deep")).unwrap();
    std::fs::write(dir.path().join("sel/deep/x.bin"), b"x").unwrap();
    std::fs::write(dir.path().join("sel/root.txt"), b"root").unwrap();
    let expected = ["sel", "sel/deep", "sel/deep/x.bin", "sel/root.txt"];

    for (name, args) in [
        ("overlap.rar", ["sel", "sel/*"]),
        ("overlap2.rar", ["sel/*", "sel"]),
    ] {
        let mut argv: Vec<&str> = vec!["a", "-idq", name];
        argv.extend_from_slice(&args);
        let out = run(RAR_CLI, &argv, dir.path());
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(cli_names(&dir.path().join(name)), expected, "{name}");
    }
}

/// A leading `.`/`./` argument component is not part of the stored name:
/// official `rar a out.rar .\sel\root.txt` stores `sel/root.txt`, and a bare
/// wildcard stores its matches without a `./` prefix.
#[test]
fn cli_leading_dot_arguments_are_normalized() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sel")).unwrap();
    std::fs::write(dir.path().join("sel/root.txt"), b"root").unwrap();
    std::fs::write(dir.path().join("f.txt"), b"f").unwrap();
    std::fs::write(dir.path().join("g.bin"), b"g").unwrap();
    let sep = std::path::MAIN_SEPARATOR;

    // A bare `.` argument names the current directory: its children are
    // stored with plain names and no empty directory entry is written.
    let out = run(RAR_CLI, &["a", "-idq", "cwd.rar", "."], dir.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        cli_names(&dir.path().join("cwd.rar")),
        ["f.txt", "g.bin", "sel", "sel/root.txt"]
    );

    let dotted = format!(".{sep}sel{sep}root.txt");
    let out = run(
        RAR_CLI,
        &["a", "-idq", "dot.rar", dotted.as_str()],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(cli_names(&dir.path().join("dot.rar")), ["sel/root.txt"]);

    let dotted_dir = format!(".{sep}sel");
    let out = run(
        RAR_CLI,
        &["a", "-idq", "dotdir.rar", dotted_dir.as_str()],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        cli_names(&dir.path().join("dotdir.rar")),
        ["sel", "sel/root.txt"]
    );

    let out = run(RAR_CLI, &["a", "-idq", "wild.rar", "*.txt"], dir.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        cli_names(&dir.path().join("wild.rar")),
        ["f.txt"],
        "a bare wildcard must not prefix names with ./"
    );
}
