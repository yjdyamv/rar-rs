//! RAR4 creation (legacy `Rar!\x1a\x07\x00` container): STORE members,
//! single-volume and multi-volume, verified by reading back through the
//! RAR4 reader.

#![allow(deprecated)] // legacy constructor family; use create_with_options

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar5::{ArchiveVersion, CreateOptions, RarArchive, discover_volumes};

fn crate_crc(data: &[u8]) -> u32 {
    let mut c = 0xFFFFFFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            let mask = -(c as i32 & 1) as u32;
            c = (c >> 1) ^ (0xEDB88320 & mask);
        }
    }
    !c
}

#[test]
fn create_rar4_store_single_roundtrip() {
    let dir = make_temp_dir();
    let content = b"Hello, RAR4 world! This is a STORE roundtrip test.\n";
    let src = dir.path().join("hello.txt");
    std::fs::write(&src, content).unwrap();
    let arc = dir.path().join("out.rar");

    let opts = CreateOptions {
        format_version: ArchiveVersion::Rar40,
        ..Default::default()
    };
    let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
    archive.add(&src, 0).expect("add STORE member");
    archive.close().expect("close");

    // Read back through the RAR4 reader.
    let mut archive = RarArchive::open(&arc).expect("reopen");
    let entries = archive.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), "hello.txt");
    assert_eq!(entries[0].size(), content.len() as u64);
    assert_eq!(entries[0].crc32(), Some(crate_crc(content)));

    let out = archive.read("hello.txt").expect("read back");
    assert_eq!(&out, content);
}

#[test]
fn create_rar4_store_multiple_files() {
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"content A\n").unwrap();
    std::fs::write(&b, b"content B is longer\n").unwrap();
    let arc = dir.path().join("multi.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            format_version: ArchiveVersion::Rar40,
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&a, 0).expect("add a");
    archive.add(&b, 0).expect("add b");
    archive.close().expect("close");

    let mut archive = RarArchive::open(&arc).expect("reopen");
    let entries = archive.list();
    assert_eq!(entries.len(), 2);
    let mut names: Vec<&str> = entries.iter().map(|e| e.name()).collect();
    names.sort();
    assert_eq!(names, vec!["a.txt", "b.txt"]);
    assert_eq!(&archive.read("a.txt").unwrap(), b"content A\n");
    assert_eq!(&archive.read("b.txt").unwrap(), b"content B is longer\n");
}

#[test]
fn create_rar4_empty_file() {
    let dir = make_temp_dir();
    let arc = dir.path().join("empty.rar");
    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            format_version: ArchiveVersion::Rar40,
            ..Default::default()
        },
    )
    .expect("create");
    // No members.
    archive.close().expect("close");

    let archive = RarArchive::open(&arc).expect("reopen");
    assert_eq!(archive.list().len(), 0);
}

#[test]
fn create_rar4_store_multivolume_split_member() {
    let dir = make_temp_dir();
    // A file big enough to span at least one volume boundary given the
    // small volume size.
    let content: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let src = dir.path().join("big.bin");
    std::fs::write(&src, &content).unwrap();
    let arc = dir.path().join("vol.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            format_version: ArchiveVersion::Rar40,
            volume_size: Some(5_000),
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 0).expect("add");
    archive.close().expect("close");

    // Discover the volume set and read it back.
    let volumes = discover_volumes(&arc);
    assert!(
        volumes.len() >= 2,
        "expected multiple volumes, got {volumes:?}"
    );

    let mut archive = RarArchive::open(&arc).expect("reopen");
    let entries = archive.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), "big.bin");
    assert_eq!(entries[0].size(), content.len() as u64);

    let out = archive.read("big.bin").expect("read split member");
    assert_eq!(&out, &content);
}
