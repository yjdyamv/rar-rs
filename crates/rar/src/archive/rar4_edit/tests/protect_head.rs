//! RAR 2.5-era PROTECT_HEAD (0x78) recovery records: appending, deleting
//! and solid repacking must drop either record shape and rebuild it as the
//! modern NEWSUB `Protect+` block, whether the record sits before ENDARC
//! (RAR 2.0 layout) or after it (RAR 2.5 layout).

use crate::archive::RarArchive;
use crate::format::rar4::RAR4_METHOD_STORE;
use crate::format::rar4::write::{
    FileHeaderParams, build_endarc_single, build_file_header, encode_file_name,
};
use crate::recovery::legacy_rr::{
    build_legacy_protect_head_block, recovery_sector_count, scan_protect,
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

/// Build a stored-member archive whose 0x78 record follows ENDARC when
/// `after_endarc`, otherwise sits between the members and ENDARC.
fn archive_with_protect_head(member_blocks: &[Vec<u8>], after_endarc: bool) -> Vec<u8> {
    let mut prefix = crate::detect::RAR4_SIGNATURE.to_vec();
    prefix.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
    for block in member_blocks {
        prefix.extend_from_slice(block);
    }
    let endarc = build_endarc_single();
    if after_endarc {
        let mut out = prefix;
        out.extend_from_slice(&endarc);
        let rec = recovery_sector_count(out.len(), 50);
        out.extend_from_slice(&build_legacy_protect_head_block(&out, rec).unwrap());
        out
    } else {
        let rec = recovery_sector_count(prefix.len(), 50);
        let record = build_legacy_protect_head_block(&prefix, rec).unwrap();
        let mut out = prefix;
        out.extend_from_slice(&record);
        out.extend_from_slice(&endarc);
        out
    }
}

fn payloads() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (vec![0x41; 3_000], vec![0x42; 2_000], vec![0x43; 1_500])
}

fn delete_b_and_check(after_endarc: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ph.rar");
    let (p1, p2, p3) = payloads();
    std::fs::write(
        &path,
        archive_with_protect_head(
            &[
                file_block("a.bin", &p1),
                file_block("b.bin", &p2),
                file_block("c.bin", &p3),
            ],
            after_endarc,
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

    // The old 0x78 record is gone and a fresh NEWSUB record protects the
    // rewritten prefix.
    let bytes = std::fs::read(&path).unwrap();
    let scan = scan_protect(&bytes).unwrap();
    let protect = scan.protect.expect("rebuilt recovery record");
    assert_eq!(&protect.mark, b"Protect+");

    // The rebuilt parity is usable: damage a protected sector and repair.
    let mut damaged = bytes.clone();
    damaged[600..680].fill(0x33);
    let damaged_path = dir.path().join("dmg.rar");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let fixed_path = dir.path().join("fixed.rar");
    assert!(
        crate::recovery::repair_legacy_archive_path(&damaged_path, &fixed_path)
            .unwrap()
            .repaired
    );
    assert_eq!(std::fs::read(&fixed_path).unwrap(), bytes);
}

#[test]
fn delete_with_protect_head_before_endarc() {
    delete_b_and_check(false);
}

#[test]
fn delete_with_protect_head_after_endarc() {
    delete_b_and_check(true);
}

#[test]
fn append_to_protect_head_after_endarc_keeps_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ph-append.rar");
    let (p1, _p2, _p3) = payloads();
    std::fs::write(
        &path,
        archive_with_protect_head(&[file_block("a.bin", &p1)], true),
    )
    .unwrap();
    let p4 = vec![0x44; 2_500];
    {
        let mut archive = RarArchive::open_append(&path).unwrap();
        archive.add_bytes("d.bin", &p4, 0).unwrap();
        archive.close().unwrap();
    }
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "d.bin"]
    );
    assert_eq!(
        archive
            .read_with_options("a.bin", Default::default())
            .unwrap(),
        p1
    );
    assert_eq!(
        archive
            .read_with_options("d.bin", Default::default())
            .unwrap(),
        p4
    );
    let bytes = std::fs::read(&path).unwrap();
    assert!(scan_protect(&bytes).unwrap().protect.is_some());
    // The rebuilt record must not carry over the stale RAR 2.0 practice of
    // repeating the 0x78 block at the old end-of-archive position.
    let scan = scan_protect(&bytes).unwrap();
    assert_eq!(&scan.protect.unwrap().mark, b"Protect+");
}

/// Solid repack path: a RAR 2.5 record carried on a solid archive is
/// translated into a percent and rebuilt, not refused.
#[test]
fn solid_repack_accepts_protect_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ph-solid.rar");
    let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
    let p1: Vec<u8> = line.iter().copied().cycle().take(40_000).collect();
    let p2: Vec<u8> = line.iter().copied().cycle().take(30_000).collect();
    {
        let mut a = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                compression: crate::version::ArchiveVersion::V29,
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        a.add_bytes("a.txt", &p1, 3).unwrap();
        a.add_bytes("b.txt", &p2, 3).unwrap();
        a.close().unwrap();
    }
    // Convert the (absent) record into a 0x78 block after ENDARC.
    let bytes = std::fs::read(&path).unwrap();
    let rec = recovery_sector_count(bytes.len(), 50);
    let mut with_record = bytes;
    with_record.extend_from_slice(&build_legacy_protect_head_block(&with_record, rec).unwrap());
    std::fs::write(&path, &with_record).unwrap();

    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let b = editor.unique_entry("b.txt").unwrap();
    editor
        .apply(crate::archive::editor::EditPlan::new().delete(b))
        .unwrap();
    drop(editor);

    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.txt"]
    );
    assert_eq!(
        archive
            .read_with_options("a.txt", Default::default())
            .unwrap(),
        p1
    );
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        &scan_protect(&bytes).unwrap().protect.unwrap().mark,
        b"Protect+"
    );
}
