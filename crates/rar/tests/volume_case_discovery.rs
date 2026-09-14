//! Regression test: a volume set spelled in upper case must be discovered on
//! case-sensitive filesystems.
//!
//! `discover_volumes` probed lower-case names with `exists()`, so a DOS-era
//! style set (`MULTIVOL.RAR` + `MULTIVOL.R00`, as in the rars fixtures)
//! looked like a single truncated volume on Linux; Windows folded the case
//! for free and the bug stayed invisible.

use std::path::Path;

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(31) ^ (i >> 5)) as u8)
        .collect()
}

/// Rename every file in `dir` to its ASCII upper-case spelling.
fn upper_case_all(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let upper = name.to_ascii_uppercase();
        if name != upper {
            std::fs::rename(entry.path(), dir.join(upper)).unwrap();
        }
    }
}

#[test]
fn upper_case_legacy_volume_sets_are_discovered() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("legacy.rar");
    let data = payload(60_000);
    {
        let mut writer = ArchiveWriter::create_with(
            &first,
            WriterOptions::default()
                .compression(rar_rs::ArchiveVersion::V29)
                .volume_size(30_000),
        )
        .unwrap();
        writer.add_bytes("f.bin", &data, stored()).unwrap();
        writer.finish().unwrap();
    }
    upper_case_all(dir.path());

    let upper_first = dir.path().join("LEGACY.RAR");
    let volumes = rar_rs::discover_volumes(&upper_first);
    assert!(volumes.len() > 1, "got {volumes:?}");
    assert!(volumes.iter().all(|volume| volume.exists()), "{volumes:?}");
    // On a case-insensitive filesystem the probe path itself exists and may
    // be returned with the probe's spelling; only a case-sensitive one has
    // to surface the real on-disk (upper-case) names.
    #[cfg(not(windows))]
    assert!(
        volumes.iter().all(|volume| volume
            .file_name()
            .unwrap()
            .to_string_lossy()
            .chars()
            .all(|c| !c.is_ascii_lowercase())),
        "the real on-disk spelling must be returned: {volumes:?}"
    );

    let mut reader = ArchiveReader::open(&volumes[0]).unwrap();
    let id = reader.unique_entry("f.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), data);
}

#[test]
fn upper_case_part_volume_sets_are_discovered() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("case.part1.rar");
    let data = payload(60_000);
    {
        let mut writer =
            ArchiveWriter::create_with(&first, WriterOptions::default().volume_size(30_000))
                .unwrap();
        writer.add_bytes("f.bin", &data, stored()).unwrap();
        writer.finish().unwrap();
    }
    upper_case_all(dir.path());

    let upper_first = dir.path().join("CASE.PART1.RAR");
    let volumes = rar_rs::discover_volumes(&upper_first);
    assert!(volumes.len() > 1, "got {volumes:?}");
    assert!(volumes.iter().all(|volume| volume.exists()), "{volumes:?}");

    let mut reader = ArchiveReader::open(&volumes[0]).unwrap();
    let id = reader.unique_entry("f.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), data);
}
