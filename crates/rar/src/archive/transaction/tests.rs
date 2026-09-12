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
