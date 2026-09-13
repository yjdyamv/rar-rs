//! Regression test: recursive add must not follow a symlink/junction loop
//! forever.
//!
//! `add_directory` recursed through `is_dir()`, which resolves reparse
//! points, so `add root` with `root/loop -> root` kept descending
//! `root/loop/loop/...` until an I/O error failed the add mid-way. The
//! canonical identity of every directory entered is now recorded and a
//! repeated one is skipped, so the add completes with the real tree.

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

#[cfg(unix)]
#[test]
fn add_skips_a_symlink_directory_loop() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("f1.bin"), b"payload").unwrap();
    symlink(&root, root.join("loop")).unwrap();

    add_and_assert_real_members(&root);
}

#[cfg(windows)]
#[test]
fn add_skips_a_junction_directory_loop() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("f1.bin"), b"payload").unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(root.join("loop"))
        .arg(&root)
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");

    add_and_assert_real_members(&root);
}

/// Add `root` recursively and read the archive back: the real member is
/// present and the loop edge was not walked.
#[cfg(any(unix, windows))]
fn add_and_assert_real_members(root: &std::path::Path) {
    let out = root.parent().unwrap().join("out.rar");
    let mut writer = ArchiveWriter::create_with(&out, WriterOptions::default()).unwrap();
    writer.add_path(root, stored()).unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open(&out).unwrap();
    let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    assert!(
        names.iter().any(|name| name == "root/f1.bin"),
        "real member missing: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name.contains("loop")),
        "the loop edge must not be walked: {names:?}"
    );
    let id = reader.unique_entry("root/f1.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), b"payload");
}
