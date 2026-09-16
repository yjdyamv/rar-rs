//! Zero-size members must still pass stored CRC32/BLAKE2sp verification:
//! a crafted empty member whose stored checksum was tampered with must be
//! reported as corrupt by `read`, `copy`, `test` and extraction.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, RarError, WriterOptions,
};

fn opts(level: u8) -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::try_from(level).unwrap())
}

/// On-disk `[CRC32][size vint][body]` header of the file block named `name`,
/// as `(block_start, header_bytes)`.
fn locate_file_header(bytes: &[u8], name: &str) -> (usize, Vec<u8>) {
    let mut cursor = std::io::Cursor::new(bytes);
    cursor.set_position(8);
    while let Ok(Some(meta)) = rar_rs::wire::read_block(&mut cursor, None) {
        if meta.block_type == 0x02 && file_header_name(&meta.raw.header_data) == name {
            return (meta.block_start as usize, meta.header_bytes);
        }
        cursor.set_position(meta.data_end);
    }
    panic!("file block {name} not found");
}

/// Apply `patch` to the plaintext header body and repair the block header
/// CRC, so the patched archive still opens.
fn patch_header(bytes: &mut [u8], name: &str, patch: impl FnOnce(&mut [u8])) {
    let (start, mut header) = locate_file_header(bytes, name);
    let (_, body_start) = read_vint(&header, 4);
    patch(&mut header[body_start..]);
    let crc = crc32fast::hash(&header[4..]);
    header[..4].copy_from_slice(&crc.to_le_bytes());
    bytes[start..start + header.len()].copy_from_slice(&header);
}

/// Rewrite the stored CRC32 of `name` (header CRC repaired).
fn set_stored_crc(bytes: &mut [u8], name: &str, crc: u32) {
    patch_header(bytes, name, |body| {
        let mut off = 0;
        let (_, n) = read_vint(body, off);
        off = n; // block type
        let (block_flags, n) = read_vint(body, off);
        off = n;
        if block_flags & 0x0001 != 0 {
            let (_, n) = read_vint(body, off);
            off = n; // extra area size
        }
        if block_flags & 0x0002 != 0 {
            let (_, n) = read_vint(body, off);
            off = n; // data area size
        }
        let (file_flags, n) = read_vint(body, off);
        off = n;
        let (_, n) = read_vint(body, off);
        off = n; // unpacked size
        let (_, n) = read_vint(body, off);
        off = n; // attributes
        if file_flags & 0x0002 != 0 {
            off += 4; // unix mtime
        }
        assert!(
            file_flags & 0x0004 != 0,
            "member {name} has no stored CRC32"
        );
        body[off..off + 4].copy_from_slice(&crc.to_le_bytes());
    });
}

/// Flip the first byte of the stored BLAKE2sp hash of `name` (header CRC
/// repaired).
fn tamper_stored_hash(bytes: &mut [u8], name: &str) {
    patch_header(bytes, name, |body| {
        let mut off = 0;
        let (_, n) = read_vint(body, off);
        off = n; // block type
        let (block_flags, n) = read_vint(body, off);
        off = n;
        let mut extra_size = 0usize;
        if block_flags & 0x0001 != 0 {
            let (value, _) = read_vint(body, off);
            extra_size = value as usize;
        }
        let mut q = body.len() - extra_size;
        while q < body.len() {
            let (rec_size, n) = read_vint(body, q);
            let rec_end = n + rec_size as usize;
            let (rec_type, n) = read_vint(body, n);
            if rec_type == 0x02 {
                let (_, n) = read_vint(body, n);
                body[n] ^= 0xFF; // hash type, then the 32-byte value
                return;
            }
            q = rec_end;
        }
        panic!("member {name} has no hash record");
    });
}

#[test]
fn valid_empty_member_passes_verification() {
    let dir = make_temp_dir();
    let path = dir.path().join("valid-empty.rar");
    {
        let mut writer = ArchiveWriter::create(&path).unwrap();
        writer.add_bytes("empty.bin", b"", opts(0)).unwrap();
        writer.finish().unwrap();
    }

    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry("empty.bin").unwrap();
    assert!(reader.read_entry(id).unwrap().is_empty());

    let out = dir.path().join("out");
    let extracted = reader.extract_entry(id, &out).unwrap();
    assert_eq!(std::fs::metadata(&extracted).unwrap().len(), 0);

    drop(reader);
    let mut archive = rar_rs::archive::RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.test().unwrap(),
        (1, 0),
        "a valid empty member must pass verification"
    );
}

#[test]
fn empty_member_crc_mismatch_fails_read_test_and_extract() {
    let dir = make_temp_dir();
    let path = dir.path().join("bad-crc.rar");
    {
        let mut writer = ArchiveWriter::create(&path).unwrap();
        writer.add_bytes("empty.bin", b"", opts(0)).unwrap();
        writer.finish().unwrap();
    }

    let mut bytes = std::fs::read(&path).unwrap();
    set_stored_crc(&mut bytes, "empty.bin", 0xDEAD_BEEF);
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry("empty.bin").unwrap();
    let err = reader.read_entry(id).unwrap_err();
    assert!(matches!(err, RarError::Crc { .. }), "read: {err}");

    let mut copy = Vec::new();
    let err = reader.copy_entry_to(id, &mut copy).unwrap_err();
    assert!(matches!(err, RarError::Crc { .. }), "copy: {err}");

    let out = dir.path().join("out");
    let err = reader.extract_entry(id, &out).unwrap_err();
    assert!(matches!(err, RarError::Crc { .. }), "extract: {err}");
    assert!(
        !out.join("empty.bin").exists(),
        "a failed extraction must not leave output behind"
    );

    drop(reader);
    let mut archive = rar_rs::archive::RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.test().unwrap(),
        (1, 1),
        "test must report the tampered empty member"
    );
}

/// The parallel whole-archive path must verify empty members too: its decode
/// phase skips `decode_member` for a zero-size payload but must still run the
/// integrity check, or a crafted zero-size header slips through.
#[cfg(feature = "parallel")]
#[test]
fn parallel_extraction_rejects_tampered_empty_member() {
    const BIG: usize = 17 * 1024 * 1024;
    let dir = make_temp_dir();
    let path = dir.path().join("parallel-empty.rar");
    {
        let mut writer = ArchiveWriter::create(&path).unwrap();
        for i in 0..4 {
            writer
                .add_bytes(&format!("big{i}.bin"), &vec![0x5A; BIG], opts(0))
                .unwrap();
        }
        writer.add_bytes("empty.bin", b"", opts(0)).unwrap();
        writer.finish().unwrap();
    }

    let mut bytes = std::fs::read(&path).unwrap();
    set_stored_crc(&mut bytes, "empty.bin", 0xDEAD_BEEF);
    std::fs::write(&path, &bytes).unwrap();

    let out = dir.path().join("out");
    let mut reader = ArchiveReader::open(&path).unwrap();
    let err = reader
        .extract_all_with_options(&out, rar_rs::ExtractOptions::default())
        .unwrap_err();
    assert!(matches!(err, RarError::Crc { .. }), "parallel: {err}");
    assert!(
        !out.join("empty.bin").exists(),
        "the tampered empty member must not land"
    );
    assert!(
        out.join("big0.bin").exists(),
        "members before the failure land"
    );
}

#[test]
fn empty_member_hash_mismatch_fails_read_and_test() {
    let dir = make_temp_dir();
    let path = dir.path().join("bad-hash.rar");
    {
        let mut writer =
            ArchiveWriter::create_with(&path, WriterOptions::default().blake2(true)).unwrap();
        writer.add_bytes("empty.bin", b"", opts(0)).unwrap();
        writer.finish().unwrap();
    }

    // Positive control: the stored hash is the fixed BLAKE2sp of empty.
    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry("empty.bin").unwrap();
    assert!(reader.read_entry(id).unwrap().is_empty());
    drop(reader);

    let mut bytes = std::fs::read(&path).unwrap();
    tamper_stored_hash(&mut bytes, "empty.bin");
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry("empty.bin").unwrap();
    let err = reader.read_entry(id).unwrap_err();
    assert!(matches!(err, RarError::HashMismatch { .. }), "read: {err}");

    drop(reader);
    let mut archive = rar_rs::archive::RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.test().unwrap(),
        (1, 1),
        "test must report the tampered empty-member hash"
    );
}
