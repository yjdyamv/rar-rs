//! Regression test: a volume base that itself contains `.part` (like
//! `my.partition`) must split at the LAST `.part` segment.
//!
//! `extract_volume_base` used the FIRST `.part` occurrence, so
//! `my.partition.part2.rar` found `.part` inside `partition`, the trailing
//! `ition.part2` failed the digit check and the name never parsed as a part
//! volume. Discovery then fell through to the legacy `.rar` strip and
//! creation staged its base as `my.partition.part1`, leaving nested
//! `my.partition.part1.part1.rar` names that could not be read back.

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(31) ^ (i >> 5)) as u8)
        .collect()
}

#[test]
fn part_containing_base_creates_and_discovers_its_volume_set() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("my.partition.part1.rar");
    let data = payload(60_000);

    {
        let mut writer =
            ArchiveWriter::create_with(&first, WriterOptions::default().volume_size(30_000))
                .unwrap();
        writer.add_bytes("f1.bin", &data, stored()).unwrap();
        writer.finish().unwrap();
    }

    let volumes = rar_rs::discover_volumes(&first);
    assert!(
        volumes.len() > 1,
        "the set must split into multiple volumes, got {volumes:?}"
    );
    for (index, volume) in volumes.iter().enumerate() {
        assert_eq!(
            volume.file_name().unwrap().to_string_lossy(),
            format!("my.partition.part{}.rar", index + 1)
        );
    }

    // Discovery from any volume, including the last, yields the full set.
    for volume in &volumes {
        assert_eq!(rar_rs::discover_volumes(volume), volumes);
    }

    // The set is readable through a non-first volume.
    let mut reader = ArchiveReader::open(volumes.last().unwrap()).unwrap();
    let id = reader.unique_entry("f1.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), data);
}
