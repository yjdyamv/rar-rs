use std::fs::OpenOptions as FsOpenOptions;
use std::io::{Seek, SeekFrom, Write};

use rar_rs::{
    ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions, ErrorCode,
    ExtractErrorPolicy, ExtractOptions, OpenOptions, RarError, ScanStrategy, SolidMode,
    WriterOptions,
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
    let opts = EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap());
    let mut archive = ArchiveWriter::create_with(path, WriterOptions::default().quick_open(true))
        .expect("create archive");
    archive
        .add_bytes("same.bin", b"first payload", opts)
        .expect("add first duplicate");
    archive
        .add_bytes("same.bin", b"second payload", opts)
        .expect("add second duplicate");
    archive.finish().expect("close archive");
}

#[test]
fn duplicate_entries_are_addressable_by_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("duplicates.rar");
    create_duplicate_archive(&path);

    let mut legacy = ArchiveReader::open(&path).expect("legacy open");
    let first = legacy
        .entries_named("same.bin")
        .next()
        .expect("legacy first duplicate")
        .id();
    assert_eq!(
        legacy.read_entry(first).expect("legacy read"),
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

/// `-or` numbers collisions `name(N).ext`; the suffix must not nest across
/// successive collisions (`a(1)(2).txt` is wrong, `a(2).txt` is right).
#[test]
fn auto_rename_numbers_without_nesting_the_suffix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dups.rar");
    let opts = EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap());
    {
        let mut archive = ArchiveWriter::create(&path).expect("create archive");
        for byte in *b"abc" {
            archive
                .add_bytes("same.bin", &[byte; 16], opts)
                .expect("add duplicate");
        }
        archive.finish().expect("close archive");
    }

    let output = dir.path().join("output");
    let mut reader = ArchiveReader::open(&path).expect("open");
    let report = reader
        .extract_all_with_options(
            &output,
            ExtractOptions {
                auto_rename: true,
                ..Default::default()
            },
        )
        .expect("extract");
    let names: Vec<String> = report
        .written()
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["same.bin", "same(1).bin", "same(2).bin"],
        "collision suffixes must be numbered, not nested"
    );
    assert_eq!(
        std::fs::read(output.join("same(2).bin")).unwrap(),
        [b'c'; 16]
    );
}

fn assert_solid_reader_recovers_after_writer_failure(version: ArchiveVersion, file_name: &str) {
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
    let mut archive = ArchiveWriter::create_with(
        &path,
        WriterOptions::default()
            .compression(version)
            .solid_mode(SolidMode::Continuous),
    )
    .expect("create solid archive");
    let opts = EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap());
    for (name, payload) in ["a.txt", "b.txt", "c.txt"].into_iter().zip(&payloads) {
        let source = source_dir.join(name);
        std::fs::write(&source, payload).expect("write solid source");
        archive.add_path(&source, opts).expect("add solid member");
    }
    archive.finish().expect("close solid archive");

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

    let mut archive = ArchiveReader::open(&path).expect("reopen corrupted archive");
    let report = archive.verify().expect("test archive");
    assert_eq!((report.passed() + report.failed(), report.failed()), (2, 1));
}

/// Extraction reports the writer's own outcome: the selected subset is
/// written, a `-o-` rerun reports every member as skipped, and the listed
/// members are checked against the total unpacked cap like a whole-archive
/// extraction.
#[test]
fn extraction_reports_written_and_skipped_members() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("extraction-report.rar");
    create_duplicate_archive(&path);

    let mut reader = ArchiveReader::open(&path).expect("open reader");
    let ids: Vec<_> = reader
        .entries_named("same.bin")
        .map(|entry| entry.id())
        .collect();
    let output = dir.path().join("out");

    let error = reader
        .extract_ids_with_options(
            &ids,
            &output,
            ExtractOptions {
                max_total_unpacked_bytes: Some(20),
                ..Default::default()
            },
        )
        .expect_err("the selected members must respect the total cap");
    assert!(matches!(error, RarError::LimitExceeded { limit: 20, .. }));

    let report = reader
        .extract_ids_with_options(&ids[..1], &output, ExtractOptions::default())
        .expect("extract the selected member");
    assert_eq!(report.written_count(), 1);
    assert_eq!(report.skipped_count(), 0);
    assert_eq!(report.written(), [output.join("same.bin")]);
    assert_eq!(
        std::fs::read(output.join("same.bin")).expect("read output"),
        b"first payload"
    );

    let report = reader
        .extract_all_with_options(
            &output,
            ExtractOptions {
                skip_existing: true,
                ..Default::default()
            },
        )
        .expect("re-extract with skip-existing");
    assert_eq!(report.written_count(), 0);
    assert_eq!(report.skipped_count(), 2);
    assert!(report.written().is_empty());
    assert_eq!(
        report.skipped(),
        [output.join("same.bin"), output.join("same.bin")],
        "skipped members carry their resolved destination in archive order"
    );
    assert_eq!(
        std::fs::read(output.join("same.bin")).expect("kept output"),
        b"first payload"
    );
}

/// Three named members, for the progress tests that need a catalog whose
/// per-member sizes differ from its total.
fn create_numbered_archive(path: &std::path::Path) {
    let opts = EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap());
    let mut archive = ArchiveWriter::create(path).expect("create archive");
    for (index, size) in [400usize, 700, 900].into_iter().enumerate() {
        archive
            .add_bytes(&format!("f{index}.bin"), &vec![b'x'; size], opts)
            .expect("add member");
    }
    archive.finish().expect("close archive");
}

/// Every `(committed, total)` a counting sink was handed.
type ProgressLog = std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>>;

/// A sink that records every `(committed, total)` it is handed.
fn counting_sink() -> (rar_rs::ExtractionProgress, ProgressLog) {
    let seen: ProgressLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let sink: rar_rs::ExtractionProgress = std::sync::Arc::new(std::sync::Mutex::new(Some(
        Box::new(move |committed, total| {
            recorder.lock().expect("recorder").push((committed, total));
        }),
    )));
    (sink, seen)
}

/// The sink is borrowed, not consumed: `ExtractOptions` is `Clone` and its
/// documentation promises that a clone reports to the same callback, so a
/// second run with the same (or a cloned) options value must still report.
/// Taking the closure out of the shared sink made every later run silent.
#[test]
fn extraction_progress_survives_reuse_of_the_same_options_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("progress-reuse.rar");
    create_numbered_archive(&path);

    let (sink, seen) = counting_sink();
    let options = ExtractOptions {
        on_progress: Some(sink),
        ..Default::default()
    };

    let mut reader = ArchiveReader::open(&path).expect("open reader");
    reader
        .extract_all_with_options(dir.path().join("first"), options.clone())
        .expect("first extraction");
    let after_first = seen.lock().expect("recorder").len();
    assert!(after_first > 0, "the first run must report progress");

    let mut reader = ArchiveReader::open(&path).expect("reopen reader");
    reader
        .extract_all_with_options(dir.path().join("second"), options.clone())
        .expect("second extraction");
    let after_second = seen.lock().expect("recorder").len();
    assert!(
        after_second > after_first,
        "reusing the options value must keep reporting: the run consumed the sink"
    );

    // Each run counts from its own zero rather than continuing the first
    // run's committed total, so a consumer sees a full 0..100% sweep twice.
    let seen = seen.lock().expect("recorder").clone();
    let (second_run, first_run) = seen.split_at(after_first);
    assert_eq!(first_run.last().copied(), Some((2000, 2000)));
    assert_eq!(second_run.last().copied(), Some((2000, 2000)));
}

/// A filtered run reports against the **selected** members, not the whole
/// catalog, and reports at all — the id-selected path used to install no
/// tracker, so a filtered extraction (every CLI `x`/`e` run with a selector)
/// was silently mute.
#[test]
fn id_selected_extraction_reports_progress_over_the_selected_members() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("progress-subset.rar");
    create_numbered_archive(&path);

    let mut reader = ArchiveReader::open(&path).expect("open reader");
    let id = reader.unique_entry("f1.bin").expect("middle member");
    let selected_size = reader.entry(id).expect("metadata").size();

    let (sink, seen) = counting_sink();
    let options = ExtractOptions {
        on_progress: Some(sink),
        ..Default::default()
    };
    reader
        .extract_ids_with_options(&[id], dir.path().join("out"), options)
        .expect("extract the selected member");

    let seen = seen.lock().expect("recorder").clone();
    assert_eq!(
        seen,
        vec![(selected_size, selected_size)],
        "the reported total is the selected member, not the whole catalog"
    );
}

/// `Collect` means the same thing on both extraction paths: a failing member
/// is recorded and the run continues. The id-selected path used to propagate
/// the first error regardless of the policy, so `Collect` silently behaved
/// like `Abort` whenever a selector was in play.
#[test]
fn collect_policy_collects_failures_on_the_id_selected_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("collect-subset.rar");
    create_duplicate_archive(&path);

    let reader = ArchiveReader::open(&path).expect("open reader");
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

    let output = dir.path().join("out");

    // A quick-open archive rotates its catalog token the moment the catalog
    // is rebuilt (the aborted run below does exactly that), so every phase
    // collects its ids from its own reader and never reuses an earlier set.
    let mut reader = ArchiveReader::open(&path).expect("reopen reader");
    let ids: Vec<_> = reader
        .entries_named("same.bin")
        .map(|entry| entry.id())
        .collect();
    let error = reader
        .extract_ids_with_options(&ids, &output, ExtractOptions::default())
        .expect_err("Abort stops at the first failing member");
    assert_eq!(error.code(), ErrorCode::CrcMismatch);

    let mut reader = ArchiveReader::open(&path).expect("reopen reader");
    let ids: Vec<_> = reader
        .entries_named("same.bin")
        .map(|entry| entry.id())
        .collect();
    let report = reader
        .extract_ids_with_options(
            &ids,
            &output,
            ExtractOptions {
                error_policy: ExtractErrorPolicy::Collect,
                ..Default::default()
            },
        )
        .expect("Collect finishes the run");
    assert_eq!(report.failed_count(), 1, "the bad member is recorded");
    assert_eq!(report.failures()[0].name(), "same.bin");
    assert_eq!(report.failures()[0].error().code(), ErrorCode::CrcMismatch);
    assert_eq!(
        report.failures()[0].index(),
        1,
        "the failure carries the member's archive-order catalog index"
    );
    assert_eq!(report.written_count(), 1, "the intact member still landed");
}
