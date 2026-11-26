//! CRC-valid but field-hostile headers.
//!
//! `corruption.rs` flips raw bytes, which almost always breaks a block's CRC —
//! so the *field-level* parser behind the CRC (vint lengths, sizes, method,
//! dictionary log, name length) is barely reached by hostile values. This suite
//! patches header fields and **recomputes the CRC**, so the reader accepts the
//! block and must then classify the hostile field: bounded, no panic, no
//! invented or wrong bytes, and no staged output left behind.
//!
//! Every case is driven through the whole read surface — open, list, read,
//! verify, extract (with tight caps) — plus `repair_archive` and
//! `reconstruct_archive_path`.

#![allow(dead_code)] // the shared support module exposes more than this suite drives

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use rar_rs::{
    ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryId, EntryWriteOptions,
    ExtractOptions, OpenOptions, ScanStrategy, WriterOptions,
};

const STORED_NAME: &str = "stored.bin";
const PACKED_NAME: &str = "packed.bin";

fn store() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn normal() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL)
}

fn stored_payload() -> Vec<u8> {
    (0..512u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect()
}

fn packed_payload() -> Vec<u8> {
    b"the quick brown fox jumps over the lazy dog\n".repeat(48)
}

/// A RAR5 archive (no quick-open: the header bytes are the only name source).
fn build_rar5(dir: &Path) -> Vec<u8> {
    let path = dir.join("h5.rar");
    let mut writer = ArchiveWriter::create_with(&path, WriterOptions::new()).unwrap();
    writer
        .add_bytes(STORED_NAME, &stored_payload(), store())
        .unwrap();
    writer
        .add_bytes(PACKED_NAME, &packed_payload(), normal())
        .unwrap();
    writer.finish().unwrap();
    std::fs::read(&path).unwrap()
}

/// A RAR4 (`unp_ver 29`) archive: fixed-width header fields, 16-bit head CRC.
fn build_rar4(dir: &Path) -> Vec<u8> {
    let path = dir.join("h4.rar");
    let mut writer =
        ArchiveWriter::create_with(&path, WriterOptions::new().compression(ArchiveVersion::V29))
            .unwrap();
    writer
        .add_bytes(STORED_NAME, &stored_payload(), store())
        .unwrap();
    writer
        .add_bytes(PACKED_NAME, &packed_payload(), normal())
        .unwrap();
    writer.finish().unwrap();
    std::fs::read(&path).unwrap()
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Tight caps: a hostile declared size must be refused, not allocated, and a
/// hostile dictionary must be rejected rather than reserving GiBs.
fn drive(path: &Path, out: &Path) {
    let opts = ExtractOptions {
        safe_paths: true,
        max_unpacked_bytes: Some(1 << 20),
        max_total_unpacked_bytes: Some(1 << 21),
        max_dict_size: Some(1 << 20),
        ..Default::default()
    };
    if let Ok(mut reader) = ArchiveReader::open(path) {
        let ids: Vec<EntryId> = reader.entries().map(|entry| entry.id()).collect();
        for id in ids {
            let _ = reader.read_entry_with_options(id, opts.clone());
        }
        let _ = reader.verify();
        let _ = reader.extract_all_with_options(out, opts);
    }
    if let Ok(bytes) = std::fs::read(path) {
        let _ = rar_rs::repair_archive(&bytes);
    }
    let rebuilt = path.with_extension("rebuilt.rar");
    let _ = rar_rs::reconstruct_archive_path(path, &rebuilt, None);
    let _ = std::fs::remove_file(&rebuilt);
}

/// Drive one hostile archive and record a panic or a staged-output leak.
fn check(label: &str, bytes: &[u8], dir: &Path, failures: &mut Vec<String>) {
    let path = dir.join(format!("{label}.rar"));
    std::fs::write(&path, bytes).unwrap();
    let out = dir.join(format!("{label}-out"));
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drive(&path, &out))) {
        failures.push(format!("{label}: panicked: {}", panic_message(&*payload)));
        return;
    }
    if out.is_dir() {
        for entry in std::fs::read_dir(&out).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if name.contains("rar5tmp") {
                failures.push(format!("{label}: staged output left behind: {name}"));
            }
        }
    }
}

// ── RAR5 patching ───────────────────────────────────────────────────────────

/// The file-header body fields this suite patches.
#[derive(Clone, Copy)]
enum R5Field {
    /// The block's data-area size (block flag 0x02).
    DataSize,
    /// The member's declared unpacked size.
    UnpackedSize,
    /// The compression-info vint (method / dictionary log).
    Compression,
    /// The member-name length vint.
    NameLen,
}

/// `(block_start, raw_header)` of the RAR5 file block named `name`.
fn locate_rar5_file_header(bytes: &[u8], name: &str) -> (usize, Vec<u8>) {
    let mut cursor = std::io::Cursor::new(bytes);
    cursor.set_position(8); // past the signature
    while let Ok(Some(meta)) = rar_rs::wire::read_block(&mut cursor, None) {
        if meta.block_type == 0x02 && support::file_header_name(&meta.raw.header_data) == name {
            return (meta.block_start as usize, meta.header_bytes);
        }
        cursor.set_position(meta.data_end);
    }
    panic!("file block {name} not found");
}

/// Byte range of `field` in a RAR5 file-header body (past the size vint).
fn r5_field_range(body: &[u8], field: R5Field) -> std::ops::Range<usize> {
    let mut q = 0usize;
    let (_, n) = support::read_vint(body, q); // block type
    q = n;
    let (flags, n) = support::read_vint(body, q);
    q = n;
    if flags & 0x0001 != 0 {
        let (_, n) = support::read_vint(body, q); // extra-area size
        q = n;
    }
    let data = if flags & 0x0002 != 0 {
        let (_, n) = support::read_vint(body, q);
        let range = q..n;
        q = n;
        range
    } else {
        0..0
    };
    if matches!(field, R5Field::DataSize) {
        return data;
    }
    let (file_flags, n) = support::read_vint(body, q);
    q = n;
    let unpacked = {
        let (_, n) = support::read_vint(body, q);
        let range = q..n;
        q = n;
        range
    };
    if matches!(field, R5Field::UnpackedSize) {
        return unpacked;
    }
    let (_, n) = support::read_vint(body, q); // attributes
    q = n;
    if file_flags & 0x0002 != 0 {
        q += 4; // Unix mtime
    }
    if file_flags & 0x0004 != 0 {
        q += 4; // CRC32
    }
    let compression = {
        let (_, n) = support::read_vint(body, q);
        let range = q..n;
        q = n;
        range
    };
    if matches!(field, R5Field::Compression) {
        return compression;
    }
    let (_, n) = support::read_vint(body, q); // host OS
    q = n;
    let name_len = {
        let (_, n) = support::read_vint(body, q);
        q..n
    };
    if matches!(field, R5Field::NameLen) {
        return name_len;
    }
    panic!("field not found")
}

/// Rebuild the file block's header after `mutate` may change the body length:
/// re-encode the size vint, recompute the block CRC, and splice the new header
/// in place of the old (the data that follows simply shifts).
fn r5_rewrite(bytes: &mut Vec<u8>, name: &str, mutate: impl FnOnce(&mut Vec<u8>)) {
    let (start, header) = locate_rar5_file_header(bytes, name);
    let (_, body_start) = support::read_vint(&header, 4);
    let mut body = header[body_start..].to_vec();
    mutate(&mut body);
    let size_vint = rar_rs::wire::vint::encode(body.len() as u64);
    let mut new_header = Vec::with_capacity(4 + size_vint.len() + body.len());
    new_header.extend_from_slice(&[0u8; 4]);
    new_header.extend_from_slice(&size_vint);
    new_header.extend_from_slice(&body);
    let crc = crc32fast::hash(&new_header[4..]);
    new_header[..4].copy_from_slice(&crc.to_le_bytes());
    bytes.splice(start..start + header.len(), new_header);
}

fn r5_patch_field(bytes: &mut Vec<u8>, name: &str, field: R5Field, value: u64) {
    r5_rewrite(bytes, name, |body| {
        let range = r5_field_range(body, field);
        body.splice(range, rar_rs::wire::vint::encode(value));
    });
}

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("needle present")
}

// ── RAR4 patching ───────────────────────────────────────────────────────────

/// The RAR4 file-header fixed fields (RAR 3.0+ layout, name at offset 32).
const R4_PACK: usize = 7;
const R4_UNP: usize = 11;
const R4_UNP_VER: usize = 24;
const R4_METHOD: usize = 25;
const R4_NAME_SIZE: usize = 26;

/// Byte offset where the RAR4 file header named `name` starts.
fn locate_rar4_file_header(bytes: &[u8], name: &str) -> usize {
    find(bytes, name.as_bytes()) - 32
}

/// Patch fixed-width fields of the RAR4 file header and recompute its 16-bit
/// head CRC (over `[2..head_size]`, the RAR4 convention).
fn rar4_rewrite(bytes: &mut [u8], name: &str, mutate: impl FnOnce(&mut [u8])) {
    let start = locate_rar4_file_header(bytes, name);
    let head_size = u16::from_le_bytes([bytes[start + 5], bytes[start + 6]]) as usize;
    let end = start + head_size;
    mutate(&mut bytes[start..end]);
    let crc = (crc32fast::hash(&bytes[start + 2..end]) & 0xffff) as u16;
    bytes[start..start + 2].copy_from_slice(&crc.to_le_bytes());
}

fn set_u32(header: &mut [u8], offset: usize, value: u32) {
    header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// Positive controls: the patcher really produces an archive the reader
/// accepts, so a hostile case that errors is the field's fault, not the edit's.
#[test]
fn patched_headers_start_from_a_readable_archive() {
    let dir = tempfile::tempdir().unwrap();

    // RAR5: a same-length rename is visible and the payload still reads.
    let mut bytes = build_rar5(dir.path());
    r5_rewrite(&mut bytes, PACKED_NAME, |body| {
        let at = find(body, PACKED_NAME.as_bytes());
        body[at + PACKED_NAME.len() - 1] = b'X';
    });
    let path = dir.path().join("ctrl5.rar");
    std::fs::write(&path, &bytes).unwrap();
    let mut reader = ArchiveReader::open(&path).unwrap();
    let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "packed.biX"), "{names:?}");
    let id = reader.unique_entry("packed.biX").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), packed_payload());

    // RAR4: same rename.
    let mut bytes = build_rar4(dir.path());
    rar4_rewrite(&mut bytes, PACKED_NAME, |header| {
        let at = find(header, PACKED_NAME.as_bytes());
        header[at + PACKED_NAME.len() - 1] = b'X';
    });
    let path = dir.path().join("ctrl4.rar");
    std::fs::write(&path, &bytes).unwrap();
    let mut reader = ArchiveReader::open(&path).unwrap();
    let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "packed.biX"), "{names:?}");
    let id = reader.unique_entry("packed.biX").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), packed_payload());
}

/// RAR5: hostile field values the block CRC no longer guards.
#[test]
fn rar5_hostile_fields_are_classified() {
    let dir = tempfile::tempdir().unwrap();
    let mut failures: Vec<String> = Vec::new();

    // Declared unpacked size far past the cap: must be refused, never allocated.
    let mut bytes = build_rar5(dir.path());
    r5_patch_field(&mut bytes, STORED_NAME, R5Field::UnpackedSize, u64::MAX / 2);
    check("r5-unpacked-huge", &bytes, dir.path(), &mut failures);

    // Declared block data area past EOF: the walk stops, nothing is invented.
    let mut bytes = build_rar5(dir.path());
    r5_patch_field(&mut bytes, STORED_NAME, R5Field::DataSize, u64::MAX / 2);
    check("r5-datasize-huge", &bytes, dir.path(), &mut failures);

    // Dictionary log 15 (4 GiB) with a 1 MiB cap: refused, not reserved.
    let mut bytes = build_rar5(dir.path());
    r5_patch_field(&mut bytes, PACKED_NAME, R5Field::Compression, 15 << 10);
    check("r5-dict-max", &bytes, dir.path(), &mut failures);

    // Invalid compression method (7).
    let mut bytes = build_rar5(dir.path());
    r5_patch_field(&mut bytes, PACKED_NAME, R5Field::Compression, 7 << 7);
    check("r5-method-7", &bytes, dir.path(), &mut failures);

    // A codec version no decoder dispatches on (low 6 bits = 63).
    let mut bytes = build_rar5(dir.path());
    r5_patch_field(&mut bytes, PACKED_NAME, R5Field::Compression, 0x3F);
    check("r5-version-63", &bytes, dir.path(), &mut failures);

    // Name length far past the header.
    let mut bytes = build_rar5(dir.path());
    r5_patch_field(&mut bytes, STORED_NAME, R5Field::NameLen, u64::MAX / 2);
    check("r5-namelen-huge", &bytes, dir.path(), &mut failures);

    assert!(
        failures.is_empty(),
        "{} hostile RAR5 headers were unsafe: {failures:?}",
        failures.len()
    );
}

/// RAR4: hostile fixed-width fields under a valid 16-bit head CRC.
#[test]
fn rar4_hostile_fields_are_classified() {
    let dir = tempfile::tempdir().unwrap();
    let mut failures: Vec<String> = Vec::new();

    let mut bytes = build_rar4(dir.path());
    rar4_rewrite(&mut bytes, STORED_NAME, |h| {
        set_u32(h, R4_UNP, u32::MAX - 1)
    });
    check("r4-unpacked-huge", &bytes, dir.path(), &mut failures);

    let mut bytes = build_rar4(dir.path());
    rar4_rewrite(&mut bytes, STORED_NAME, |h| {
        set_u32(h, R4_PACK, u32::MAX - 1)
    });
    check("r4-packed-huge", &bytes, dir.path(), &mut failures);

    let mut bytes = build_rar4(dir.path());
    rar4_rewrite(&mut bytes, PACKED_NAME, |h| h[R4_METHOD] = 0xFF);
    check("r4-method-ff", &bytes, dir.path(), &mut failures);

    // A codec version no decoder dispatches on.
    let mut bytes = build_rar4(dir.path());
    rar4_rewrite(&mut bytes, PACKED_NAME, |h| h[R4_UNP_VER] = 0xFF);
    check("r4-unpver-ff", &bytes, dir.path(), &mut failures);

    let mut bytes = build_rar4(dir.path());
    rar4_rewrite(&mut bytes, STORED_NAME, |h| {
        h[R4_NAME_SIZE..R4_NAME_SIZE + 2].copy_from_slice(&u16::MAX.to_le_bytes());
    });
    check("r4-namesize-max", &bytes, dir.path(), &mut failures);

    assert!(
        failures.is_empty(),
        "{} hostile RAR4 headers were unsafe: {failures:?}",
        failures.len()
    );
}

/// `(data area start, data area length)` of the RAR5 quick-open service block.
fn qo_data_range(bytes: &[u8]) -> (usize, usize) {
    let mut cursor = std::io::Cursor::new(bytes);
    cursor.set_position(8);
    while let Ok(Some(meta)) = rar_rs::wire::read_block(&mut cursor, None) {
        if meta.block_type == 0x03
            && support::service_name(&meta.raw.header_data).as_deref() == Some("QO")
        {
            return (meta.data_offset as usize, meta.raw.data_size as usize);
        }
        cursor.set_position(meta.data_end);
    }
    panic!("no quick-open record");
}

/// The quick-open record's *payload* is not covered by the service block's
/// header CRC, only by a per-entry CRC. A byte damaged there must make the
/// reader reject the cache (or otherwise stay honest): it must never list a
/// member the real headers do not carry, and never return the wrong bytes for
/// a real member.
#[test]
fn quick_open_payload_damage_never_poisons_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("qo.rar");
    {
        let mut writer =
            ArchiveWriter::create_with(&path, WriterOptions::new().quick_open(true)).unwrap();
        writer
            .add_bytes(STORED_NAME, &stored_payload(), store())
            .unwrap();
        writer
            .add_bytes(PACKED_NAME, &packed_payload(), normal())
            .unwrap();
        writer.finish().unwrap();
    }
    let original: Vec<(String, Vec<u8>)> = vec![
        (STORED_NAME.to_string(), stored_payload()),
        (PACKED_NAME.to_string(), packed_payload()),
    ];
    let good = std::fs::read(&path).unwrap();
    let (start, len) = qo_data_range(&good);
    assert!(len > 0, "the fixture must carry a quick-open payload");
    let work = dir.path().join("qo-damaged.rar");

    let mut failures: Vec<String> = Vec::new();
    let step = (len / 32).max(1);
    for offset in (0..len).step_by(step) {
        let mut bytes = good.clone();
        bytes[start + offset] ^= 0xFF;
        std::fs::write(&work, &bytes).unwrap();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let Ok(mut reader) = ArchiveReader::open_with(
                &work,
                OpenOptions::new().scan_strategy(ScanStrategy::PreferQuickOpen),
            ) else {
                return; // refusing the damaged cache outright is safe
            };
            let listed: Vec<(String, EntryId)> = reader
                .entries()
                .map(|entry| (entry.name().to_string(), entry.id()))
                .collect();
            for (name, id) in listed {
                if let Ok(got) = reader.read_entry(id) {
                    match original.iter().find(|(member, _)| member == &name) {
                        Some((_, bytes)) => {
                            assert_eq!(&got, bytes, "quick-open read wrong bytes for {name}")
                        }
                        None => panic!("quick-open invented member {name}"),
                    }
                }
            }
        }));
        if let Err(payload) = outcome {
            failures.push(format!(
                "qo payload offset {offset}: {}",
                panic_message(&*payload)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "quick-open payload damage was unsafe: {failures:?}"
    );
}
