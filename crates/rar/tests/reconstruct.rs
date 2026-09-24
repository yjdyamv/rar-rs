//! `reconstruct_archive_path`: decode-validated salvage of an archive that
//! carries no recovery record (WinRAR's `rar r` fallback).

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions};

fn store() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn names(items: &[String]) -> Vec<&str> {
    items.iter().map(String::as_str).collect()
}

#[test]
fn reconstruct_keeps_every_member_of_a_healthy_archive() {
    let dir = make_temp_dir();
    let src = dir.path().join("a.rar");
    let dst = dir.path().join("rebuilt.a.rar");
    {
        let mut writer = ArchiveWriter::create(&src).unwrap();
        writer.add_bytes("f1.txt", b"one", store()).unwrap();
        writer.add_bytes("f2.txt", b"two", store()).unwrap();
        writer.finish().unwrap();
    }

    let report = rar_rs::reconstruct_archive_path(&src, &dst, None).unwrap();
    assert_eq!(names(report.recovered()), ["f1.txt", "f2.txt"]);
    assert!(report.dropped().is_empty());

    let mut reader = ArchiveReader::open(&dst).unwrap();
    let id1 = reader.unique_entry("f1.txt").unwrap();
    assert_eq!(reader.read_entry(id1).unwrap(), b"one");
    let id2 = reader.unique_entry("f2.txt").unwrap();
    assert_eq!(reader.read_entry(id2).unwrap(), b"two");
}

#[test]
fn reconstruct_drops_a_member_whose_payload_is_damaged() {
    let dir = make_temp_dir();
    let src = dir.path().join("b.rar");
    let dst = dir.path().join("rebuilt.b.rar");
    {
        let mut writer = ArchiveWriter::create(&src).unwrap();
        writer
            .add_bytes("good.txt", b"good payload", store())
            .unwrap();
        writer
            .add_bytes("bad.txt", b"bad payload!", store())
            .unwrap();
        writer.finish().unwrap();
    }
    // Flip a byte inside bad.txt's stored (STORE) payload.
    let mut bytes = std::fs::read(&src).unwrap();
    let needle = b"bad payload!";
    let pos = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("stored payload in the archive");
    bytes[pos] ^= 0xFF;
    std::fs::write(&src, &bytes).unwrap();

    let report = rar_rs::reconstruct_archive_path(&src, &dst, None).unwrap();
    assert_eq!(names(report.recovered()), ["good.txt"]);
    assert_eq!(names(report.dropped()), ["bad.txt"]);

    let mut reader = ArchiveReader::open(&dst).unwrap();
    assert!(reader.unique_entry("bad.txt").is_err());
    let id = reader.unique_entry("good.txt").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), b"good payload");
}

#[test]
fn reconstruct_salvages_past_a_corrupt_header() {
    let dir = make_temp_dir();
    let src = dir.path().join("c.rar");
    let dst = dir.path().join("rebuilt.c.rar");
    {
        let mut writer = ArchiveWriter::create(&src).unwrap();
        writer.add_bytes("f1.txt", b"one", store()).unwrap();
        writer.add_bytes("f2.txt", b"two", store()).unwrap();
        writer.add_bytes("f3.txt", b"three", store()).unwrap();
        writer.finish().unwrap();
    }
    // Corrupt f2's FILE_HEADER (the bytes holding its name): the strict scan
    // fails, and the salvage scan resyncs to f3's header.
    let mut bytes = std::fs::read(&src).unwrap();
    let pos = bytes
        .windows(6)
        .position(|window| window == b"f2.txt")
        .expect("f2 header name");
    bytes[pos] ^= 0xFF;
    std::fs::write(&src, &bytes).unwrap();

    let report = rar_rs::reconstruct_archive_path(&src, &dst, None).unwrap();
    assert!(report.skipped_damage(), "a corrupt header must be reported");
    assert_eq!(names(report.recovered()), ["f1.txt", "f3.txt"]);

    let mut reader = ArchiveReader::open(&dst).unwrap();
    let rebuilt: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(rebuilt, ["f1.txt", "f3.txt"]);
    let id = reader.unique_entry("f3.txt").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), b"three");
}

#[test]
fn reconstruct_salvages_past_a_corrupt_legacy_header() {
    let dir = make_temp_dir();
    let src = dir.path().join("legacy-dmg.rar");
    let dst = dir.path().join("rebuilt.legacy-dmg.rar");
    {
        let options = rar_rs::WriterOptions::default().compression(ArchiveVersion::V29);
        let mut writer = ArchiveWriter::create_with(&src, options).unwrap();
        writer.add_bytes("f1.txt", b"one", store()).unwrap();
        writer.add_bytes("f2.txt", b"two", store()).unwrap();
        writer.finish().unwrap();
    }
    // Corrupt f2's FILE_HEAD (the bytes holding its name); the salvage scan
    // must resync to the next valid header.
    let mut bytes = std::fs::read(&src).unwrap();
    let pos = bytes
        .windows(6)
        .position(|window| window == b"f2.txt")
        .expect("f2 header name");
    bytes[pos] ^= 0xFF;
    std::fs::write(&src, &bytes).unwrap();

    let report = rar_rs::reconstruct_archive_path(&src, &dst, None).unwrap();
    assert!(
        report.skipped_damage(),
        "a corrupt legacy header must be reported"
    );
    assert!(report.legacy(), "the source is a legacy archive");
    assert_eq!(names(report.recovered()), ["f1.txt"]);

    let mut reader = ArchiveReader::open(&dst).unwrap();
    let id = reader.unique_entry("f1.txt").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), b"one");
    assert!(reader.unique_entry("f2.txt").is_err());
}

#[test]
fn reconstruct_rebuilds_a_legacy_archive_as_rar4() {
    let dir = make_temp_dir();
    let src = dir.path().join("legacy.rar");
    let dst = dir.path().join("rebuilt.legacy.rar");
    {
        let options = rar_rs::WriterOptions::default().compression(ArchiveVersion::V29);
        let mut writer = ArchiveWriter::create_with(&src, options).unwrap();
        writer.add_bytes("f.txt", b"legacy bytes", store()).unwrap();
        writer.finish().unwrap();
    }

    let report = rar_rs::reconstruct_archive_path(&src, &dst, None).unwrap();
    assert_eq!(names(report.recovered()), ["f.txt"]);
    assert!(
        report.legacy(),
        "the rebuilt container keeps the legacy family"
    );

    // The rebuilt container is RAR4, not RAR5.
    let bytes = std::fs::read(&dst).unwrap();
    assert_eq!(
        &bytes[..rar_rs::detect::RAR4_SIGNATURE.len()],
        rar_rs::detect::RAR4_SIGNATURE,
        "a legacy source rebuilds as a RAR4 container"
    );

    // Members are stored, and a STORE member in a v29 container carries the
    // official `-m0` `unp_ver` 20 (see `format::rar4::write::member_unp_ver`),
    // so the member reports V20 — a legacy (RAR4) codec, not the container's
    // V29. The container, not the member codec, is what reconstruct preserves.
    let mut reader = ArchiveReader::open(&dst).unwrap();
    let id = reader.unique_entry("f.txt").unwrap();
    let version = reader.entry(id).unwrap().version();
    assert_eq!(
        version,
        ArchiveVersion::V20,
        "a STORE member in a RAR4 container is the -m0 layout"
    );
    assert_eq!(reader.read_entry(id).unwrap(), b"legacy bytes");
}
