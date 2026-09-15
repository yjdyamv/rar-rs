#![cfg(feature = "parallel")]
//! A crafted STORE member whose packed area exceeds its declared unpacked
//! size must fail the parallel whole-archive extraction exactly like the
//! serial per-member path, without writing the excess bytes.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, ExtractOptions, RarError,
};

const MEMBER_BYTES: usize = 16 * 1024 * 1024;
const MEMBERS: usize = 5;

fn store_opts() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap())
}

/// Overwrite the declared unpacked size of `name`'s file header in place
/// (same-width vint) and repair the block header CRC. `new_size` must have
/// the same vint width as the stored value.
fn set_unpacked_size(bytes: &mut [u8], name: &str, new_size: u64) {
    let mut cursor = std::io::Cursor::new(&bytes[..]);
    cursor.set_position(8);
    while let Ok(Some(meta)) = rar_rs::wire::read_block(&mut cursor, None) {
        if meta.block_type == 0x02 && support::file_header_name(&meta.raw.header_data) == name {
            let mut header = meta.header_bytes.clone();
            let (_, body_start) = support::read_vint(&header, 4); // size vint
            let (_, n) = support::read_vint(&header, body_start); // block type
            let mut off = n;
            let (block_flags, n) = support::read_vint(&header, off);
            off = n;
            if block_flags & 0x0001 != 0 {
                let (_, n) = support::read_vint(&header, off);
                off = n;
            }
            if block_flags & 0x0002 != 0 {
                let (_, n) = support::read_vint(&header, off);
                off = n;
            }
            let (_, n) = support::read_vint(&header, off); // file flags
            off = n;
            let (_, after) = support::read_vint(&header, off); // unpacked size
            let encoded = rar_rs::wire::vint::encode(new_size);
            assert_eq!(encoded.len(), after - off, "patch must keep the vint width");
            header[off..after].copy_from_slice(&encoded);
            let crc = crc32fast::hash(&header[4..]);
            header[..4].copy_from_slice(&crc.to_le_bytes());
            let start = meta.block_start as usize;
            bytes[start..start + header.len()].copy_from_slice(&header);
            return;
        }
        cursor.set_position(meta.data_end);
    }
    panic!("member {name} not found");
}

/// Build a parallel-eligible archive (5 STORE members, 80 MiB packed) and
/// shrink one member's declared unpacked size below its packed size.
fn build_bound_archive(path: &std::path::Path) {
    let payload = vec![0x5Au8; MEMBER_BYTES];
    let mut writer = ArchiveWriter::create(path).unwrap();
    for i in 0..MEMBERS {
        writer
            .add_bytes(&format!("member{i}.bin"), &payload, store_opts())
            .unwrap();
    }
    writer.finish().unwrap();

    let mut bytes = std::fs::read(path).unwrap();
    // 8 MiB encodes in the same 4 vint bytes as the stored 16 MiB.
    set_unpacked_size(&mut bytes, "member2.bin", 8 * 1024 * 1024);
    std::fs::write(path, &bytes).unwrap();
}

#[test]
fn parallel_store_packed_over_unpacked_errors_like_serial() {
    let dir = make_temp_dir();
    let archive = dir.path().join("store-bound.rar");
    build_bound_archive(&archive);

    // Serial control: the per-member decoder rejects the mismatch.
    let serial_out = dir.path().join("serial");
    let mut reader = ArchiveReader::open(&archive).unwrap();
    let id = reader.unique_entry("member2.bin").unwrap();
    let serial_err = reader
        .extract_entry_with_options(id, &serial_out, ExtractOptions::default())
        .unwrap_err();
    assert!(
        matches!(serial_err, RarError::Format(_)),
        "serial: {serial_err:?}"
    );

    // The whole-archive extraction is parallel-eligible (5 members >= 4,
    // 72 MiB total unpacked >= 64 MiB) and must report the identical error.
    let parallel_out = dir.path().join("parallel");
    let mut reader = ArchiveReader::open(&archive).unwrap();
    let parallel_err = reader
        .extract_all_with_options(&parallel_out, ExtractOptions::default())
        .unwrap_err();
    assert!(
        matches!(parallel_err, RarError::Format(_)),
        "parallel: {parallel_err:?}"
    );
    assert_eq!(
        serial_err.to_string(),
        parallel_err.to_string(),
        "parallel and serial must report the same format error"
    );
    assert!(
        !parallel_out.join("member2.bin").exists(),
        "the bound-breaking member must not be written"
    );
    assert!(
        !parallel_out.join("member3.bin").exists(),
        "members after the failure must not be replayed"
    );
    assert!(
        parallel_out.join("member0.bin").exists(),
        "precondition: the parallel path replayed the earlier members"
    );
}

/// The mirror image: a STORE member whose packed area is *shorter* than its
/// declared unpacked size must fail identically in parallel and serial. The
/// parallel phase used to extract the truncated bytes whenever the stored
/// CRC happened to cover exactly the bytes present; it now shares
/// `payload::decode_member`'s size check with the serial path.
#[test]
fn parallel_short_store_payload_errors_like_serial() {
    let dir = make_temp_dir();
    let archive = dir.path().join("store-short.rar");
    let payload = vec![0x5Au8; MEMBER_BYTES];
    {
        let mut writer = ArchiveWriter::create(&archive).unwrap();
        for i in 0..MEMBERS {
            writer
                .add_bytes(&format!("member{i}.bin"), &payload, store_opts())
                .unwrap();
        }
        writer.finish().unwrap();
    }
    let mut bytes = std::fs::read(&archive).unwrap();
    // 17 MiB encodes in the same 4 vint bytes as the stored 16 MiB.
    set_unpacked_size(&mut bytes, "member2.bin", 17 * 1024 * 1024);
    std::fs::write(&archive, &bytes).unwrap();

    let serial_out = dir.path().join("serial");
    let mut reader = ArchiveReader::open(&archive).unwrap();
    let id = reader.unique_entry("member2.bin").unwrap();
    let serial_err = reader
        .extract_entry_with_options(id, &serial_out, ExtractOptions::default())
        .unwrap_err();
    assert!(
        matches!(serial_err, RarError::Format(_)),
        "serial: {serial_err:?}"
    );

    let parallel_out = dir.path().join("parallel");
    let mut reader = ArchiveReader::open(&archive).unwrap();
    let parallel_err = reader
        .extract_all_with_options(&parallel_out, ExtractOptions::default())
        .unwrap_err();
    assert!(
        matches!(parallel_err, RarError::Format(_)),
        "parallel: {parallel_err:?}"
    );
    assert_eq!(
        serial_err.to_string(),
        parallel_err.to_string(),
        "parallel and serial must report the same short-payload error"
    );
    assert!(
        !parallel_out.join("member2.bin").exists(),
        "the short member must not be written"
    );
}
