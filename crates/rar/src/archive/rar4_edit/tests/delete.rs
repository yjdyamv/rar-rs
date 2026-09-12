use crate::archive::RarArchive;
use crate::error::RarError;
use crate::format::rar4::RAR4_METHOD_STORE;
use crate::format::rar4::write::{
    FileHeaderParams, build_endarc, build_file_header, encode_file_name,
};
use crate::recovery::legacy_rr::{
    build_legacy_recovery_block, recovery_sector_count, scan_protect,
};

fn file_block(name: &str, payload: &[u8]) -> Vec<u8> {
    let (name_bytes, _flags) = encode_file_name(name);
    let mut h = build_file_header(&FileHeaderParams {
        flags: 0,
        packed_size: payload.len() as u32,
        unpacked_size: payload.len() as u32,
        host_os: 0,
        file_crc: crate::crc32::crc32(payload),
        file_time: 0,
        unp_ver: 20,
        method: RAR4_METHOD_STORE,
        name: &name_bytes,
        attr: 0x20,
        window_bits: 0,
        salt: None,
        ext_time: None,
    })
    .unwrap();
    h.extend_from_slice(payload);
    h
}

fn archive_bytes(member_blocks: &[Vec<u8>], with_rr: bool) -> Vec<u8> {
    let mut prefix = crate::detect::RAR4_SIGNATURE.to_vec();
    prefix.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
    for block in member_blocks {
        prefix.extend_from_slice(block);
    }
    let mut out = prefix;
    if with_rr {
        let rec = recovery_sector_count(out.len(), 10);
        let rr = build_legacy_recovery_block(&out, rec).unwrap();
        out.extend_from_slice(&rr);
    }
    out.extend_from_slice(&build_endarc(0));
    out
}

fn payloads() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (vec![0x41; 3_000], vec![0x42; 2_000], vec![0x43; 1_500])
}

#[test]
fn delete_members_keeps_others_and_rebuilds_rr() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("del.rar");
    let (p1, p2, p3) = payloads();
    std::fs::write(
        &path,
        archive_bytes(
            &[
                file_block("a.bin", &p1),
                file_block("b.bin", &p2),
                file_block("c.bin", &p3),
            ],
            true,
        ),
    )
    .unwrap();

    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let b = editor.unique_entry("b.bin").unwrap();
    let report = editor
        .apply(crate::archive::editor::EditPlan::new().delete(b))
        .unwrap();
    assert_eq!((report.deleted(), report.renamed()), (1, 0));

    drop(editor);
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "c.bin"]
    );
    assert_eq!(
        archive
            .read_with_options("a.bin", Default::default())
            .unwrap(),
        p1
    );
    assert_eq!(
        archive
            .read_with_options("c.bin", Default::default())
            .unwrap(),
        p3
    );

    // The rebuilt record still protects the new prefix.
    let bytes = std::fs::read(&path).unwrap();
    assert!(scan_protect(&bytes).unwrap().protect.is_some());
    let mut damaged = bytes.clone();
    damaged[600..680].fill(0x33);
    let damaged_path = dir.path().join("dmg.rar");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let fixed_path = dir.path().join("fixed.rar");
    assert!(crate::recovery::repair_legacy_archive_path(&damaged_path, &fixed_path).unwrap());
    assert_eq!(std::fs::read(&fixed_path).unwrap(), bytes);
}

#[test]
fn delete_first_and_last_members() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("del2.rar");
    let (p1, p2, p3) = payloads();
    std::fs::write(
        &path,
        archive_bytes(
            &[
                file_block("a.bin", &p1),
                file_block("b.bin", &p2),
                file_block("c.bin", &p3),
            ],
            false,
        ),
    )
    .unwrap();

    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let a = editor.unique_entry("a.bin").unwrap();
    let c = editor.unique_entry("c.bin").unwrap();
    assert_eq!(
        editor.delete_entries(&[a, c]).unwrap(),
        2,
        "delete a.bin and c.bin"
    );
    drop(editor);
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["b.bin"]
    );
    assert_eq!(
        archive
            .read_with_options("b.bin", Default::default())
            .unwrap(),
        p2
    );
}

#[test]
fn delete_every_member_erases_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("del3.rar");
    let (p1, _, _) = payloads();
    std::fs::write(&path, archive_bytes(&[file_block("a.bin", &p1)], false)).unwrap();
    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let a = editor.unique_entry("a.bin").unwrap();
    assert_eq!(editor.delete_entries(&[a]).unwrap(), 1);
    assert!(!path.exists(), "deleting every member erases the archive");
    drop(editor);
    assert!(RarArchive::open(&path).is_err());
}

#[test]
fn delete_conflicts_and_solid_are_refused_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("del4.rar");
    let (p1, p2, _) = payloads();
    std::fs::write(
        &path,
        archive_bytes(&[file_block("a.bin", &p1), file_block("b.bin", &p2)], false),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let a = editor.unique_entry("a.bin").unwrap();
    // Deleting and renaming the same member in one plan is rejected.
    assert!(matches!(
        editor.apply(
            crate::archive::editor::EditPlan::new()
                .delete(a)
                .rename(a, "x.bin")
        ),
        Err(RarError::InvalidOption(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before, "untouched");

    // Solid archives refuse deletion (stage C repack).
    let solid = dir.path().join("solid.rar");
    let mut solid_archive = crate::archive::RarArchive::create_with_options(
        &solid,
        crate::options::CreateOptions {
            compression: crate::version::ArchiveVersion::V29,
            solid: true,
            ..Default::default()
        },
    )
    .unwrap();
    solid_archive.add_bytes("m1.bin", &p1, 3).unwrap();
    solid_archive.add_bytes("m2.bin", &p2, 3).unwrap();
    solid_archive.close().unwrap();
    // Solid deletes now repack (stage C): the member is removed and the
    // survivor's data is intact.
    let mut editor = crate::archive::editor::ArchiveEditor::open(&solid).unwrap();
    let m1 = editor.unique_entry("m1.bin").unwrap();
    assert_eq!(editor.delete_entries(&[m1]).unwrap(), 1);
    drop(editor);
    let mut ar = crate::archive::RarArchive::open(&solid).unwrap();
    assert_eq!(
        ar.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["m2.bin"]
    );
    assert_eq!(
        ar.read_with_options("m2.bin", Default::default()).unwrap(),
        p2
    );
}
