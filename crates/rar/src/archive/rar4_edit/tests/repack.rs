use super::super::layout::scan_layout;

use crate::archive::RarArchive;
use crate::format::rar4::MHD_SOLID;
use crate::recovery::legacy_rr::scan_protect;

use std::fs;

fn build_solid(path: &std::path::Path, payloads: &[(&str, &[u8])]) {
    let mut a = crate::archive::RarArchive::create_with_options(
        path,
        crate::options::CreateOptions {
            compression: crate::version::ArchiveVersion::V29,
            solid: true,
            ..Default::default()
        },
    )
    .unwrap();
    for (name, data) in payloads {
        a.add_bytes(name, data, 3).unwrap();
    }
    a.close().unwrap();
}

fn make_text(n: usize) -> Vec<u8> {
    // NOTE: pure repeated lines only — sectioned content around ~460 KB
    // triggers a pre-existing solid-codec bug (see the ignored
    // regression test at the end of this module).
    let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
    let mut out = Vec::with_capacity(line.len() * n);
    for _ in 0..n {
        out.extend_from_slice(line);
    }
    out
}

#[test]
fn delete_middle_of_solid_chain_repacks_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid.rar");
    let p1 = make_text(60_000);
    let p2 = make_text(50_000);
    let p3 = make_text(40_000);
    build_solid(&path, &[("a.txt", &p1), ("b.txt", &p2), ("c.txt", &p3)]);
    // Sanity: the archive really is a solid chain.
    assert!(
        scan_layout(&fs::read(&path).unwrap(), 0, None)
            .unwrap()
            .main_flags
            & MHD_SOLID
            != 0
    );

    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let b = editor.unique_entry("b.txt").unwrap();
    let report = editor
        .apply(crate::archive::editor::EditPlan::new().delete(b))
        .unwrap();
    assert_eq!((report.deleted(), report.renamed()), (1, 0));

    drop(editor);
    let mut a = RarArchive::open(&path).unwrap();
    assert_eq!(
        a.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.txt", "c.txt"]
    );
    assert_eq!(
        a.read_with_options("a.txt", Default::default()).unwrap(),
        p1
    );
    assert_eq!(
        a.read_with_options("c.txt", Default::default()).unwrap(),
        p3
    );
}

#[test]
fn delete_first_and_last_of_solid_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid2.rar");
    let p1 = make_text(50_000);
    let p2 = make_text(45_000);
    let p3 = make_text(40_000);
    build_solid(&path, &[("a.txt", &p1), ("b.txt", &p2), ("c.txt", &p3)]);
    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let a = editor.unique_entry("a.txt").unwrap();
    let c = editor.unique_entry("c.txt").unwrap();
    assert_eq!(editor.delete_entries(&[a, c]).unwrap(), 2);
    drop(editor);
    let mut ar = RarArchive::open(&path).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["b.txt"]
    );
    assert_eq!(
        ar.read_with_options("b.txt", Default::default()).unwrap(),
        p2
    );
}

#[test]
fn member_comment_set_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cf.rar");
    let payload = b"payload for the commented member".to_vec();
    {
        let mut a = crate::archive::RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                compression: crate::version::ArchiveVersion::V29,
                solid: false,
                ..Default::default()
            },
        )
        .unwrap();
        a.add_bytes("a.txt", &payload, 3).unwrap();
        a.close().unwrap();
    }

    // Set a per-file comment (like `rar cf`).
    {
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.txt").unwrap();
        editor
            .apply(
                crate::archive::editor::EditPlan::new()
                    .set_member_comment(a, b"release notes".to_vec()),
            )
            .unwrap();
    }
    {
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(
            archive.entries[0].comment().unwrap(),
            b"release notes".as_slice()
        );
        assert_eq!(
            archive
                .read_with_options("a.txt", Default::default())
                .unwrap(),
            payload
        );
    }

    // Empty bytes clear the comment again.
    {
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.txt").unwrap();
        editor
            .apply(crate::archive::editor::EditPlan::new().set_member_comment(a, Vec::new()))
            .unwrap();
    }
    let mut archive = RarArchive::open(&path).unwrap();
    assert!(archive.entries[0].comment().is_none());
    assert_eq!(
        archive
            .read_with_options("a.txt", Default::default())
            .unwrap(),
        payload
    );
}

#[test]
fn solid_repack_preserves_member_comment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cf_solid.rar");
    let p1 = make_text(50_000);
    let p2 = make_text(40_000);
    build_solid(&path, &[("a.txt", &p1), ("b.txt", &p2)]);

    // Give a.txt a comment.
    {
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.txt").unwrap();
        editor
            .apply(
                crate::archive::editor::EditPlan::new().set_member_comment(a, b"keep me".to_vec()),
            )
            .unwrap();
    }
    // Deleting b.txt repacks the whole solid chain; a.txt keeps its comment.
    {
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let b = editor.unique_entry("b.txt").unwrap();
        editor
            .apply(crate::archive::editor::EditPlan::new().delete(b))
            .unwrap();
    }
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.txt"]
    );
    assert_eq!(archive.entries[0].comment().unwrap(), b"keep me".as_slice());
    assert_eq!(
        archive
            .read_with_options("a.txt", Default::default())
            .unwrap(),
        p1
    );
}

#[test]
fn solid_repack_keeps_directory_member() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("soliddir.rar");
    let p1 = make_text(50_000);
    let p2 = make_text(40_000);
    build_solid(&path, &[("docs/", &[]), ("a.txt", &p1), ("b.txt", &p2)]);

    // The directory member is present and recognized as a directory.
    {
        let a = crate::archive::RarArchive::open(&path).unwrap();
        let dir_entry = a
            .entries
            .iter()
            .find(|e| e.name() == "docs/")
            .expect("directory member present");
        assert!(dir_entry.is_dir());
    }

    // Delete b.txt; the solid chain (including the directory) is repacked.
    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let b = editor.unique_entry("b.txt").unwrap();
    editor
        .apply(crate::archive::editor::EditPlan::new().delete(b))
        .unwrap();
    drop(editor);

    let mut ar = crate::archive::RarArchive::open(&path).unwrap();
    let names: Vec<&str> = ar.entries.iter().map(|e| e.name()).collect();
    assert_eq!(names, ["docs/", "a.txt"]);
    let dir_entry = ar
        .entries
        .iter()
        .find(|e| e.name() == "docs/")
        .expect("directory member survives repack");
    assert!(dir_entry.is_dir());
    assert_eq!(
        ar.read_with_options("a.txt", Default::default()).unwrap(),
        p1
    );
}

#[test]
fn solid_delete_with_rename_rr_and_comment_compose() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid3.rar");
    let p1 = make_text(120_000);
    let p2 = make_text(100_000);
    let p3 = make_text(80_000);
    build_solid(&path, &[("a.txt", &p1), ("b.txt", &p2), ("c.txt", &p3)]);
    {
        let mut e = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        e.apply(crate::archive::editor::EditPlan::new().set_recovery(10))
            .unwrap();
        let mut e = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let cmt = "solid chain 注释".as_bytes();
        e.apply(crate::archive::editor::EditPlan::new().set_comment(cmt.to_vec()))
            .unwrap();
    }
    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let b = editor.unique_entry("b.txt").unwrap();
    let c = editor.unique_entry("c.txt").unwrap();
    let report = editor
        .apply(
            crate::archive::editor::EditPlan::new()
                .delete(b)
                .rename(c, "renamed.txt"),
        )
        .unwrap();
    assert_eq!((report.deleted(), report.renamed()), (1, 1));

    drop(editor);
    let mut a = RarArchive::open(&path).unwrap();
    assert_eq!(
        a.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.txt", "renamed.txt"]
    );
    assert_eq!(
        a.read_with_options("a.txt", Default::default()).unwrap(),
        p1
    );
    assert_eq!(
        a.read_with_options("renamed.txt", Default::default())
            .unwrap(),
        p3
    );
    // The comment and recovery record survived the repack.
    let mut a = RarArchive::open(&path).unwrap();
    assert_eq!(
        a.get_comment().unwrap(),
        Some("solid chain 注释".as_bytes().to_vec())
    );
    let bytes = std::fs::read(&path).unwrap();
    assert!(scan_protect(&bytes).unwrap().protect.is_some());
    // (The rebuilt record's repair capability is exercised by the
    // non-solid append/delete tests; this archive is too compressible
    // to leave a full protected sector for a damage test.)
}

#[test]
fn solid_delete_all_erases_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid4.rar");
    let p1 = make_text(10_000);
    build_solid(&path, &[("a.txt", &p1)]);
    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let a = editor.unique_entry("a.txt").unwrap();
    assert_eq!(editor.delete_entries(&[a]).unwrap(), 1);
    assert!(!path.exists(), "deleting every member erases the archive");
}

/// Regression: RAR4 solid chains with sectioned text members around
/// ~460 KB used to break from the second member on. The solid encoder
/// rolled its level-table state back to the pre-member value even when
/// an LZ member won, while the decoder keeps the member-final tables;
/// the next member's keep/delta table header was then applied to the
/// wrong base. Locked by this test (members 1 and 2 must decode).
#[test]
fn solid_sectioned_content_decode_regression() {
    let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
    let mut data = Vec::new();
    for i in 0..8_000 {
        data.extend_from_slice(line);
        if i % 7 == 0 {
            data.extend_from_slice(b"\n===== section =====\n");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sect.rar");
    {
        let mut a = crate::archive::RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                compression: crate::version::ArchiveVersion::V29,
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        a.add_bytes("a.txt", &data, 3).unwrap();
        a.add_bytes("b.txt", &data, 3).unwrap();
        a.close().unwrap();
    }
    let mut a = crate::archive::RarArchive::open(&path).unwrap();
    a.rar4_decode_solid_through(1)
        .expect("second solid member must decode");
}
