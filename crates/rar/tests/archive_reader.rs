#![allow(deprecated)] // legacy facade (list/read via RarArchive) — mirrors the typed reader

use std::fs::OpenOptions as FsOpenOptions;
use std::io::{Seek, SeekFrom, Write};

use rar_rs::{
    ArchiveReader, ArchiveVersion, CreateOptions, ErrorCode, ExtractOptions, OpenOptions,
    RarArchive, RarError, ScanStrategy,
};

struct FailAfter {
    remaining: usize,
}

impl Write for FailAfter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(std::io::Error::other("injected writer failure"));
        }
        let written = buf.len().min(self.remaining);
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn create_duplicate_archive(path: &std::path::Path) {
    let mut archive = RarArchive::create_with_options(
        path,
        CreateOptions {
            quick_open: true,
            ..Default::default()
        },
    )
    .expect("create archive");
    archive
        .add_bytes("same.bin", b"first payload", 0)
        .expect("add first duplicate");
    archive
        .add_bytes("same.bin", b"second payload", 0)
        .expect("add second duplicate");
    archive.close().expect("close archive");
}

#[test]
fn duplicate_entries_are_addressable_by_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("duplicates.rar");
    create_duplicate_archive(&path);

    let mut legacy = RarArchive::open(&path).expect("legacy open");
    assert_eq!(
        legacy.read("same.bin").expect("legacy read"),
        b"first payload"
    );

    let mut reader = ArchiveReader::open(&path).expect("typed open");
    let ids: Vec<_> = reader
        .entries_named("same.bin")
        .map(|entry| entry.id())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        reader.entry(ids[0]).expect("first metadata").name(),
        "same.bin"
    );
    assert_eq!(
        reader.entry(ids[1]).expect("second metadata").name(),
        "same.bin"
    );

    assert_eq!(
        reader.read_entry(ids[0]).expect("read first"),
        b"first payload"
    );
    assert_eq!(
        reader.read_entry(ids[1]).expect("read second"),
        b"second payload"
    );

    let mut first_copy = Vec::new();
    let mut second_copy = Vec::new();
    reader
        .copy_entry_to(ids[0], &mut first_copy)
        .expect("copy first");
    reader
        .copy_entry_to_with_options(ids[1], &mut second_copy, ExtractOptions::default())
        .expect("copy second");
    assert_eq!(first_copy, b"first payload");
    assert_eq!(second_copy, b"second payload");

    let output = dir.path().join("output");
    let first_path = reader
        .extract_entry(ids[0], &output)
        .expect("extract first");
    let second_path = reader
        .extract_entry_with_options(
            ids[1],
            &output,
            ExtractOptions {
                auto_rename: true,
                ..Default::default()
            },
        )
        .expect("extract second");
    assert_ne!(first_path, second_path);
    assert_eq!(
        std::fs::read(first_path).expect("read first output"),
        b"first payload"
    );
    assert_eq!(
        std::fs::read(second_path).expect("read second output"),
        b"second payload"
    );
}

fn assert_solid_reader_recovers_after_writer_failure(format: ArchiveVersion, file_name: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(file_name);
    let common = b"shared solid dictionary content with enough repetition\n".repeat(4_096);
    let payloads = [
        [common.as_slice(), b"first member\n"].concat(),
        [common.as_slice(), b"second member\n"].concat(),
        [common.as_slice(), b"third member\n"].concat(),
    ];

    let source_dir = dir.path().join("src");
    std::fs::create_dir(&source_dir).expect("create source directory");
    let mut archive = RarArchive::create_with_options(
        &path,
        CreateOptions {
            format_version: format,
            solid: true,
            ..Default::default()
        },
    )
    .expect("create solid archive");
    for (name, payload) in ["a.txt", "b.txt", "c.txt"].into_iter().zip(&payloads) {
        let source = source_dir.join(name);
        std::fs::write(&source, payload).expect("write solid source");
        archive.add(&source, 3).expect("add solid member");
    }
    archive.close().expect("close solid archive");

    let mut reader =
        ArchiveReader::open(&path).unwrap_or_else(|err| panic!("open {file_name}: {err}"));
    let entries: Vec<_> = reader.entries().collect();
    assert_eq!(entries.len(), 3, "{file_name}");
    assert!(entries[1].metadata().comp_solid(), "{file_name}");
    assert_ne!(entries[1].metadata().method(), 0, "{file_name}");
    let ids: Vec<_> = entries.into_iter().map(|entry| entry.id()).collect();

    let mut failing = FailAfter { remaining: 64 };
    let error = reader
        .copy_entry_to(ids[1], &mut failing)
        .expect_err("injected writer must fail");
    assert!(
        error.to_string().contains("injected writer failure"),
        "{file_name}: {error}"
    );

    assert_eq!(
        reader
            .read_entry(ids[2])
            .expect("later solid member must restart and decode"),
        payloads[2],
        "{file_name}"
    );
}

#[test]
fn rar5_solid_reader_recovers_after_writer_failure() {
    assert_solid_reader_recovers_after_writer_failure(ArchiveVersion::V50, "solid-rar5.rar");
}

#[test]
fn rar4_solid_reader_recovers_after_writer_failure() {
    assert_solid_reader_recovers_after_writer_failure(ArchiveVersion::V29, "solid-rar4.rar");
}

#[test]
fn entry_ids_are_reader_scoped_and_unique_lookup_detects_duplicates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reader-options.rar");
    create_duplicate_archive(&path);

    let full_options = OpenOptions::new()
        .password("unused")
        .scan_strategy(ScanStrategy::Full);
    let full_reader = ArchiveReader::open_with(&path, full_options).expect("full password open");
    let foreign_id = full_reader.entries().next().expect("entry").id();

    let quick_options = OpenOptions::new()
        .scan_strategy(ScanStrategy::PreferQuickOpen)
        .password("unused");
    let quick_reader = ArchiveReader::open_with(&path, quick_options).expect("quick password open");

    assert!(matches!(
        quick_reader.entry(foreign_id),
        Err(RarError::StaleEntryId)
    ));
    assert!(matches!(
        quick_reader.unique_entry("same.bin"),
        Err(RarError::AmbiguousMember {
            name,
            matches: 2
        }) if name == "same.bin"
    ));

    let entry = {
        let query = String::from("same.bin");
        quick_reader.entries_named(&query).next().expect("entry")
    };
    assert_eq!(entry.name(), "same.bin");
}

#[test]
fn verification_enforces_the_total_unpacked_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("verification-limit.rar");
    create_duplicate_archive(&path);

    let mut reader = ArchiveReader::open(&path).expect("open reader");
    let error = reader
        .verify_with_options(ExtractOptions {
            max_unpacked_bytes: Some(20),
            max_total_unpacked_bytes: Some(20),
            ..Default::default()
        })
        .expect_err("aggregate limit must reject the archive");
    assert!(matches!(error, RarError::LimitExceeded { limit: 20, .. }));
}

#[test]
fn legacy_test_checks_each_duplicate_entry_by_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("duplicate-corruption.rar");
    create_duplicate_archive(&path);

    let reader = ArchiveReader::open(&path).expect("open for offsets");
    let ids: Vec<_> = reader
        .entries_named("same.bin")
        .map(|entry| entry.id())
        .collect();
    let second_offset = reader.entry(ids[1]).expect("second entry").data_offset();
    drop(reader);

    let mut file = FsOpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open archive for corruption");
    file.seek(SeekFrom::Start(second_offset))
        .expect("seek second payload");
    file.write_all(&[0xff]).expect("corrupt second payload");
    file.flush().expect("flush corruption");

    let mut reader = ArchiveReader::open(&path).expect("reopen typed reader");
    let report_ids: Vec<_> = reader
        .entries_named("same.bin")
        .map(|entry| entry.id())
        .collect();
    let report = reader.verify().expect("verify archive");
    assert_eq!(report.checked(), 2);
    assert_eq!(report.passed(), 1);
    assert_eq!(report.failed(), 1);
    assert!(!report.is_ok());
    assert_eq!(report.failures()[0].entry_id(), report_ids[1]);
    assert_eq!(report.failures()[0].error().code(), ErrorCode::CrcMismatch);

    let mut archive = RarArchive::open(&path).expect("reopen corrupted archive");
    assert_eq!(archive.test().expect("test archive"), (2, 1));
}
