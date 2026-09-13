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

/// Reorder the two cached entries of a quick-open record without touching
/// their bodies (each body keeps its own header CRC and relative offset, so
/// the resulting record is structurally valid but lists members in the
/// opposite order from the archive scan).
fn swap_quick_open_entries(archive: &[u8]) -> Vec<u8> {
    use std::io::Read;

    let mut cursor = std::io::Cursor::new(archive);
    cursor.set_position(8);
    loop {
        let meta = rar_rs::wire::read_block(&mut cursor, None)
            .expect("read block")
            .expect("block");
        if meta.block_type == 0x03 {
            assert!(meta.raw.data_size > 0, "empty quick-open payload");
            cursor.set_position(meta.data_offset);
            let mut payload = vec![0u8; meta.raw.data_size as usize];
            cursor.read_exact(&mut payload).expect("read qo payload");
            let mut entries: Vec<Vec<u8>> = Vec::new();
            let mut off = 0usize;
            while off < payload.len() {
                let start = off;
                off += 4; // entry CRC32
                let (body_size, n) =
                    rar_rs::wire::vint::decode_from_slice(&payload, off).expect("body size vint");
                off += n + body_size as usize;
                assert!(off <= payload.len(), "truncated quick-open body");
                entries.push(payload[start..off].to_vec());
            }
            assert_eq!(entries.len(), 2, "test archive must cache two entries");
            entries.reverse();
            let swapped: Vec<u8> = entries.concat();
            assert_eq!(swapped.len(), payload.len());
            let mut out = archive.to_vec();
            out.splice(
                meta.data_offset as usize..meta.data_offset as usize + payload.len(),
                swapped,
            );
            return out;
        }
        cursor.set_position(meta.data_end);
        if meta.block_type == 0x05 {
            panic!("archive has no quick-open record");
        }
    }
}

/// A quick-open catalog may order members differently from the full scan
/// that extraction runs to discover "STM" records. An ID obtained from the
/// cached listing must then resolve to the same member (or come back as
/// [`rar_rs::RarError::StaleEntryId`]); it must never extract a different
/// member that happens to sit at the same index after the rescan.
#[test]
fn quick_open_rescan_never_extracts_a_different_member() {
    let dir = temp_dir();
    let path = dir.path().join("qo-swapped.rar");
    let aa = b"aa member payload".repeat(100);
    let bb = b"bb member payload".repeat(100);
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().quick_open(true),
        )
        .expect("create");
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("aa.txt", &aa, opts).expect("add aa");
        rar.add_bytes("bb.txt", &bb, opts).expect("add bb");
        rar.finish().expect("close");
    }

    let bytes = fs::read(&path).expect("read archive");
    let swapped_path = dir.path().join("qo-swapped-patched.rar");
    fs::write(&swapped_path, swap_quick_open_entries(&bytes)).expect("write patched archive");

    let mut reader = ArchiveReader::open_with(
        &swapped_path,
        rar_rs::OpenOptions::new().scan_strategy(rar_rs::ScanStrategy::PreferQuickOpen),
    )
    .expect("open quick");
    let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(
        names,
        vec!["bb.txt".to_string(), "aa.txt".to_string()],
        "the cached listing must be reversed"
    );

    let bb_id = reader.unique_entry("bb.txt").expect("bb id");
    let out = dir.path().join("out");
    match reader.extract_entry(bb_id, &out) {
        Ok(extracted) => {
            assert_eq!(
                extracted.file_name().and_then(|n| n.to_str()),
                Some("bb.txt"),
                "the ID must resolve to the member it names"
            );
            assert_eq!(fs::read(&extracted).expect("read extracted bb"), bb);
            assert!(
                !out.join("aa.txt").exists(),
                "a bb request must not extract aa"
            );
        }
        Err(rar_rs::RarError::StaleEntryId) => {}
        Err(other) => panic!("unexpected extraction error: {other}"),
    }
}

/// Directory (and redirect) headers are members too: the quick-open record
/// must cache them or `open_quick` lists fewer members than the full scan.
#[test]
fn open_quick_lists_directories_like_the_full_scan() {
    let dir = temp_dir();
    let path = dir.path().join("qo-dir.rar");
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).expect("mkdir");
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().quick_open(true),
        )
        .expect("create");
        rar.add_directory(&sub, "sub").expect("add dir");
        rar.add_bytes("sub/a.bin", b"data", rar_rs::EntryWriteOptions::new())
            .expect("add file");
        rar.finish().expect("close");
    }

    let list = |reader: &ArchiveReader| -> Vec<(String, bool)> {
        reader
            .entries()
            .map(|entry| (entry.name().to_string(), entry.is_dir()))
            .collect()
    };
    let full = ArchiveReader::open(&path).expect("open");
    let quick = ArchiveReader::open_with(
        &path,
        rar_rs::OpenOptions::new().scan_strategy(rar_rs::ScanStrategy::PreferQuickOpen),
    )
    .expect("open_quick");

    let full_list = list(&full);
    assert!(
        full_list.iter().any(|(_, is_dir)| *is_dir),
        "full scan sees the directory"
    );
    assert_eq!(
        list(&quick),
        full_list,
        "QO listing must match the full scan"
    );
}

/// The quick-open path also records the archive-level solid flag.
#[test]
fn open_quick_reports_solid_archives() {
    let dir = temp_dir();
    let path = dir.path().join("qo-solid.rar");
    {
        let data = b"solid ".repeat(2000);
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .quick_open(true)
                .solid_mode(rar_rs::SolidMode::Continuous),
        )
        .unwrap();
        rar.add_bytes(
            "a.txt",
            &data,
            rar_rs::EntryWriteOptions::new()
                .compression_level(rar_rs::CompressionLevel::try_from(5).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }

    let quick = ArchiveReader::open_with(
        &path,
        rar_rs::OpenOptions::new().scan_strategy(rar_rs::ScanStrategy::PreferQuickOpen),
    )
    .unwrap();
    assert!(
        quick.is_solid(),
        "quick-open must report the archive-level solid flag"
    );
}
