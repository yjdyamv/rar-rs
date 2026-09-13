#![cfg(windows)]
use std::process::Command;

use crate::support::{rar_bin, run, temp_dir, unrar_bin, write_pattern_file};

// ── -oh hard links (Windows only here; the Unix side shares the code) ──────

/// Hard links stored with `-oh` interoperate in both directions: the
/// second path is a "Hard link" redirect, and extraction recreates one
/// on-disk file (writing through either name is visible in the other).
#[cfg(windows)]
#[test]
fn oh_hardlinks_interop_with_winrar() {
    let dir = temp_dir();
    let h1 = dir.path().join("h1.txt");
    std::fs::write(&h1, b"hardlink interop body").unwrap();
    std::fs::hard_link(&h1, dir.path().join("h2.txt")).unwrap();

    // Ours -> WinRAR.
    let ours = dir.path().join("ours_oh.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-oh", "-idq"])
        .arg(&ours)
        .args(["h1.txt", "h2.txt"])
        .current_dir(dir.path()));
    assert!(ok, "our CLI a -oh failed:\n{out}");
    if let Some(unrar) = unrar_bin() {
        let win = dir.path().join("win_oh");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-y", "-idq"])
            .arg(&ours)
            .arg(&win));
        assert!(ok, "UnRAR x failed on our -oh archive:\n{out}");
        append_then_read(&win.join("h1.txt"), &win.join("h2.txt"));
    }

    // WinRAR -> ours.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_oh.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-oh", "-idq"])
            .arg(&theirs)
            .args(["h1.txt", "h2.txt"])
            .current_dir(dir.path()));
        assert!(ok, "WinRAR a -oh failed:\n{out}");
        let out_dir = dir.path().join("ours_oh_out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
            .args(["x", "-idq", "--dest"])
            .arg(&out_dir)
            .arg(&theirs));
        assert!(ok, "our CLI x failed on WinRAR's -oh archive:\n{out}");
        append_then_read(&out_dir.join("h1.txt"), &out_dir.join("h2.txt"));
    }
}

/// Append a byte through `first`; a real hard link makes `second` show it.
fn append_then_read(first: &std::path::Path, second: &std::path::Path) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(first)
        .unwrap();
    f.write_all(b"!").unwrap();
    assert_eq!(
        std::fs::read(second).unwrap(),
        b"hardlink interop body!",
        "{} and {} must be the same on-disk file",
        first.display(),
        second.display()
    );
}

// ── -oi identical files ─────────────────────────────────────────────────────

/// Identical files stored with `-oi` interoperate in both directions:
/// WinRAR extracts our references to the exact bytes, and our CLI extracts
/// WinRAR's references.
#[cfg(windows)]
#[test]
fn oi_identical_files_interop_with_winrar() {
    let dir = temp_dir();
    let i1 = dir.path().join("i1.bin");
    write_pattern_file(&i1, 128 * 1024, 9);
    std::fs::copy(&i1, dir.path().join("i2.bin")).unwrap();
    let data = std::fs::read(&i1).unwrap();

    let ours = dir.path().join("ours_oi.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-oi", "-idq"])
        .arg(&ours)
        .args(["i1.bin", "i2.bin"])
        .current_dir(dir.path()));
    assert!(ok, "our CLI a -oi failed:\n{out}");
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&ours));
        assert!(ok, "UnRAR t failed on our -oi archive:\n{out}");
        let win = dir.path().join("win_oi");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-y", "-idq"])
            .arg(&ours)
            .arg(&win));
        assert!(ok, "UnRAR x failed on our -oi archive:\n{out}");
        assert_eq!(
            std::fs::read(win.join("i2.bin")).unwrap(),
            data,
            "WinRAR must restore our identical-file reference"
        );
    }

    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_oi.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-oi", "-idq"])
            .arg(&theirs)
            .args(["i1.bin", "i2.bin"])
            .current_dir(dir.path()));
        assert!(ok, "WinRAR a -oi failed:\n{out}");
        let out_dir = dir.path().join("ours_oi_out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
            .args(["x", "-idq", "--dest"])
            .arg(&out_dir)
            .arg(&theirs));
        assert!(ok, "our CLI x failed on WinRAR's -oi archive:\n{out}");
        assert_eq!(
            std::fs::read(out_dir.join("i2.bin")).unwrap(),
            data,
            "we must restore WinRAR's identical-file reference"
        );
    }
}
