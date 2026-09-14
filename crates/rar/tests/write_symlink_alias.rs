//! Regression test: an alias of a sibling directory (junction on Windows,
//! symlink on Unix) must not swallow the sibling's subtree.
//!
//! `add_directory_inner` kept one whole-tree set of canonical directory
//! identities, and `canonicalize` resolves a junction/symlink to its target.
//! Adding `root` with `root/link -> root/real` therefore recorded
//! `root/link`'s canonical identity (`root/real`) first and skipped the real
//! `root/real` directory afterwards, dropping its whole subtree from the
//! archive. Only the identities of the current ancestor chain are tracked
//! now, so the cycle edge is still cut but a non-cyclic sibling alias is
//! walked like the acyclic tree it is.

use std::path::Path;

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

#[cfg(windows)]
#[test]
fn junction_to_a_sibling_directory_keeps_both_subtrees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::fs::write(root.join("real").join("inside.txt"), b"payload").unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(root.join("link"))
        .arg(root.join("real"))
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");

    assert_both_subtrees_archived(&root);
}

#[cfg(unix)]
#[test]
fn symlink_to_a_sibling_directory_keeps_both_subtrees() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::fs::write(root.join("real").join("inside.txt"), b"payload").unwrap();
    symlink(root.join("real"), root.join("link")).unwrap();

    assert_both_subtrees_archived(&root);
}

/// `root/link` and `root/real` alias the same directory; recursing through
/// the link must archive its subtree under the link's name without dropping
/// the real directory's own subtree (the link sorts first, so the old global
/// visited set lost `real`).
#[cfg(any(unix, windows))]
fn assert_both_subtrees_archived(root: &Path) {
    let out = root.parent().unwrap().join("out.rar");
    let mut writer = ArchiveWriter::create_with(&out, WriterOptions::default()).unwrap();
    writer.add_path(root, stored()).unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open(&out).unwrap();
    let mut names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    names.sort();
    for expected in ["root/link/inside.txt", "root/real/inside.txt"] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected} in {names:?}"
        );
        let id = reader.unique_entry(expected).unwrap();
        assert_eq!(reader.read_entry(id).unwrap(), b"payload", "{expected}");
    }
}
