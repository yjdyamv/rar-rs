#![cfg(feature = "parallel")]
//! Parallel extraction must honor `flat_paths`, `skip_existing` and
//! `auto_rename` exactly like the serial `extract_entry` path, and decode
//! compressed members through the same `payload::decode_member`.
//!
//! The archives here are deliberately eligible for the parallel path:
//! 4 members, 4 x 16 MiB unpacked (>= 64 MiB) and no solid chain.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, ExtractOptions};
use std::path::Path;

const MEMBER_BYTES: usize = 16 * 1024 * 1024;
const MEMBERS: usize = 4;

fn opts(level: u8) -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::try_from(level).unwrap())
}

fn member_name(i: usize) -> String {
    format!("sub/member{i}.bin")
}

/// Build a parallel-eligible archive and return the member payload.
fn build_eligible_archive(path: &Path) -> Vec<u8> {
    let payload = vec![0x5Au8; MEMBER_BYTES];
    let mut writer = ArchiveWriter::create(path).unwrap();
    for i in 0..MEMBERS {
        writer
            .add_bytes(&member_name(i), &payload, opts(0))
            .unwrap();
    }
    writer.finish().unwrap();
    payload
}

fn flat_options() -> ExtractOptions {
    ExtractOptions {
        flat_paths: true,
        ..Default::default()
    }
}

#[test]
fn parallel_flat_extraction_matches_serial() {
    let dir = make_temp_dir();
    let archive = dir.path().join("flat.rar");
    let payload = build_eligible_archive(&archive);

    let parallel_out = dir.path().join("parallel");
    let mut reader = ArchiveReader::open(&archive).unwrap();
    let report = reader
        .extract_all_with_options(&parallel_out, flat_options())
        .unwrap();
    assert_eq!(report.written_count(), MEMBERS);
    assert_eq!(report.skipped_count(), 0);

    let serial_out = dir.path().join("serial");
    let mut reader = ArchiveReader::open(&archive).unwrap();
    for i in 0..MEMBERS {
        let id = reader.unique_entry(&member_name(i)).unwrap();
        reader
            .extract_entry_with_options(id, &serial_out, flat_options())
            .unwrap();
    }

    for i in 0..MEMBERS {
        let name = format!("member{i}.bin");
        let parallel = std::fs::read(parallel_out.join(&name)).unwrap();
        let serial = std::fs::read(serial_out.join(&name)).unwrap();
        assert!(parallel == payload, "parallel flat payload {i}");
        assert!(serial == payload, "serial flat payload {i}");
    }
    assert!(
        !parallel_out.join("sub").exists(),
        "parallel flat mode must not create subdirectories"
    );
    assert!(
        !serial_out.join("sub").exists(),
        "serial flat mode must not create subdirectories"
    );
}

/// Compressed (non-STORE) members decode through the same
/// `payload::decode_member` as the serial paths, in parallel; this pins the
/// parity the earlier inline decode could drift from.
#[test]
fn parallel_compressed_extraction_matches_serial() {
    let dir = make_temp_dir();
    let archive = dir.path().join("compressed.rar");
    let payload: Vec<u8> = (0..MEMBER_BYTES).map(|i| (i % 251) as u8).collect();
    {
        let mut writer = ArchiveWriter::create(&archive).unwrap();
        for i in 0..MEMBERS {
            writer
                .add_bytes(&member_name(i), &payload, opts(3))
                .unwrap();
        }
        writer.finish().unwrap();
    }
    // The archive must actually exercise the compressed branch: STORE
    // members decode through a different path inside `decode_member`.
    let reader = ArchiveReader::open(&archive).unwrap();
    for i in 0..MEMBERS {
        let id = reader.unique_entry(&member_name(i)).unwrap();
        assert_ne!(
            reader.entry(id).unwrap().method(),
            0,
            "member {i} must be compressed for this test to cover the branch"
        );
    }

    let parallel_out = dir.path().join("parallel");
    let mut reader = ArchiveReader::open(&archive).unwrap();
    reader
        .extract_all_with_options(&parallel_out, ExtractOptions::default())
        .unwrap();

    let serial_out = dir.path().join("serial");
    let mut reader = ArchiveReader::open(&archive).unwrap();
    for i in 0..MEMBERS {
        let id = reader.unique_entry(&member_name(i)).unwrap();
        reader
            .extract_entry_with_options(id, &serial_out, ExtractOptions::default())
            .unwrap();
    }

    for i in 0..MEMBERS {
        let path = member_name(i);
        let parallel = std::fs::read(parallel_out.join(&path)).unwrap();
        let serial = std::fs::read(serial_out.join(&path)).unwrap();
        assert_eq!(parallel, payload, "parallel compressed payload {i}");
        assert_eq!(serial, payload, "serial compressed payload {i}");
    }
}

#[test]
fn parallel_skip_existing_matches_serial() {
    let dir = make_temp_dir();
    let archive = dir.path().join("skip.rar");
    build_eligible_archive(&archive);

    let parallel_out = dir.path().join("parallel");
    let serial_out = dir.path().join("serial");
    std::fs::create_dir_all(parallel_out.join("sub")).unwrap();
    std::fs::create_dir_all(serial_out.join("sub")).unwrap();
    for i in 0..MEMBERS {
        let sentinel = format!("sentinel-{i}");
        std::fs::write(parallel_out.join(member_name(i)), &sentinel).unwrap();
        std::fs::write(serial_out.join(member_name(i)), &sentinel).unwrap();
    }

    let options = ExtractOptions {
        skip_existing: true,
        ..Default::default()
    };
    let mut reader = ArchiveReader::open(&archive).unwrap();
    let report = reader
        .extract_all_with_options(&parallel_out, options)
        .unwrap();
    assert_eq!(report.written_count(), 0);
    assert_eq!(report.skipped_count(), MEMBERS);
    assert_eq!(
        report.skipped()[0],
        parallel_out.join(member_name(0)),
        "skipped members keep archive order and their resolved paths"
    );

    let mut reader = ArchiveReader::open(&archive).unwrap();
    for i in 0..MEMBERS {
        let id = reader.unique_entry(&member_name(i)).unwrap();
        reader
            .extract_entry_with_options(id, &serial_out, options)
            .unwrap();
    }

    for i in 0..MEMBERS {
        let sentinel = format!("sentinel-{i}");
        assert_eq!(
            std::fs::read(parallel_out.join(member_name(i))).unwrap(),
            sentinel.as_bytes(),
            "parallel -o- must not overwrite existing member {i}"
        );
        assert_eq!(
            std::fs::read(serial_out.join(member_name(i))).unwrap(),
            sentinel.as_bytes(),
            "serial -o- must not overwrite existing member {i}"
        );
    }
}

#[test]
fn parallel_auto_rename_matches_serial() {
    let dir = make_temp_dir();
    let archive = dir.path().join("rename.rar");
    build_eligible_archive(&archive);

    let parallel_out = dir.path().join("parallel");
    let serial_out = dir.path().join("serial");
    std::fs::create_dir_all(parallel_out.join("sub")).unwrap();
    std::fs::create_dir_all(serial_out.join("sub")).unwrap();
    for i in 0..MEMBERS {
        let sentinel = format!("sentinel-{i}");
        std::fs::write(parallel_out.join(member_name(i)), &sentinel).unwrap();
        std::fs::write(serial_out.join(member_name(i)), &sentinel).unwrap();
    }

    let options = ExtractOptions {
        auto_rename: true,
        ..Default::default()
    };
    let mut reader = ArchiveReader::open(&archive).unwrap();
    reader
        .extract_all_with_options(&parallel_out, options)
        .unwrap();

    let mut reader = ArchiveReader::open(&archive).unwrap();
    let mut serial_paths = Vec::new();
    for i in 0..MEMBERS {
        let id = reader.unique_entry(&member_name(i)).unwrap();
        serial_paths.push(
            reader
                .extract_entry_with_options(id, &serial_out, options)
                .unwrap(),
        );
    }

    for (i, serial_path) in serial_paths.iter().enumerate() {
        let sentinel = format!("sentinel-{i}");
        assert_eq!(
            std::fs::read(parallel_out.join(member_name(i))).unwrap(),
            sentinel.as_bytes(),
            "parallel -or must leave the colliding file alone ({i})"
        );
        let renamed = format!("sub/member{i}(1).bin");
        assert!(
            parallel_out.join(&renamed).exists(),
            "parallel -or must create {renamed}"
        );
        assert_eq!(
            serial_path.file_name().and_then(|n| n.to_str()),
            Some(format!("member{i}(1).bin").as_str()),
            "serial -or path {i}"
        );
        assert!(
            serial_out.join(&renamed).exists(),
            "serial -or must create {renamed}"
        );
    }
}

/// `-kb` (keep broken): the parallel replay stages a failed member's decoded
/// bytes through the same materialization policy as the serial path, so the
/// partial output is kept exactly when `-kb` asks for it; the failure still
/// aborts the run and later members stay unwritten.
#[test]
fn parallel_keep_broken_matches_serial() {
    let dir = make_temp_dir();
    let archive = dir.path().join("broken.rar");
    build_eligible_archive(&archive);
    let victim_name = member_name(MEMBERS - 1);

    // The victim must be a STORE member so the corrupted payload still
    // decodes into the full output and only the CRC check fails.
    {
        let reader = ArchiveReader::open(&archive).unwrap();
        let id = reader.unique_entry(&victim_name).unwrap();
        assert_eq!(reader.entry(id).unwrap().method(), 0);
    }
    let mut bytes = std::fs::read(&archive).unwrap();
    let payload = file_data_offset(&bytes, &victim_name);
    bytes[payload] ^= 0xFF;
    std::fs::write(&archive, &bytes).unwrap();

    for keep_broken in [false, true] {
        let options = ExtractOptions {
            keep_broken,
            ..Default::default()
        };
        let parallel_out = dir.path().join(format!("par-{keep_broken}"));
        let serial_out = dir.path().join(format!("ser-{keep_broken}"));

        let mut reader = ArchiveReader::open(&archive).unwrap();
        let err = reader
            .extract_all_with_options(&parallel_out, options)
            .unwrap_err();
        assert!(matches!(err, rar_rs::RarError::Crc { .. }), "{err}");

        let mut reader = ArchiveReader::open(&archive).unwrap();
        for i in 0..MEMBERS - 1 {
            let id = reader.unique_entry(&member_name(i)).unwrap();
            reader
                .extract_entry_with_options(id, &serial_out, options)
                .unwrap();
        }
        let id = reader.unique_entry(&victim_name).unwrap();
        let err = reader
            .extract_entry_with_options(id, &serial_out, options)
            .unwrap_err();
        assert!(matches!(err, rar_rs::RarError::Crc { .. }), "{err}");

        for out in [&parallel_out, &serial_out] {
            assert!(
                out.join(member_name(0)).exists(),
                "members before the failure land ({out:?})"
            );
            let kept = out.join(&victim_name);
            assert_eq!(
                kept.exists(),
                keep_broken,
                "-kb decides whether {kept:?} stays on disk"
            );
            if keep_broken {
                assert_eq!(
                    std::fs::metadata(&kept).unwrap().len(),
                    MEMBER_BYTES as u64,
                    "the STORE victim decodes fully before the CRC check"
                );
            }
        }
    }
}
