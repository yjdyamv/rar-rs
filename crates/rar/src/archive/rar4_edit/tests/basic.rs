use super::super::comment::decode_comment_payload;
use super::super::headers::rename_file_header;
use super::super::layout::{MHD_COMMENT, patch_main_header, scan_layout};
use super::super::{encode_comment_text, header_crc16};

use crate::archive::RarArchive;
use crate::format::rar4::RAR4_METHOD_STORE;
use crate::format::rar4::write::{
    FileHeaderParams, build_endarc, build_file_header, encode_file_name,
};
use crate::format::rar4::{COMM_HEAD, FHD_COMMENT, FHD_UNICODE, MHD_LOCK, MHD_RECOVERY};
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

fn archive_bytes(member_blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = crate::detect::RAR4_SIGNATURE.to_vec();
    out.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
    for block in member_blocks {
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&build_endarc(0));
    out
}

#[test]
fn patch_main_header_sets_bits_and_keeps_crc_valid() {
    let main = crate::format::rar4::write::build_main_header(0);
    assert_eq!(main.len(), 13);
    let patched = patch_main_header(&main, MHD_LOCK | MHD_RECOVERY).unwrap();
    let flags = u16::from_le_bytes([patched[3], patched[4]]);
    assert_ne!(flags & MHD_LOCK, 0);
    assert_ne!(flags & MHD_RECOVERY, 0);
    let crc = header_crc16(&patched[2..]);
    assert_eq!(u16::from_le_bytes([patched[0], patched[1]]), crc);
    assert_eq!(&patched[5..], &main[5..]);
}

/// A RAR 1.5–2.9 main header whose archive comment is embedded after the
/// fixed 13 bytes must survive a flag patch whole: truncating it used to
/// shift every following block by the comment length. The CRC keeps
/// covering only `[2..13]`, exactly the reader's coverage for
/// `MHD_COMMENT`.
#[test]
fn patch_main_header_preserves_an_embedded_comment() {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rar40/rar2/comment_nopsw.rar"
    ))
    .unwrap();
    let main = &fixture[7..7 + 51];
    assert_eq!(main.len(), 51);
    assert_ne!(u16::from_le_bytes([main[3], main[4]]) & MHD_COMMENT, 0);

    let patched = patch_main_header(main, MHD_RECOVERY).unwrap();
    assert_eq!(patched.len(), main.len(), "the embedded comment must stay");
    assert_eq!(&patched[13..], &main[13..], "comment bytes byte-identical");
    let flags = u16::from_le_bytes([patched[3], patched[4]]);
    assert_ne!(flags & MHD_RECOVERY, 0);
    assert_eq!(
        u16::from_le_bytes([patched[0], patched[1]]),
        header_crc16(&patched[2..13]),
        "CRC coverage stays at the fixed 13 bytes"
    );
}

/// The fixture-based RR edit (and a follow-up delete) on an archive whose
/// comment lives in the main header must keep the full 51-byte header with
/// its nested comment, stay parseable, and leave the members extractable.
/// Before the fix `patch_main_header` truncated the header to 13 bytes and
/// every later block parsed 38 bytes late.
#[test]
fn recovery_and_delete_keep_an_embedded_main_comment() {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rar40/rar2/comment_nopsw.rar"
    );
    let fixture = std::fs::read(fixture_path).unwrap();
    // The fixture predates the optional end-of-archive marker; the editor
    // requires one.
    let mut bytes = fixture.clone();
    bytes.extend_from_slice(&build_endarc(0));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rar2_cmt.rar");
    std::fs::write(&path, &bytes).unwrap();

    // Expected member bytes straight from the untouched fixture.
    let (expected_1, expected_2) = {
        let mut untouched = RarArchive::open(fixture_path).unwrap();
        (
            untouched
                .read_with_options("FILE1.TXT", Default::default())
                .unwrap(),
            untouched
                .read_with_options("FILE2.TXT", Default::default())
                .unwrap(),
        )
    };

    // `rar rr` on the comment-bearing archive: rebuild the record.
    {
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        editor
            .apply(crate::archive::editor::EditPlan::new().set_recovery(5))
            .unwrap();
    }
    let edited = std::fs::read(&path).unwrap();
    let layout = scan_layout(&edited, 0, None).unwrap();
    assert_eq!(layout.main_header.len(), 51, "full main header kept");
    assert_ne!(layout.main_flags & MHD_RECOVERY, 0, "record flag set");
    assert_eq!(
        &layout.main_header[13..],
        &fixture[20..58],
        "embedded comment unchanged"
    );
    assert_eq!(
        u16::from_le_bytes([layout.main_header[0], layout.main_header[1]]),
        header_crc16(&layout.main_header[2..13])
    );
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["FILE1.TXT", "FILE2.TXT"]
    );
    assert_eq!(
        archive
            .read_with_options("FILE1.TXT", Default::default())
            .unwrap(),
        expected_1
    );
    assert_eq!(
        archive
            .read_with_options("FILE2.TXT", Default::default())
            .unwrap(),
        expected_2
    );

    // A follow-up delete (which rebuilds the existing record) must keep the
    // embedded comment too.
    {
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let file2 = editor.unique_entry("FILE2.TXT").unwrap();
        assert_eq!(editor.delete_entries(&[file2]).unwrap(), 1);
    }
    let after_delete = std::fs::read(&path).unwrap();
    let layout = scan_layout(&after_delete, 0, None).unwrap();
    assert_eq!(layout.main_header.len(), 51);
    assert_eq!(&layout.main_header[13..], &fixture[20..58]);
    assert_eq!(
        u16::from_le_bytes([layout.main_header[0], layout.main_header[1]]),
        header_crc16(&layout.main_header[2..13])
    );
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["FILE1.TXT"]
    );
    assert_eq!(
        archive
            .read_with_options("FILE1.TXT", Default::default())
            .unwrap(),
        expected_1
    );
}

/// The layout scan routes block envelopes through the shared reader: a
/// `head_size`-7 block with LONG_BLOCK (no room for its 4-byte ADD_SIZE)
/// must be rejected cleanly instead of reading past the header.
#[test]
fn layout_scan_rejects_short_long_block() {
    let mut bytes = crate::detect::RAR4_SIGNATURE.to_vec();
    bytes.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
    let mut bad = vec![0u8; 7];
    bad[2] = 0x74; // FILE_HEAD
    bad[3..5].copy_from_slice(&0x8000u16.to_le_bytes()); // LONG_BLOCK
    bad[5..7].copy_from_slice(&7u16.to_le_bytes());
    bytes.extend_from_slice(&bad);
    bytes.extend_from_slice(&build_endarc(0));

    let err = match scan_layout(&bytes, 0, None) {
        Err(err) => err,
        Ok(_) => panic!("expected the malformed block to be rejected"),
    };
    assert!(
        matches!(err, crate::error::RarError::Format(_)),
        "expected a format error, got {err}"
    );
}

#[test]
fn rename_file_header_swaps_the_name_field_only() {
    let (name_bytes, name_flags) = encode_file_name("plain.txt");
    let header = build_file_header(&FileHeaderParams {
        flags: name_flags,
        packed_size: 100,
        unpacked_size: 100,
        host_os: 0,
        file_crc: 0x1234_5678,
        file_time: 0x9abc_def0,
        unp_ver: 20,
        method: RAR4_METHOD_STORE,
        name: &name_bytes,
        attr: 0x20,
        window_bits: 0,
        salt: Some([0x11; 8]),
        ext_time: Some(&[1, 2, 3, 4]),
    })
    .unwrap();
    let renamed = rename_file_header(&header, "重命名-ünï.txt").unwrap();

    // Fields outside the name (fixed 32-byte block + salt/ext-time tail)
    // are preserved byte-for-byte; only CRC, flags, head_size and the
    // name_size/name pair change.
    assert_eq!(&renamed[7..26], &header[7..26]);
    assert_eq!(&renamed[28..32], &header[28..32]);
    let new_hs = u16::from_le_bytes([renamed[5], renamed[6]]) as usize;
    assert_eq!(new_hs, renamed.len());
    // The new name decodes back through the reader path.
    let flags = u16::from_le_bytes([renamed[3], renamed[4]]);
    assert_ne!(flags & FHD_UNICODE, 0);
    let name_size = u16::from_le_bytes([renamed[26], renamed[27]]) as usize;
    let decoded = crate::format::rar4::decode_file_name(&renamed[32..32 + name_size], flags);
    assert_eq!(decoded, "重命名-ünï.txt");
    // CRC16 covers the whole (comment-free) header body, like the reader.
    let crc = header_crc16(&renamed[2..]);
    assert_eq!(u16::from_le_bytes([renamed[0], renamed[1]]), crc);
}

#[test]
fn rename_members_roundtrips_through_open_and_keeps_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rename.rar");
    let a_payload = vec![0x42; 5_000];
    let b_payload = vec![0x77; 3_000];
    std::fs::write(
        &path,
        archive_bytes(&[
            file_block("a.bin", &a_payload),
            file_block("b.txt", &b_payload),
        ]),
    )
    .unwrap();

    let archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "b.txt"]
    );

    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let a = editor.unique_entry("a.bin").unwrap();
    let b = editor.unique_entry("b.txt").unwrap();
    let report = editor
        .apply(
            crate::archive::editor::EditPlan::new()
                .rename(a, "alpha.bin")
                .rename(b, "贝塔.txt"),
        )
        .unwrap();
    assert_eq!(report.renamed(), 2);

    drop(editor);
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["alpha.bin", "贝塔.txt"]
    );
    assert_eq!(
        archive
            .read_with_options("alpha.bin", Default::default())
            .unwrap(),
        a_payload
    );
    assert_eq!(
        archive
            .read_with_options("贝塔.txt", Default::default())
            .unwrap(),
        b_payload
    );
}

#[test]
fn dir_rename_expands_to_descendants_and_rebuilds_rr() {
    // Hand-build: d/ (directory member) + d/x.txt, plus a NEWSUB `rr`
    // record over the prefix and an ENDARC, mirroring the writer's
    // layout (members, RR, ENDARC).
    let (dir_name, _) = encode_file_name("d/");
    let dir_head = build_file_header(&FileHeaderParams {
        flags: 0,
        packed_size: 0,
        unpacked_size: 0,
        host_os: 2,
        file_crc: 0,
        file_time: 0,
        unp_ver: 20,
        method: RAR4_METHOD_STORE,
        name: &dir_name,
        attr: 0x10, // FILE_ATTRIBUTE_DIRECTORY
        window_bits: 0,
        salt: None,
        ext_time: None,
    })
    .unwrap();
    let x_payload = vec![0x5a; 2_000];
    let mut prefix = crate::detect::RAR4_SIGNATURE.to_vec();
    prefix.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
    prefix.extend_from_slice(&dir_head);
    prefix.extend_from_slice(&file_block("d/x.txt", &x_payload));
    let rec = recovery_sector_count(prefix.len(), 10);
    let rr_block = build_legacy_recovery_block(&prefix, rec).unwrap();
    let mut full = prefix;
    full.extend_from_slice(&rr_block);
    full.extend_from_slice(&build_endarc(0));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dirs.rar");
    std::fs::write(&path, &full).unwrap();

    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let names: Vec<String> = editor.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(names, ["d/", "d/x.txt"]);
    let d = editor.unique_entry("d/").unwrap();
    let report = editor
        .apply(crate::archive::editor::EditPlan::new().rename(d, "renamed"))
        .unwrap();
    assert_eq!(report.renamed(), 1, "one explicit pair");

    drop(editor);
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["renamed/", "renamed/x.txt"]
    );
    assert_eq!(
        archive
            .read_with_options("renamed/x.txt", Default::default())
            .unwrap(),
        x_payload
    );

    // The archive still carries a valid, repairable recovery record
    // (rebuilt over the renamed prefix): damage a protected sector and
    // repair it back byte-for-byte.
    let rewritten = std::fs::read(&path).unwrap();
    assert!(scan_protect(&rewritten).unwrap().protect.is_some());
    let mut damaged = rewritten.clone();
    damaged[1_000..1_064].fill(0xcc);
    let damaged_path = dir.path().join("dmg.rar");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let fixed_path = dir.path().join("fixed.rar");
    assert!(crate::recovery::repair_legacy_archive_path(&damaged_path, &fixed_path).unwrap());
    assert_eq!(std::fs::read(&fixed_path).unwrap(), rewritten);
}

/// A genuine WinRAR 6.23 RAR4 archive carrying a UTF-8 comment stored as
/// UTF-16LE must decode back to the exact text `rar cw` would emit.
#[test]
fn reads_winrar_623_rar4_comment() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rar40/comment/comment_zh.rar"
    );
    let expected = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rar40/comment/comment.txt"
    ))
    .unwrap();
    let mut archive = RarArchive::open(fixture).unwrap();
    assert_eq!(archive.get_comment().unwrap(), Some(expected));
}

#[test]
fn comment_set_replace_and_remove_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cmt.rar");
    let payload = vec![0x44; 9_000];
    std::fs::write(&path, archive_bytes(&[file_block("a.bin", &payload)])).unwrap();

    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    // No comment yet.
    {
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.get_comment().unwrap(), None);
    }
    // Set an ASCII comment.
    editor
        .apply(crate::archive::editor::EditPlan::new().set_comment(b"first comment"))
        .unwrap();
    {
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(
            archive.get_comment().unwrap(),
            Some(b"first comment".to_vec())
        );
    }
    // Replace it with a Unicode one, combined with rr in the same plan.
    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    let a = editor.unique_entry("a.bin").unwrap();
    editor
        .apply(
            crate::archive::editor::EditPlan::new()
                .set_comment("第二段注释 ünï".as_bytes())
                .set_recovery(10)
                .rename(a, "renamed.bin"),
        )
        .unwrap();
    {
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(
            archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
            ["renamed.bin"]
        );
        assert_eq!(
            archive.get_comment().unwrap(),
            Some("第二段注释 ünï".as_bytes().to_vec())
        );
        // The combined rewrite kept the recovery record repairable.
        let bytes = std::fs::read(&path).unwrap();
        assert!(scan_protect(&bytes).unwrap().protect.is_some());
    }
    // Empty comment removes it.
    let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
    editor
        .apply(crate::archive::editor::EditPlan::new().set_comment(Vec::new()))
        .unwrap();
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(archive.get_comment().unwrap(), None);
}

#[test]
fn comment_encode_decode_symmetry() {
    for text in [b"plain ascii".as_slice(), "第二段注释 ünï".as_bytes(), b""] {
        let (payload, unicode) = encode_comment_text(text);
        assert_eq!(decode_comment_payload(&payload, unicode), text);
        assert_eq!(unicode, !text.is_ascii());
    }
    // A WinRAR UTF-16LE payload (attr marker set) decodes to the text.
    let utf16: Vec<u8> = "中文测试"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(decode_comment_payload(&utf16, true), "中文测试".as_bytes());
    // Without the marker a valid-UTF-8 payload is returned untouched.
    assert_eq!(
        decode_comment_payload("中文测试".as_bytes(), false),
        "中文测试".as_bytes()
    );
}

/// `rar cf` writes the official RAR 3.x/4.x standalone member comment: no
/// `FHD_COMMENT` nesting, a `COMM_HEAD` block after the member data whose
/// HEAD_CRC covers only the 13-byte block header. Clearing drops the block.
#[test]
fn member_comment_uses_the_standalone_comm_head_layout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cf_layout.rar");
    let payload = b"payload for the commented member".to_vec();
    let comment: &[u8] = b"release notes";
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
    {
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.txt").unwrap();
        editor
            .apply(crate::archive::editor::EditPlan::new().set_member_comment(a, comment.to_vec()))
            .unwrap();
    }

    let bytes = std::fs::read(&path).unwrap();
    let layout = scan_layout(&bytes, 0, None).unwrap();
    assert_eq!(layout.files.len(), 1);
    let (file_offset, header) = &layout.files[0];
    let flags = u16::from_le_bytes([header[3], header[4]]);
    assert_eq!(flags & FHD_COMMENT, 0, "the comment is not nested");
    let head_size = u16::from_le_bytes([header[5], header[6]]) as usize;
    let packed = u32::from_le_bytes(header[7..11].try_into().unwrap()) as usize;
    let comment_offset = file_offset + head_size + packed;
    assert_eq!(bytes[comment_offset + 2], COMM_HEAD);
    let block_size =
        u16::from_le_bytes([bytes[comment_offset + 5], bytes[comment_offset + 6]]) as usize;
    assert_eq!(block_size, 13 + comment.len());
    let head_crc = u16::from_le_bytes([bytes[comment_offset], bytes[comment_offset + 1]]);
    assert_eq!(
        head_crc,
        (crate::crc32::crc32(&bytes[comment_offset + 2..comment_offset + 13]) & 0xffff) as u16,
        "HEAD_CRC covers only the 13-byte block header"
    );
    assert_eq!(
        &bytes[comment_offset + 13..comment_offset + block_size],
        comment
    );

    // The comment reads back through the public API.
    let archive = RarArchive::open(&path).unwrap();
    assert_eq!(archive.entries[0].comment().unwrap(), comment);

    // Clearing drops the standalone block again.
    {
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.txt").unwrap();
        editor
            .apply(crate::archive::editor::EditPlan::new().set_member_comment(a, Vec::new()))
            .unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let layout = scan_layout(&bytes, 0, None).unwrap();
    let (file_offset, header) = &layout.files[0];
    let head_size = u16::from_le_bytes([header[5], header[6]]) as usize;
    let packed = u32::from_le_bytes(header[7..11].try_into().unwrap()) as usize;
    assert_eq!(
        layout.endarc_offset,
        file_offset + head_size + packed,
        "no comment block before the end-of-archive marker"
    );
    let archive = RarArchive::open(&path).unwrap();
    assert!(archive.entries[0].comment().is_none());
}
