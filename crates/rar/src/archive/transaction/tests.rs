use crate::archive::RarArchive;
use crate::error::RarError;
use crate::format::rar5::{
    BLOCK_FLAG_DATA_AREA, BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_TYPE_END_ARCHIVE,
    BLOCK_TYPE_SERVICE_HEADER, RAR5_SIGNATURE, vint,
};

/// A block as it sits on disk: CRC over `[size vint][body]`, then the
/// size vint and the body, then the data area.
fn block(body: &[u8], data: &[u8]) -> Vec<u8> {
    let mut header = vint::encode(body.len() as u64);
    header.extend_from_slice(body);
    let mut out = crc32fast::hash(&header).to_le_bytes().to_vec();
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    out
}

/// A minimal single-volume archive whose only service block is a "CMT"
/// comment declaring `declared` bytes of data while actually carrying
/// `payload`. A hand-made archive can make the two disagree and still
/// pass the header CRC, which is exactly what the size cap defends.
fn archive_with_comment(declared: u64, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(vint::encode(BLOCK_TYPE_SERVICE_HEADER));
    // Data area only: no extra records, so no extra-size field.
    body.extend(vint::encode(BLOCK_FLAG_DATA_AREA));
    body.extend(vint::encode(declared));
    // File flags plus the four fixed service-header fields, all zero.
    for _ in 0..5 {
        body.extend(vint::encode(0));
    }
    body.extend(vint::encode(3));
    body.extend_from_slice(b"CMT");

    // Main header: `[type][flags][archive flags]`, everything else absent.
    let mut out = RAR5_SIGNATURE.to_vec();
    out.extend(block(
        &[
            vint::encode(BLOCK_TYPE_ARCHIVE_HEADER),
            vint::encode(0),
            vint::encode(0),
        ]
        .concat(),
        &[],
    ));
    out.extend(block(&body, payload));
    out.extend(block(
        &[vint::encode(BLOCK_TYPE_END_ARCHIVE), vint::encode(0)].concat(),
        &[],
    ));
    out
}

#[test]
fn comment_is_read_from_the_service_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cmt.rar");
    std::fs::write(&path, archive_with_comment(2, b"hi")).unwrap();

    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(archive.get_comment().unwrap().as_deref(), Some(&b"hi"[..]));
}

/// The ceiling is a caller option, not a constant: a two-byte comment is
/// fine by default, rejected when the caller lowers the cap, and accepted
/// again when the caller removes it for an archive it trusts.
#[test]
fn comment_size_cap_follows_the_extract_options() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cmt-capped.rar");
    std::fs::write(&path, archive_with_comment(2, b"hi")).unwrap();

    let mut archive = RarArchive::open(&path).unwrap();
    archive.read_ctx_mut().extract_options = crate::options::ExtractOptions {
        max_metadata_bytes: Some(1),
        ..Default::default()
    };
    assert!(
        matches!(
            archive.get_comment().unwrap_err(),
            RarError::LimitExceeded { .. }
        ),
        "a cap below the declared size must reject the block"
    );

    let mut archive = RarArchive::open(&path).unwrap();
    archive.read_ctx_mut().extract_options = crate::options::ExtractOptions {
        max_metadata_bytes: None,
        ..Default::default()
    };
    assert_eq!(archive.get_comment().unwrap().as_deref(), Some(&b"hi"[..]));
}

/// The declared size comes from the archive, so a cap has to stop it
/// before it reaches an allocation: without one the reader would try to
/// allocate the declared size and abort instead of returning an error.
#[test]
fn oversized_comment_block_is_rejected_instead_of_allocated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cmt-huge.rar");
    std::fs::write(&path, archive_with_comment(1u64 << 60, b"")).unwrap();

    let mut archive = RarArchive::open(&path).unwrap();
    let err = archive.get_comment().unwrap_err();
    assert!(
        matches!(err, RarError::LimitExceeded { .. }),
        "expected a limit error, got {err:?}"
    );
}

/// Splice a "CMT" comment service block into `bytes` right after the main
/// archive header (WinRAR's placement) and return the new bytes.
fn with_comment_block(bytes: &[u8], comment: &[u8]) -> Vec<u8> {
    use std::io::{Seek, SeekFrom};

    let mut body = Vec::new();
    body.extend(vint::encode(BLOCK_TYPE_SERVICE_HEADER));
    body.extend(vint::encode(BLOCK_FLAG_DATA_AREA));
    body.extend(vint::encode(comment.len() as u64));
    // File flags plus the fixed service-header fields, all zero.
    for _ in 0..5 {
        body.extend(vint::encode(0));
    }
    body.extend(vint::encode(3));
    body.extend_from_slice(b"CMT");

    let mut cursor = std::io::Cursor::new(bytes);
    cursor
        .seek(SeekFrom::Start(RAR5_SIGNATURE.len() as u64))
        .unwrap();
    let main = crate::format::rar5::headers::read_block(&mut cursor, None)
        .unwrap()
        .unwrap();
    assert_eq!(main.block_type, BLOCK_TYPE_ARCHIVE_HEADER);
    let insert_at = main.data_end as usize;
    let mut out = bytes.to_vec();
    out.splice(insert_at..insert_at, block(&body, comment));
    out
}

/// A multi-volume rewrite must carry the archive comment over: the re-split
/// path emits members only, so the comment used to be silently dropped.
#[test]
fn multivolume_delete_preserves_the_archive_comment() {
    use crate::archive::{ArchiveWriter, discover_volumes};
    use crate::{CompressionLevel, EntryWriteOptions, WriterOptions};

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("mv-cmt.rar");
    {
        let stored = || EntryWriteOptions::new().compression_level(CompressionLevel::STORE);
        let mut writer =
            ArchiveWriter::create_with(&base, WriterOptions::new().volume_size(32 * 1024)).unwrap();
        writer
            .add_bytes("a.bin", &vec![7u8; 100 * 1024], stored())
            .unwrap();
        writer
            .add_bytes("b.bin", &vec![8u8; 16 * 1024], stored())
            .unwrap();
        writer.finish().unwrap();
    }
    let volumes = discover_volumes(&base);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    let bytes = std::fs::read(&volumes[0]).unwrap();
    std::fs::write(&volumes[0], with_comment_block(&bytes, b"kept comment")).unwrap();

    let mut editor = crate::archive::ArchiveEditor::open(&volumes[0]).unwrap();
    let id = editor.unique_entry("b.bin").unwrap();
    editor.delete_entries(&[id]).unwrap();
    drop(editor);

    let mut archive = RarArchive::open(&volumes[0]).unwrap();
    assert_eq!(
        archive.get_comment().unwrap().as_deref(),
        Some(b"kept comment".as_slice()),
        "the re-split rewrite must re-emit the comment"
    );
    assert_eq!(
        archive.test().unwrap().1,
        0,
        "the rewritten set must verify"
    );
}

/// Opening a lone middle volume (its siblings deleted, so discovery reports
/// one path) must refuse edits instead of rewriting a fragment.
#[test]
fn editing_a_lone_volume_of_a_set_is_refused() {
    use crate::archive::{ArchiveWriter, discover_volumes};
    use crate::{CompressionLevel, EntryWriteOptions, WriterOptions};

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("lone.rar");
    {
        let mut writer =
            ArchiveWriter::create_with(&base, WriterOptions::new().volume_size(32 * 1024)).unwrap();
        writer
            .add_bytes(
                "a.bin",
                &vec![7u8; 100 * 1024],
                EntryWriteOptions::new().compression_level(CompressionLevel::STORE),
            )
            .unwrap();
        writer
            .add_bytes(
                "b.bin",
                &vec![9u8; 4 * 1024],
                EntryWriteOptions::new().compression_level(CompressionLevel::STORE),
            )
            .unwrap();
        writer.finish().unwrap();
    }
    let volumes = discover_volumes(&base);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    let lone = volumes.last().unwrap().clone();
    for earlier in &volumes[..volumes.len() - 1] {
        std::fs::remove_file(earlier).unwrap();
    }

    let mut editor = crate::archive::ArchiveEditor::open(&lone).unwrap();
    // The lone part holds whatever fragments start in it (the split prefix
    // lives in the deleted first volume).
    let id = editor
        .entries()
        .next()
        .map(|entry| entry.id())
        .expect("the lone part has at least one entry");
    let err = editor.delete_entries(&[id]).unwrap_err();
    assert!(
        matches!(err, RarError::Unsupported(_)),
        "expected an Unsupported refusal, got {err:?}"
    );
    assert!(lone.exists(), "a refused edit must leave the volume alone");
}

/// One synthetic RAR5 file header block plus its (zero-filled) data area.
fn synthetic_file_block(name: &str, packed: u64, solid: bool, is_dir: bool) -> Vec<u8> {
    use crate::format::rar5::{FILE_FLAG_CRC32, OS_UNIX};

    let fh = crate::model::FileHeader {
        name: name.to_string(),
        unpacked_size: if is_dir { 0 } else { packed },
        packed_size: if is_dir { 0 } else { packed },
        crc32_val: Some(0),
        comp_method: if is_dir { 0 } else { 3 },
        comp_solid: solid,
        is_directory: is_dir,
        file_flags: FILE_FLAG_CRC32,
        host_os: OS_UNIX,
        ..Default::default()
    };
    let mut out = fh.to_bytes();
    out.resize(out.len() + fh.packed_size as usize, 0);
    out
}

/// The solid-chain walk must treat directories as transparent, like the
/// reader's `find_solid_chain_start` does: a non-solid directory between two
/// solid members is not a chain boundary, and using it as the chain head
/// would decode the rebuilt members against the wrong (tiny) window.
#[test]
fn chain_range_walks_across_a_non_solid_directory() {
    use crate::format::rar5::headers::{ArchiveHeader, EndOfArchiveHeader};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chain-dir.rar");
    let mut bytes = RAR5_SIGNATURE.to_vec();
    bytes.extend(
        ArchiveHeader {
            flags: 0,
            extra_data: Vec::new(),
            volume_number: None,
        }
        .to_bytes(),
    );
    bytes.extend(synthetic_file_block("m0.bin", 16, false, false));
    bytes.extend(synthetic_file_block("mid/", 0, false, true));
    bytes.extend(synthetic_file_block("m1.bin", 16, true, false));
    bytes.extend(synthetic_file_block("m2.bin", 16, true, false));
    bytes.extend(EndOfArchiveHeader { flags: 0 }.to_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let archive = RarArchive::open(&path).unwrap();
    assert_eq!(archive.entries.len(), 4, "catalog scan");
    assert_eq!(
        archive.chain_range_around(2),
        Some((0, 3)),
        "the walk must cross the non-solid directory to the real chain head"
    );
}
