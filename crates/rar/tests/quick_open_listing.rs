//! Quick-open fast path: `open_quick` must list and extract identically
//! to the full-scan opener, and transparently fall back when the archive
//! has no quick-open record.

use std::fs;

use rar_rs::ArchiveReader;

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn payloads() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("a.bin", (0..300_000u32).map(|i| (i % 251) as u8).collect()),
        ("b.txt", b"hello quick-open".repeat(500)),
        ("c.bin", vec![0xAB; 10_000]),
    ]
}

#[test]
fn open_quick_lists_identically_to_full_scan() {
    let dir = temp_dir();
    let path = dir.path().join("qo.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().quick_open(true),
        )
        .expect("create");
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        for (name, data) in payloads() {
            rar.add_bytes(name, &data, opts).expect("add");
        }
        rar.finish().expect("close");
    }

    let full = ArchiveReader::open(&path).expect("open");
    let quick = ArchiveReader::open_with(
        &path,
        rar_rs::OpenOptions::new().scan_strategy(rar_rs::ScanStrategy::PreferQuickOpen),
    )
    .expect("open_quick");

    let full_list: Vec<_> = full
        .entries()
        .map(|e| (e.name().to_string(), e.size(), e.method(), e.is_dir()))
        .collect();
    let quick_list: Vec<_> = quick
        .entries()
        .map(|e| (e.name().to_string(), e.size(), e.method(), e.is_dir()))
        .collect();
    assert_eq!(quick_list, full_list, "QO listing must match the full scan");
    assert!(!quick_list.is_empty());
}

#[test]
fn open_quick_reads_and_extracts_members() {
    let dir = temp_dir();
    let path = dir.path().join("qo.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().quick_open(true),
        )
        .expect("create");
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        for (name, data) in payloads() {
            rar.add_bytes(name, &data, opts).expect("add");
        }
        rar.finish().expect("close");
    }

    let mut quick = ArchiveReader::open_with(
        &path,
        rar_rs::OpenOptions::new().scan_strategy(rar_rs::ScanStrategy::PreferQuickOpen),
    )
    .expect("open_quick");
    let source: std::collections::HashMap<_, _> = payloads().into_iter().collect();
    for (name, expect) in &source {
        let got = quick
            .read_entry(quick.unique_entry(name).unwrap())
            .expect("read via QO");
        assert_eq!(&got, expect, "member {name} must read through QO entries");
    }

    let out = dir.path().join("out");
    quick
        .extract_all_with_options(
            &out,
            rar_rs::ExtractOptions {
                safe_paths: true,
                ..Default::default()
            },
        )
        .expect("extract via QO");
    for (name, expect) in &source {
        assert_eq!(
            &fs::read(out.join(name)).expect("extracted file"),
            expect,
            "member {name} must extract through QO entries"
        );
    }
}

#[test]
fn open_quick_falls_back_without_quick_open_record() {
    let dir = temp_dir();
    let path = dir.path().join("plain.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create(&path).expect("create"); // quick_open: false
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        for (name, data) in payloads() {
            rar.add_bytes(name, &data, opts).expect("add");
        }
        rar.finish().expect("close");
    }

    // No QO record -> open_quick falls back to the full scan.
    let mut quick = ArchiveReader::open_with(
        &path,
        rar_rs::OpenOptions::new().scan_strategy(rar_rs::ScanStrategy::PreferQuickOpen),
    )
    .expect("open_quick fallback");
    assert_eq!(quick.entries().count(), payloads().len());
    assert_eq!(
        quick
            .read_entry(quick.unique_entry("a.bin").unwrap())
            .expect("read"),
        payloads()[0].1
    );
}

#[test]
fn open_quick_handles_encrypted_archives() {
    let dir = temp_dir();
    let path = dir.path().join("enc.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .quick_open(true)
                .password("secret"),
        )
        .expect("create");
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        for (name, data) in payloads() {
            rar.add_bytes(name, &data, opts).expect("add");
        }
        rar.finish().expect("close");
    }

    let mut quick = ArchiveReader::open_with(
        &path,
        rar_rs::OpenOptions::new()
            .password("secret")
            .scan_strategy(rar_rs::ScanStrategy::PreferQuickOpen),
    )
    .expect("open_quick encrypted");
    assert_eq!(quick.entries().count(), payloads().len());
    assert_eq!(
        quick
            .read_entry(quick.unique_entry("b.txt").unwrap())
            .expect("read"),
        payloads()[1].1
    );

    // Header-encrypted archives never carry a QO record: the fallback
    // must produce the same listing (password required).
    let path_hp = dir.path().join("hp.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path_hp,
            rar_rs::WriterOptions::default()
                .encrypt_headers(true)
                .password("secret"),
        )
        .expect("create");
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        for (name, data) in payloads() {
            rar.add_bytes(name, &data, opts).expect("add");
        }
        rar.finish().expect("close");
    }
    let mut quick_hp = ArchiveReader::open_with(
        &path_hp,
        rar_rs::OpenOptions::new()
            .password("secret")
            .scan_strategy(rar_rs::ScanStrategy::PreferQuickOpen),
    )
    .expect("open_quick -hp");
    assert_eq!(quick_hp.entries().count(), payloads().len());
    assert_eq!(
        quick_hp
            .read_entry(quick_hp.unique_entry("a.bin").unwrap())
            .expect("read"),
        payloads()[0].1
    );
}
