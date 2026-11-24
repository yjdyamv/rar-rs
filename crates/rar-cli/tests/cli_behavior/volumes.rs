//! Reshaping an existing archive into a set of a requested volume size
//! (`-v` on an existing archive; WinRAR refuses this with "Cannot modify
//! volume", rar-rs rebuilds the set). Covers the set→set and single→set
//! cases for RAR5 and RAR4.

use crate::support::{RAR_CLI, pseudo_random_bytes};
use std::path::Path;

fn run(dir: &Path, args: &[&str], archive: &Path, extra: &Path) -> std::process::ExitStatus {
    std::process::Command::new(RAR_CLI)
        .args(args)
        .arg(archive)
        .arg(extra)
        .current_dir(dir)
        .status()
        .unwrap()
}

fn volume_lengths(first: &Path) -> Vec<u64> {
    rar_rs::discover_volumes(first)
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .collect()
}

fn test_ok(dir: &Path, archive: &Path) -> bool {
    std::process::Command::new(RAR_CLI)
        .args(["t", "-idq"])
        .arg(archive)
        .current_dir(dir)
        .status()
        .unwrap()
        .success()
}

/// `a -v<new> <existing set>` resplits the whole set at the new size and keeps
/// every member readable.
#[test]
fn cli_rar5_set_is_resized() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    std::fs::write(&a, pseudo_random_bytes(120_000, 1)).unwrap();
    std::fs::write(&b, pseudo_random_bytes(80_000, 2)).unwrap();

    let base = dir.path().join("set.rar");
    assert!(
        run(dir.path(), &["a", "-m0", "-v32k", "-idq"], &base, &a).success(),
        "create the set"
    );
    let first = dir.path().join("set.part1.rar");
    assert!(volume_lengths(&first).len() > 1, "created a set");

    assert!(
        run(dir.path(), &["a", "-m0", "-v64k", "-idq"], &first, &b).success(),
        "resize the set"
    );
    let lengths = volume_lengths(&first);
    assert!(lengths.len() > 1, "still a set: {lengths:?}");
    for length in &lengths[..lengths.len() - 1] {
        assert_eq!(*length, 64 * 1024, "non-final volume is the new size");
    }
    assert!(test_ok(dir.path(), &first), "the resized set verifies");
}

/// `a -v<size> <existing single file>` turns a single-volume archive into a
/// set of that size.
#[test]
fn cli_rar5_single_file_becomes_a_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    std::fs::write(&a, pseudo_random_bytes(120_000, 3)).unwrap();
    std::fs::write(&b, pseudo_random_bytes(80_000, 4)).unwrap();

    let single = dir.path().join("single.rar");
    assert!(run(dir.path(), &["a", "-m0", "-idq"], &single, &a).success());
    assert!(single.exists());

    assert!(
        run(dir.path(), &["a", "-m0", "-v40k", "-idq"], &single, &b).success(),
        "reshape the single file into a set"
    );
    let first = dir.path().join("single.part1.rar");
    let lengths = volume_lengths(&first);
    assert!(lengths.len() > 1, "became a set: {lengths:?}");
    assert!(!single.exists(), "the replaced single file is retired");
    assert!(test_ok(dir.path(), &first));
}

/// The same reshape for the RAR 1.5–4.x container, which defaults to the
/// modern `.partNN.rar` names.
#[test]
fn cli_rar4_single_file_becomes_a_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    std::fs::write(&a, pseudo_random_bytes(120_000, 5)).unwrap();
    std::fs::write(&b, pseudo_random_bytes(80_000, 6)).unwrap();

    let single = dir.path().join("one.rar");
    assert!(run(dir.path(), &["a", "-ma4", "-m0", "-idq"], &single, &a).success());

    assert!(
        run(
            dir.path(),
            &["a", "-ma4", "-m0", "-v40k", "-idq"],
            &single,
            &b
        )
        .success(),
        "reshape the single RAR4 file into a set"
    );
    let first = dir.path().join("one.part1.rar");
    assert!(volume_lengths(&first).len() > 1, "became a set");
    assert!(!single.exists(), "the replaced single file is retired");
    assert!(test_ok(dir.path(), &first));
}
