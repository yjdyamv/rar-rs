use crate::error::RarError;
use crate::recovery::legacy_rr::scan_protect;

fn build_rar4(path: &std::path::Path, payloads: &[(&str, &[u8])]) {
    let mut a = crate::archive::RarArchive::create_with_options(
        path,
        crate::options::CreateOptions {
            compression: crate::version::ArchiveVersion::V29,
            ..Default::default()
        },
    )
    .unwrap();
    for (name, data) in payloads {
        a.add_bytes(name, data, 0).unwrap();
    }
    a.close().unwrap();
}

#[test]
fn append_adds_members_and_keeps_existing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap.rar");
    let p1 = vec![0x11; 4_000];
    let p2 = vec![0x22; 3_000];
    let p3 = vec![0x33; 2_000];
    build_rar4(&path, &[("a.bin", &p1), ("b.bin", &p2)]);
    {
        let mut a = crate::archive::RarArchive::open_append(&path).unwrap();
        a.add_bytes("c.bin", &p3, 0).unwrap();
        a.close().unwrap();
    }
    let mut a = crate::archive::RarArchive::open(&path).unwrap();
    assert_eq!(
        a.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "b.bin", "c.bin"]
    );
    assert_eq!(
        a.read_with_options("a.bin", Default::default()).unwrap(),
        p1
    );
    assert_eq!(
        a.read_with_options("b.bin", Default::default()).unwrap(),
        p2
    );
    assert_eq!(
        a.read_with_options("c.bin", Default::default()).unwrap(),
        p3
    );
}

#[test]
fn append_rebuilds_existing_rr_over_the_new_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap2.rar");
    let p1 = vec![0x44; 40_000];
    let p2 = vec![0x55; 30_000];
    build_rar4(&path, &[("a.bin", &p1)]);
    {
        let mut e = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        e.apply(crate::archive::editor::EditPlan::new().set_recovery(10))
            .unwrap();
    }
    {
        let mut a = crate::archive::RarArchive::open_append(&path).unwrap();
        a.add_bytes("b.bin", &p2, 0).unwrap();
        a.close().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(scan_protect(&bytes).unwrap().protect.is_some());
    // Damage a protected sector inside the appended member: the rebuilt
    // record must restore the exact original bytes.
    let mut damaged = bytes.clone();
    let at = bytes.len() - 8_000;
    damaged[at..at + 64].fill(0x7e);
    let dmg_path = dir.path().join("dmg.rar");
    std::fs::write(&dmg_path, &damaged).unwrap();
    let fixed = dir.path().join("fixed.rar");
    assert!(crate::recovery::repair_legacy_archive_path(&dmg_path, &fixed).unwrap());
    assert_eq!(std::fs::read(&fixed).unwrap(), bytes);
    let mut a = crate::archive::RarArchive::open(&path).unwrap();
    assert_eq!(
        a.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "b.bin"]
    );
    assert_eq!(
        a.read_with_options("b.bin", Default::default()).unwrap(),
        p2
    );
}

#[test]
fn append_solid_defers_repack_and_locked_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let p = vec![0x66; 2_000];
    // Appending to a solid archive defers to a close-time repack: the
    // new member lands after the existing chain and everything decodes.
    let solid = dir.path().join("solid.rar");
    {
        let mut a = crate::archive::RarArchive::create_with_options(
            &solid,
            crate::options::CreateOptions {
                compression: crate::version::ArchiveVersion::V29,
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        a.add_bytes("old.bin", &p, 3).unwrap();
        a.close().unwrap();
    }
    let added = vec![0x77; 3_000];
    {
        let mut a = crate::archive::RarArchive::open_append(&solid).unwrap();
        a.add_bytes("new.bin", &added, 3).unwrap();
        a.close().unwrap();
    }
    let mut ar = crate::archive::RarArchive::open(&solid).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["old.bin", "new.bin"]
    );
    assert_eq!(
        ar.read_with_options("old.bin", Default::default()).unwrap(),
        p
    );
    assert_eq!(
        ar.read_with_options("new.bin", Default::default()).unwrap(),
        added
    );
    // Locked archive.
    let locked = dir.path().join("locked.rar");
    build_rar4(&locked, &[("m.bin", &p)]);
    {
        let mut e = crate::archive::editor::ArchiveEditor::open(&locked).unwrap();
        e.lock().unwrap();
    }
    assert!(matches!(
        crate::archive::RarArchive::open_append(&locked),
        Err(RarError::ArchiveLocked)
    ));
}

#[test]
fn solid_append_preserves_comment_and_rebuilds_rr() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid-app.rar");
    let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
    let p1: Vec<u8> = varied(30_000, line);
    let p2: Vec<u8> = varied(25_000, line);
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
        a.add_bytes("a.txt", &p1, 3).unwrap();
        a.close().unwrap();
    }
    {
        let mut e = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        e.apply(crate::archive::editor::EditPlan::new().set_recovery(10))
            .unwrap();
        let mut e = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        e.apply(
            crate::archive::editor::EditPlan::new()
                .set_comment("solid append 注释".as_bytes().to_vec()),
        )
        .unwrap();
    }
    {
        let mut a = crate::archive::RarArchive::open_append(&path).unwrap();
        a.add_bytes("b.txt", &p2, 3).unwrap();
        a.close().unwrap();
    }
    let mut ar = crate::archive::RarArchive::open(&path).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.txt", "b.txt"]
    );
    assert_eq!(
        ar.read_with_options("a.txt", Default::default()).unwrap(),
        p1
    );
    assert_eq!(
        ar.read_with_options("b.txt", Default::default()).unwrap(),
        p2
    );
    assert_eq!(
        ar.get_comment().unwrap(),
        Some("solid append 注释".as_bytes().to_vec())
    );
    let bytes = std::fs::read(&path).unwrap();
    assert!(scan_protect(&bytes).unwrap().protect.is_some());
    // The rebuilt record protects the appended member (repair needs a
    // full protected sector; the varied content leaves ~30 KB packed).
    assert!(bytes.len() > 16_000, "archive should be repairable-size");
    let mut damaged = bytes.clone();
    let at = bytes.len() - 8_000;
    damaged[at..at + 64].fill(0x44);
    let dmg = dir.path().join("dmg.rar");
    std::fs::write(&dmg, &damaged).unwrap();
    let fixed = dir.path().join("fixed.rar");
    assert!(crate::recovery::repair_legacy_archive_path(&dmg, &fixed).unwrap());
    assert_eq!(std::fs::read(&fixed).unwrap(), bytes);
}

/// Moderately compressible content (indexed lines) so packed members
/// stay big enough for a recovery-record repair test.
fn varied(n: usize, line: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..n {
        out.extend_from_slice(format!("{i:08}: ").as_bytes());
        out.extend_from_slice(line);
        out.extend_from_slice(b"--variant--\n");
    }
    out
}
