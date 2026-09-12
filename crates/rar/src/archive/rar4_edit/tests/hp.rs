use super::super::layout::scan_layout;

use crate::archive::RarArchive;
use crate::error::RarError;
use crate::format::rar4::{MHD_LOCK, MHD_PASSWORD, MHD_RECOVERY};
use crate::recovery::legacy_rr::{scan_protect, scan_protect_with_password};

const HP: &str = "hp-secret";

/// Deterministic, incompressible payload (a 32-bit LCG byte stream).
fn noise(n: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

fn build_hp(path: &std::path::Path, members: &[(&str, &[u8])], solid: bool) {
    let mut a = crate::archive::RarArchive::create_with_options(
        path,
        crate::options::CreateOptions {
            compression: crate::version::ArchiveVersion::V29,
            solid,
            password: Some(HP.to_string()),
            encrypt_headers: true,
            ..Default::default()
        },
    )
    .unwrap();
    for (name, data) in members {
        a.add_bytes(name, data, 3).unwrap();
    }
    a.close().unwrap();
}

/// Main-header flags of an SFX-free archive (the header starts at 7).
fn main_flags_of(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[10], bytes[11]])
}

/// Header encryption must hide member names from the raw bytes.
fn name_is_hidden(bytes: &[u8], name: &str) -> bool {
    !bytes.windows(name.len()).any(|w| w == name.as_bytes())
}

#[test]
fn hp_rename_rewrites_the_encrypted_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-rename.rar");
    let p1 = noise(6_000);
    let p2 = noise(4_000);
    build_hp(
        &path,
        &[("secret-alpha.bin", &p1), ("secret-beta.txt", &p2)],
        false,
    );
    assert!(name_is_hidden(
        &std::fs::read(&path).unwrap(),
        "secret-alpha.bin"
    ));

    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    let a = editor.unique_entry("secret-alpha.bin").unwrap();
    let report = editor
        .apply(
            crate::archive::editor::EditPlan::new()
                .rename(a, "重命名-ünï.bin")
                .rename(
                    editor.unique_entry("secret-beta.txt").unwrap(),
                    "beta-renamed.txt",
                ),
        )
        .unwrap();
    assert_eq!(report.renamed(), 2);
    drop(editor);

    let mut ar = RarArchive::open_with_password(&path, HP).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["重命名-ünï.bin", "beta-renamed.txt"]
    );
    assert_eq!(
        ar.read_with_options("重命名-ünï.bin", Default::default())
            .unwrap(),
        p1
    );
    assert_eq!(
        ar.read_with_options("beta-renamed.txt", Default::default())
            .unwrap(),
        p2
    );

    let after = std::fs::read(&path).unwrap();
    assert_ne!(
        main_flags_of(&after) & MHD_PASSWORD,
        0,
        "still header-encrypted"
    );
    assert!(name_is_hidden(&after, "beta-renamed.txt"));
    assert!(name_is_hidden(&after, "重命名-ünï.bin"));
}

#[test]
fn hp_delete_drops_the_member_and_keeps_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-del.rar");
    let (p1, p2, p3) = (noise(3_000), noise(2_000), noise(1_500));
    build_hp(
        &path,
        &[("a.bin", &p1), ("b.bin", &p2), ("c.bin", &p3)],
        false,
    );

    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    let b = editor.unique_entry("b.bin").unwrap();
    assert_eq!(editor.delete_entries(&[b]).unwrap(), 1);
    drop(editor);

    let mut ar = RarArchive::open_with_password(&path, HP).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "c.bin"]
    );
    assert_eq!(
        ar.read_with_options("a.bin", Default::default()).unwrap(),
        p1
    );
    assert_eq!(
        ar.read_with_options("c.bin", Default::default()).unwrap(),
        p3
    );
}

#[test]
fn hp_comment_sets_reads_and_removes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-cmt.rar");
    let p = noise(9_000);
    build_hp(&path, &[("a.bin", &p)], false);

    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    editor
        .apply(crate::archive::editor::EditPlan::new().set_comment("hp comment ünï".as_bytes()))
        .unwrap();
    drop(editor);
    {
        let mut ar = RarArchive::open_with_password(&path, HP).unwrap();
        assert_eq!(
            ar.get_comment().unwrap(),
            Some("hp comment ünï".as_bytes().to_vec())
        );
        assert_eq!(
            ar.read_with_options("a.bin", Default::default()).unwrap(),
            p
        );
    }

    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    editor
        .apply(crate::archive::editor::EditPlan::new().set_comment(Vec::new()))
        .unwrap();
    drop(editor);
    let mut ar = RarArchive::open_with_password(&path, HP).unwrap();
    assert_eq!(ar.get_comment().unwrap(), None);
}

#[test]
fn hp_recovery_record_rebuilds_and_repairs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-rr.rar");
    let p = noise(200_000);
    build_hp(&path, &[("big.bin", &p)], false);

    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    editor
        .apply(crate::archive::editor::EditPlan::new().set_recovery(10))
        .unwrap();
    drop(editor);

    let bytes = std::fs::read(&path).unwrap();
    assert_ne!(main_flags_of(&bytes) & MHD_RECOVERY, 0);
    // The record is only findable with the password: its header is
    // encrypted like every other block after the main header.
    assert!(
        scan_protect_with_password(&bytes, Some(HP.as_bytes()))
            .unwrap()
            .protect
            .is_some()
    );
    assert!(scan_protect(&bytes).is_err(), "no password, no record");

    let mut damaged = bytes.clone();
    damaged[1_500..1_564].fill(0x5a);
    let dmg = dir.path().join("dmg.rar");
    std::fs::write(&dmg, &damaged).unwrap();
    let fixed = dir.path().join("fixed.rar");
    assert!(
        crate::recovery::repair_legacy_archive_path_with_password(&dmg, &fixed, Some(HP)).unwrap()
    );
    assert_eq!(std::fs::read(&fixed).unwrap(), bytes);
}

#[test]
fn hp_delete_rebuilds_an_existing_recovery_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-del-rr.rar");
    let p1 = noise(120_000);
    let p2 = noise(60_000);
    build_hp(&path, &[("a.bin", &p1), ("b.bin", &p2)], false);
    {
        let mut editor =
            crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
        editor
            .apply(crate::archive::editor::EditPlan::new().set_recovery(10))
            .unwrap();
    }
    // Deleting a member rewrites the prefix, so the record is stripped
    // and rebuilt at the same strength — still under `-hp`.
    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    let a = editor.unique_entry("a.bin").unwrap();
    assert_eq!(editor.delete_entries(&[a]).unwrap(), 1);
    drop(editor);

    let mut ar = RarArchive::open_with_password(&path, HP).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["b.bin"]
    );
    assert_eq!(
        ar.read_with_options("b.bin", Default::default()).unwrap(),
        p2
    );

    let bytes = std::fs::read(&path).unwrap();
    assert_ne!(main_flags_of(&bytes) & MHD_RECOVERY, 0);
    let scan = scan_protect_with_password(&bytes, Some(HP.as_bytes()))
        .unwrap()
        .protect
        .expect("record survived the rewrite");
    assert!(scan.rec_sectors > 0);

    let mut damaged = bytes.clone();
    damaged[1_500..1_564].fill(0x77);
    let dmg = dir.path().join("dmg.rar");
    std::fs::write(&dmg, &damaged).unwrap();
    let fixed = dir.path().join("fixed.rar");
    assert!(
        crate::recovery::repair_legacy_archive_path_with_password(&dmg, &fixed, Some(HP)).unwrap()
    );
    assert_eq!(std::fs::read(&fixed).unwrap(), bytes);
}

#[test]
fn hp_lock_marks_the_archive_and_blocks_further_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-lock.rar");
    let p = noise(2_000);
    build_hp(&path, &[("a.bin", &p)], false);

    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    editor.lock().unwrap();
    let raw = std::fs::read(&path).unwrap();
    let flags = main_flags_of(&raw);
    assert_ne!(flags & MHD_LOCK, 0, "locked");
    assert_ne!(flags & MHD_PASSWORD, 0, "still header-encrypted");

    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    let a = editor.unique_entry("a.bin").unwrap();
    assert!(matches!(
        editor.apply(crate::archive::editor::EditPlan::new().rename(a, "x.bin")),
        Err(RarError::ArchiveLocked)
    ));
}

#[test]
fn hp_append_keeps_the_header_encryption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-app.rar");
    let p1 = noise(4_000);
    let p2 = noise(3_000);
    build_hp(&path, &[("a.bin", &p1)], false);
    {
        let mut a = RarArchive::open_append_with_password(&path, HP).unwrap();
        a.add_bytes("added-new.bin", &p2, 0).unwrap();
        a.close().unwrap();
    }
    let mut ar = RarArchive::open_with_password(&path, HP).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "added-new.bin"]
    );
    assert_eq!(
        ar.read_with_options("a.bin", Default::default()).unwrap(),
        p1
    );
    assert_eq!(
        ar.read_with_options("added-new.bin", Default::default())
            .unwrap(),
        p2
    );

    let raw = std::fs::read(&path).unwrap();
    assert!(name_is_hidden(&raw, "added-new.bin"));
}

#[test]
fn hp_solid_delete_repacks_under_the_same_protection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-solid.rar");
    let t1 = noise(40_000);
    let t2 = noise(35_000);
    build_hp(&path, &[("a.txt", &t1), ("b.txt", &t2)], true);

    let mut editor = crate::archive::editor::ArchiveEditor::open_with_password(&path, HP).unwrap();
    let a = editor.unique_entry("a.txt").unwrap();
    assert_eq!(editor.delete_entries(&[a]).unwrap(), 1);
    drop(editor);

    let mut ar = RarArchive::open_with_password(&path, HP).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["b.txt"]
    );
    assert_eq!(
        ar.read_with_options("b.txt", Default::default()).unwrap(),
        t2
    );

    let raw = std::fs::read(&path).unwrap();
    assert_ne!(main_flags_of(&raw) & MHD_PASSWORD, 0, "repacked under -hp");
    assert!(name_is_hidden(&raw, "b.txt"));
}

#[test]
fn hp_edits_require_the_archive_password() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp-nopw.rar");
    let p = noise(2_000);
    build_hp(&path, &[("a.bin", &p)], false);
    let bytes = std::fs::read(&path).unwrap();

    // Without the password the layout scan cannot even read the blocks.
    assert!(matches!(
        scan_layout(&bytes, 0, None),
        Err(RarError::Encrypted(_))
    ));
    // A wrong password decrypts to garbage (head_size sanity check).
    assert!(scan_layout(&bytes, 0, Some("wrong")).is_err());
    // The right one parses.
    assert!(scan_layout(&bytes, 0, Some(HP)).is_ok());
    // And the editor refuses to open without it.
    assert!(crate::archive::editor::ArchiveEditor::open(&path).is_err());
}
