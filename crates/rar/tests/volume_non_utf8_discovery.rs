//! Regression test: a multi-volume set whose base is not valid UTF-8 must be
//! discovered on Unix.
//!
//! `discover_volumes` bailed to the single opened path when `file_name()`
//! was not valid UTF-8 (`to_str()` returned `None`), so a set renamed to raw
//! bytes (`caf\xE9.part1.rar`, …) could not be enumerated. The probes and
//! sibling index now carry raw `OsStr` bytes on Unix.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

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
