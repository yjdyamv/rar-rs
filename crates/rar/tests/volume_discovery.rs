//! Volume-set discovery regressions: case folding, non-UTF-8 bases and a
//! base that itself contains `.part`.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

mod volume_case_discovery {
    //! Regression test: a volume set spelled in upper case must be discovered on
    //! case-sensitive filesystems.
    //!
    //! `discover_volumes` probed lower-case names with `exists()`, so a DOS-era
    //! style set (`MULTIVOL.RAR` + `MULTIVOL.R00`, as in the rars fixtures)
    //! looked like a single truncated volume on Linux; Windows folded the case
    //! for free and the bug stayed invisible.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use std::path::Path;

    use rar_rs::{
        ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions,
    };

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
}

mod volume_non_utf8_discovery {
    //! Regression test: a multi-volume set whose base is not valid UTF-8 must be
    //! discovered on Unix.
    //!
    //! `discover_volumes` bailed to the single opened path when `file_name()`
    //! was not valid UTF-8 (`to_str()` returned `None`), so a set renamed to raw
    //! bytes (`caf\xE9.part1.rar`, …) could not be enumerated. The probes and
    //! sibling index now carry raw `OsStr` bytes on Unix.

    #![cfg(unix)]

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use rar_rs::{
        ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions,
    };

    fn stored() -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i.wrapping_mul(31) ^ (i >> 5)) as u8)
            .collect()
    }

    #[test]
    fn non_utf8_volume_base_is_discovered_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("set.part1.rar");
        let data = payload(60_000);
        {
            let mut writer =
                ArchiveWriter::create_with(&first, WriterOptions::default().volume_size(30_000))
                    .unwrap();
            writer.add_bytes("f.bin", &data, stored()).unwrap();
            writer.finish().unwrap();
        }

        // Rename every volume to `caf\xE9.partN.rar`: the base carries a byte
        // that is not valid UTF-8. Entries are collected before any rename so
        // the directory scan cannot pick up the renamed targets.
        let entries: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        let mut renamed: Vec<PathBuf> = Vec::new();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let Some(suffix) = name.strip_prefix("set") else {
                continue;
            };
            let mut raw = b"caf\xE9".to_vec();
            raw.extend_from_slice(suffix.as_bytes());
            let target = dir.path().join(OsString::from_vec(raw));
            std::fs::rename(&path, &target).unwrap();
            renamed.push(target);
        }
        renamed.sort();
        assert!(
            renamed.len() > 1,
            "the set must split into multiple volumes: {renamed:?}"
        );

        // Discovery from the first volume (and from a later one) yields the
        // full set with the original raw bytes.
        let discovered = rar_rs::discover_volumes(&renamed[0]);
        assert_eq!(discovered, renamed, "discovery must find every volume");
        let from_last = rar_rs::discover_volumes(renamed.last().unwrap());
        assert_eq!(from_last, renamed, "discovery from any volume");

        let mut reader = ArchiveReader::open(&discovered[0]).unwrap();
        let id = reader.unique_entry("f.bin").unwrap();
        assert_eq!(reader.read_entry(id).unwrap(), data);
    }
}

mod volume_part_base {
    //! Regression test: a volume base that itself contains `.part` (like
    //! `my.partition`) must split at the LAST `.part` segment.
    //!
    //! `extract_volume_base` used the FIRST `.part` occurrence, so
    //! `my.partition.part2.rar` found `.part` inside `partition`, the trailing
    //! `ition.part2` failed the digit check and the name never parsed as a part
    //! volume. Discovery then fell through to the legacy `.rar` strip and
    //! creation staged its base as `my.partition.part1`, leaving nested
    //! `my.partition.part1.part1.rar` names that could not be read back.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions,
    };

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
}
