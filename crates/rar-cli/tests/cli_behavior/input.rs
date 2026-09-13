//! WinRAR input-syntax parity: `@listfiles`, implicit `*.*`, member
//! filters on list/test commands, automatic `.rar`, positional extraction
//! destinations and the no-op empty `d`.

use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir};
use std::process::Command;

fn run(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    Command::new(RAR_CLI)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn run_unrar(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    Command::new(UNRAR_CLI)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

/// `@listfile` supplies file/member names for creation and extraction;
/// `//` comments and blank lines are ignored and `-@` disables list
/// processing.
#[test]
fn cli_listfiles_feed_create_and_extract() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"aa").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"bb").unwrap();
    std::fs::write(
        dir.path().join("names.lst"),
        b"a.txt // comment\r\n\r\n// whole line\r\nb.txt\r\n",
    )
    .unwrap();

    let out = run(&["a", "-idq", "arc.rar", "@names.lst"], dir.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listing = run(&["lb", "arc.rar"], dir.path());
    let names = String::from_utf8_lossy(&listing.stdout);
    assert!(
        names.contains("a.txt") && names.contains("b.txt"),
        "{names}"
    );
    assert!(!names.contains("names.lst"), "{names}");

    // Member list for extraction (both binaries), with a destination.
    std::fs::write(dir.path().join("only.lst"), b"a.txt\n").unwrap();
    let out = run(
        &["x", "-idq", "arc.rar", "@only.lst", "--dest", "out"],
        dir.path(),
    );
    assert!(out.status.success());
    assert!(dir.path().join("out").join("a.txt").exists());
    assert!(!dir.path().join("out").join("b.txt").exists());

    let out = run_unrar(
        &["x", "-idq", "arc.rar", "@only.lst", "--dest", "out2"],
        dir.path(),
    );
    assert!(out.status.success());

    // `-@` disables list processing: `@names.lst` becomes a literal path.
    let out = run(&["a", "-idq", "-@", "nolist.rar", "@names.lst"], dir.path());
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("@names.lst"), "{text}");
}

/// `rar a archive` with no files archives everything (`*.*` implied).
#[test]
fn cli_create_without_files_adds_everything() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
    let out = run(&["a", "-idq", "all.rar"], dir.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listing = run(&["lb", "all.rar"], dir.path());
    let names = String::from_utf8_lossy(&listing.stdout);
    assert!(
        names.contains("a.txt") && names.contains("b.txt"),
        "{names}"
    );
}

/// `t`/`l`/`v`/`lt`/`unrar t` accept member filters and fail cleanly when
/// nothing matches.
#[test]
fn cli_list_and_test_member_filters() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
    assert!(
        run(&["a", "-idq", "f.rar", "a.txt", "b.txt"], dir.path())
            .status
            .success()
    );

    let bare = run(&["lb", "f.rar", "a.txt"], dir.path());
    assert_eq!(String::from_utf8_lossy(&bare.stdout).trim(), "a.txt");

    let list = run(&["l", "f.rar", "b.txt"], dir.path());
    let text = String::from_utf8_lossy(&list.stdout);
    assert!(text.contains("b.txt") && !text.contains("a.txt"), "{text}");

    let tech = run(&["lt", "f.rar", "a.txt"], dir.path());
    let text = String::from_utf8_lossy(&tech.stdout);
    assert!(text.contains("a.txt") && !text.contains("b.txt"), "{text}");

    // `-idq` suppresses the listing tables, like WinRAR.
    for command in ["l", "v", "lt", "lb"] {
        let quiet = run(&[command, "-idq", "f.rar"], dir.path());
        assert!(
            quiet.stdout.is_empty(),
            "{command} -idq: {}",
            String::from_utf8_lossy(&quiet.stdout)
        );
    }

    assert!(
        run(&["t", "-idq", "f.rar", "a.txt"], dir.path())
            .status
            .success()
    );
    assert!(
        run_unrar(&["t", "-idq", "f.rar", "a.txt"], dir.path())
            .status
            .success()
    );
    assert!(
        !run(&["t", "-idq", "f.rar", "missing.txt"], dir.path())
            .status
            .success()
    );
    assert!(
        !run_unrar(&["t", "-idq", "f.rar", "missing.txt"], dir.path())
            .status
            .success()
    );
}

/// `rar a foo` creates `foo.rar` when the name has no extension.
#[test]
fn cli_create_appends_the_rar_extension() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    assert!(
        run(&["a", "-idq", "foo", "f.txt"], dir.path())
            .status
            .success()
    );
    assert!(dir.path().join("foo.rar").exists());
    assert!(!dir.path().join("foo").exists());
}

/// A trailing argument ending with a path separator is the extraction
/// destination (WinRAR syntax), for `rar` and `unrar`.
#[test]
fn cli_positional_extraction_destination() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    assert!(
        run(&["a", "-idq", "p.rar", "a.txt"], dir.path())
            .status
            .success()
    );

    assert!(
        run(&["x", "-idq", "p.rar", "dest\\"], dir.path())
            .status
            .success()
    );
    assert!(dir.path().join("dest").join("a.txt").exists());

    assert!(
        run_unrar(&["x", "-idq", "p.rar", "dest2/"], dir.path())
            .status
            .success()
    );
    assert!(dir.path().join("dest2").join("a.txt").exists());

    // `--dest` still wins over a trailing name.
    assert!(
        run(&["x", "-idq", "p.rar", "--dest", "explicit"], dir.path())
            .status
            .success()
    );
    assert!(dir.path().join("explicit").join("a.txt").exists());
}

/// `rar d` without members is a successful no-op (WinRAR exit 0).
#[test]
fn cli_delete_without_members_is_a_noop() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    assert!(
        run(&["a", "-idq", "d.rar", "a.txt"], dir.path())
            .status
            .success()
    );
    let before = std::fs::read(dir.path().join("d.rar")).unwrap();
    let out = run(&["d", "-idq", "d.rar"], dir.path());
    assert!(out.status.success());
    assert_eq!(std::fs::read(dir.path().join("d.rar")).unwrap(), before);
}
