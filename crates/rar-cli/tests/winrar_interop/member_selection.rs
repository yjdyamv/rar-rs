//! Differential checks against the official console tools for the member
//! selection and name-policy fixes: stored-path selectors, case-folded
//! extraction counts, add-time directory dedupe and leading `./`
//! normalization.
//!
//! Each case runs the same fixture through WinRAR/UnRAR and through our
//! binaries, then compares the observable outcome (exit code, produced
//! files, stored member names). Tests skip when WinRAR is not installed.

use rar_rs::ArchiveReader;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::{rar_bin, temp_dir, unrar_bin};

/// The `rar`/`unrar` binaries built by this crate.
const OUR_RAR: &str = env!("CARGO_BIN_EXE_rar");
const OUR_UNRAR: &str = env!("CARGO_BIN_EXE_unrar");

/// Run a command in `cwd`, returning (exit code, stdout+stderr).
fn run_in(binary: &Path, args: &[&str], cwd: &Path) -> (Option<i32>, String) {
    let out = Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run binary");
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// A destination argument spelled the way the official tools accept: with a
/// trailing separator.
fn dest_arg(path: &Path) -> String {
    format!("{}{}", path.display(), std::path::MAIN_SEPARATOR)
}

/// Stored member names, sorted and with trailing separators trimmed (the
/// two writers spell directory entries differently).
fn archive_member_names(archive: &Path) -> Vec<String> {
    let rar = ArchiveReader::open(archive).expect("open archive");
    let mut names: Vec<String> = rar
        .entries()
        .map(|entry| entry.name().trim_end_matches(['/', '\\']).to_string())
        .collect();
    names.sort();
    names
}

/// File names in a directory, sorted.
fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dest")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn official_tools() -> Option<(PathBuf, PathBuf)> {
    match (rar_bin(), unrar_bin()) {
        (Some(rar), Some(unrar)) => Some((rar, unrar)),
        _ => {
            eprintln!("skipped: WinRAR not found");
            None
        }
    }
}

/// `x`/`e`/`t`/`p` with a bare nested basename: the official tools report
/// "No files to extract" (exit 10) and write nothing; masks still match the
/// basename anywhere and the full stored path keeps working. Our binaries
/// must agree with both official binaries.
#[test]
fn selector_stored_path_policy_matches_official() {
    let Some((rar, unrar)) = official_tools() else {
        return;
    };
    let dir = temp_dir();
    std::fs::create_dir_all(dir.path().join("sel/sub")).unwrap();
    std::fs::write(dir.path().join("sel/sub/a.txt"), b"nested").unwrap();
    let (code, out) = run_in(&rar, &["a", "-idq", "sel.rar", "sel"], dir.path());
    assert_eq!(code, Some(0), "official create failed: {out}");

    for (tag, binary) in [
        ("official-rar", rar.as_path()),
        ("official-unrar", unrar.as_path()),
        ("our-rar", Path::new(OUR_RAR)),
        ("our-unrar", Path::new(OUR_UNRAR)),
    ] {
        let dest_path = dir.path().join(format!("{tag}-dest"));
        std::fs::create_dir_all(&dest_path).unwrap();
        let dest = dest_arg(&dest_path);

        let (code, out) = run_in(
            binary,
            &["x", "-idq", "-o+", "sel.rar", "a.txt", &dest],
            dir.path(),
        );
        assert_eq!(code, Some(10), "{tag} x bare name must not match: {out}");
        assert!(
            dir_entries(&dest_path).is_empty(),
            "{tag} extracted a nested basename"
        );

        let (code, out) = run_in(
            binary,
            &["e", "-idq", "-o+", "sel.rar", "a.txt", &dest],
            dir.path(),
        );
        assert_eq!(code, Some(10), "{tag} e bare name must not match: {out}");

        let (code, out) = run_in(binary, &["t", "-idq", "sel.rar", "a.txt"], dir.path());
        assert_eq!(code, Some(10), "{tag} t bare name must not match: {out}");

        let (code, out) = run_in(binary, &["p", "-idq", "sel.rar", "a.txt"], dir.path());
        assert_eq!(code, Some(10), "{tag} p bare name must not match: {out}");

        // A mask matches the basename anywhere in the tree.
        let (code, out) = run_in(
            binary,
            &["x", "-idq", "-o+", "sel.rar", "*a.txt", &dest],
            dir.path(),
        );
        assert_eq!(code, Some(0), "{tag} mask selection failed: {out}");
        assert!(
            dest_path.join("sel").join("sub").join("a.txt").exists(),
            "{tag} mask selection missed the nested member"
        );
    }
}

/// Two members whose names differ only by ASCII case collide on Windows:
/// official `e -o-` writes one file and `e -or` renames the second to
/// `A(1).txt`. Our extraction and its reported count must match the official
/// outcome.
#[test]
fn case_colliding_extraction_count_matches_official() {
    let Some((rar, _unrar)) = official_tools() else {
        return;
    };
    let dir = temp_dir();
    std::fs::create_dir_all(dir.path().join("one")).unwrap();
    std::fs::create_dir_all(dir.path().join("two")).unwrap();
    std::fs::write(dir.path().join("one/a.txt"), b"lower").unwrap();
    std::fs::write(dir.path().join("two/A.txt"), b"upper").unwrap();
    let (code, out) = run_in(
        &rar,
        &["a", "-ep", "-idq", "case.rar", "one/a.txt", "two/A.txt"],
        dir.path(),
    );
    assert_eq!(code, Some(0), "official create failed: {out}");
    assert_eq!(
        archive_member_names(&dir.path().join("case.rar")),
        ["A.txt", "a.txt"]
    );

    // `-o-`: the second (case-folded) member is skipped.
    let official_dest = dir.path().join("official-dest");
    std::fs::create_dir_all(&official_dest).unwrap();
    let (code, out) = run_in(
        &rar,
        &["e", "-o-", "-idq", "case.rar", &dest_arg(&official_dest)],
        dir.path(),
    );
    assert_eq!(code, Some(0), "official e -o-: {out}");

    let our_dest = dir.path().join("our-dest");
    let (code, out) = run_in(
        Path::new(OUR_RAR),
        &["e", "-o-", "case.rar", &dest_arg(&our_dest)],
        dir.path(),
    );
    assert_eq!(code, Some(0), "our e -o-: {out}");
    assert_eq!(
        dir_entries(&our_dest),
        dir_entries(&official_dest),
        "our file set must match official"
    );
    if cfg!(windows) {
        assert_eq!(
            dir_entries(&official_dest),
            ["a.txt"],
            "official must collapse the case-folded member"
        );
        assert!(
            out.contains("Extracted 1 file(s)"),
            "count must reflect the single written file: {out}"
        );
        assert!(
            out.contains("Skipping"),
            "the skipped member must be reported: {out}"
        );
    }

    // `-or`: the colliding member is auto-renamed.
    let official_rename = dir.path().join("official-rename");
    std::fs::create_dir_all(&official_rename).unwrap();
    let (code, out) = run_in(
        &rar,
        &["e", "-or", "-idq", "case.rar", &dest_arg(&official_rename)],
        dir.path(),
    );
    assert_eq!(code, Some(0), "official e -or: {out}");

    let our_rename = dir.path().join("our-rename");
    let (code, out) = run_in(
        Path::new(OUR_RAR),
        &["e", "-or", "case.rar", &dest_arg(&our_rename)],
        dir.path(),
    );
    assert_eq!(code, Some(0), "our e -or: {out}");
    assert_eq!(
        dir_entries(&our_rename),
        dir_entries(&official_rename),
        "auto-rename must match official"
    );
    if cfg!(windows) {
        assert_eq!(dir_entries(&official_rename), ["A(1).txt", "a.txt"]);
    }
}

/// Repeated directory arguments, a directory overlapping a wildcard, and
/// leading `./` spelling must all store the same member names as the
/// official tool (`rar a dup.rar sel sel` stores every entry once;
/// `.\sel\root.txt` stores `sel/root.txt`).
#[test]
fn add_time_dedupe_and_dot_normalization_match_official() {
    let Some((rar, _unrar)) = official_tools() else {
        return;
    };
    let dir = temp_dir();
    std::fs::create_dir_all(dir.path().join("sel/deep")).unwrap();
    std::fs::write(dir.path().join("sel/deep/x.bin"), b"x").unwrap();
    std::fs::write(dir.path().join("sel/root.txt"), b"root").unwrap();
    std::fs::write(dir.path().join("f.txt"), b"f").unwrap();
    std::fs::write(dir.path().join("g.bin"), b"g").unwrap();

    let sep = std::path::MAIN_SEPARATOR;
    let dotted = format!(".{sep}sel{sep}root.txt");
    let tree = ["sel", "sel/deep", "sel/deep/x.bin", "sel/root.txt"];
    let cases: [(&str, Vec<&str>, &[&str]); 5] = [
        ("dup.rar", vec!["sel", "sel"], &tree),
        ("overlap.rar", vec!["sel", "sel/*"], &tree),
        ("overlap2.rar", vec!["sel/*", "sel"], &tree),
        ("dot.rar", vec![dotted.as_str()], &["sel/root.txt"]),
        ("wild.rar", vec!["*.txt"], &["f.txt"]),
    ];

    for (name, args, expected) in cases {
        let official_name = format!("official-{name}");
        let mut argv: Vec<&str> = vec!["a", "-idq", official_name.as_str()];
        argv.extend_from_slice(&args);
        let (code, out) = run_in(&rar, &argv, dir.path());
        assert_eq!(code, Some(0), "official {name}: {out}");
        assert_eq!(
            archive_member_names(&dir.path().join(&official_name)),
            expected,
            "official {name} membership"
        );

        let our_name = format!("our-{name}");
        let mut argv: Vec<&str> = vec!["a", "-idq", our_name.as_str()];
        argv.extend_from_slice(&args);
        let (code, out) = run_in(Path::new(OUR_RAR), &argv, dir.path());
        assert_eq!(code, Some(0), "our {name}: {out}");
        assert_eq!(
            archive_member_names(&dir.path().join(&our_name)),
            archive_member_names(&dir.path().join(&official_name)),
            "our {name} membership"
        );
    }
}
