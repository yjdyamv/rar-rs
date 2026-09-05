use std::path::PathBuf;

use rar5::{
    AppendOptions, ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, DictionarySize,
    EntryWriteOptions, RarArchive, RarError, ThreadCount, WriteEntry, WriterOptions,
};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

#[test]
fn validated_value_types_enforce_boundaries_and_mappings() {
    assert_eq!(CompressionLevel::STORE.get(), 0);
    assert_eq!(CompressionLevel::NORMAL.get(), 3);
    assert_eq!(CompressionLevel::BEST.get(), 5);
    assert_eq!(
        CompressionLevel::try_from(0).unwrap(),
        CompressionLevel::STORE
    );
    assert_eq!(
        CompressionLevel::try_from(5).unwrap(),
        CompressionLevel::BEST
    );
    assert!(CompressionLevel::try_from(6).is_err());

    let min = 128 * 1024u64;
    let rar5_max = 4 * 1024 * 1024 * 1024u64;
    let max = 126 * 1024 * 1024 * 1024u64;
    assert!(DictionarySize::try_from(min - 1).is_err());
    assert_eq!(DictionarySize::try_from(min).unwrap(), DictionarySize::MIN);
    assert_eq!(DictionarySize::MIN.rar5_log(), Some(0));
    assert_eq!(
        DictionarySize::from_rar5_log(8).unwrap().bytes(),
        32 * 1024 * 1024
    );
    assert_eq!(DictionarySize::from_rar5_log(15).unwrap().bytes(), rar5_max);
    assert!(DictionarySize::from_rar5_log(16).is_err());
    assert!(DictionarySize::try_from(3 * 1024 * 1024u64).is_err());
    assert_eq!(
        DictionarySize::try_from(rar5_max).unwrap().rar5_log(),
        Some(15)
    );
    assert_eq!(
        DictionarySize::try_from(rar5_max + 1).unwrap().rar5_log(),
        None
    );
    assert_eq!(DictionarySize::try_from(max).unwrap(), DictionarySize::MAX);
    assert!(DictionarySize::try_from(max + 1).is_err());

    assert_eq!(ThreadCount::AUTOMATIC.get(), 0);
    assert_eq!(ThreadCount::try_from(64).unwrap().get(), 64);
    assert!(ThreadCount::try_from(65).is_err());
}

#[test]
fn writer_options_validate_combinations_before_staging_and_redact_passwords() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.rar");
    let options = WriterOptions::new()
        .password("do-not-print")
        .encrypt_headers(true)
        .recovery_percent(10)
        .volume_size(32 * 1024);
    let debug = format!("{options:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("do-not-print"));
    assert!(matches!(
        ArchiveWriter::create_with(&path, options),
        Err(RarError::InvalidOption(_))
    ));
    assert!(!path.exists());

    let append_debug = format!("{:?}", AppendOptions::new().password("append-secret"));
    assert!(append_debug.contains("<redacted>"));
    assert!(!append_debug.contains("append-secret"));

    // A RAR4 archive cannot take a dictionary at all.
    assert!(matches!(
        ArchiveWriter::create_with(
            &path,
            WriterOptions::new()
                .format_version(ArchiveVersion::Rar40)
                .dictionary_size(DictionarySize::try_from(4 * 1024 * 1024).unwrap())
        ),
        Err(RarError::InvalidOption(_))
    ));
    assert!(!path.exists());
}

#[test]
fn typed_rar50_big_dictionary_keeps_legacy_auto_semantics() {
    // A > 4 GiB dictionary request on RAR50 is the WinRAR auto mode: the
    // member-size cap decides v50 vs v70. For a small member the typed
    // writer must produce the same bytes as the legacy option struct.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("payload.bin");
    let payload: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
    std::fs::write(&source, &payload).unwrap();

    let legacy_path = dir.path().join("legacy.rar");
    let mut legacy = RarArchive::create_with_options(
        &legacy_path,
        rar5::CreateOptions {
            dict_size_bytes: Some(6 * 1024 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    legacy.add(&source, CompressionLevel::NORMAL.get()).unwrap();
    legacy.close().unwrap();

    let typed_path = dir.path().join("typed.rar");
    let mut writer = ArchiveWriter::create_with(
        &typed_path,
        WriterOptions::new()
            .dictionary_size(DictionarySize::try_from(6 * 1024 * 1024 * 1024u64).unwrap()),
    )
    .unwrap();
    writer
        .add_path(
            &source,
            EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL),
        )
        .unwrap();
    writer.finish().unwrap();

    assert_eq!(
        std::fs::read(legacy_path).unwrap(),
        std::fs::read(typed_path).unwrap()
    );
}

#[test]
fn typed_create_matches_equivalent_legacy_output() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("payload.txt");
    std::fs::write(&source, b"deterministic payload\n".repeat(200)).unwrap();
    let legacy_path = dir.path().join("legacy.rar");
    let typed_path = dir.path().join("typed.rar");

    let mut legacy = RarArchive::create_with_options(&legacy_path, Default::default()).unwrap();
    legacy.add(&source, CompressionLevel::NORMAL.get()).unwrap();
    legacy.close().unwrap();

    let mut typed = ArchiveWriter::create(&typed_path).unwrap();
    typed
        .add_path(
            &source,
            EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL),
        )
        .unwrap();
    typed.finish().unwrap();

    assert_eq!(
        std::fs::read(legacy_path).unwrap(),
        std::fs::read(typed_path).unwrap()
    );
}

#[test]
fn typed_create_and_append_abort_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let create_path = dir.path().join("create-abort.rar");
    {
        let mut writer = ArchiveWriter::create(&create_path).unwrap();
        writer.add_bytes("new.txt", b"new", stored()).unwrap();
    }
    assert!(!create_path.exists());

    let append_path = dir.path().join("append-abort.rar");
    let mut legacy = RarArchive::create_with_options(&append_path, Default::default()).unwrap();
    legacy.add_bytes("old.txt", b"old", 0).unwrap();
    legacy.close().unwrap();
    let before = std::fs::read(&append_path).unwrap();
    {
        let mut writer = ArchiveWriter::append(&append_path).unwrap();
        writer.add_bytes("new.txt", b"new", stored()).unwrap();
    }
    assert_eq!(std::fs::read(&append_path).unwrap(), before);
}

#[test]
fn failed_add_poisons_and_aborts_the_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("poisoned.rar");
    let mut writer = ArchiveWriter::create(&path).unwrap();
    writer.add_bytes("first.txt", b"first", stored()).unwrap();

    let error = writer
        .add_path(dir.path().join("missing.txt"), stored())
        .unwrap_err();
    assert!(matches!(error, RarError::Io(_)));
    assert!(matches!(writer.finish(), Err(RarError::InvalidState(_))));
    assert!(!path.exists());
}

#[test]
fn write_report_uses_final_single_and_multivolume_paths() {
    let dir = tempfile::tempdir().unwrap();
    let single = dir.path().join("single.rar");
    let mut writer = ArchiveWriter::create(&single).unwrap();
    writer.add_bytes("one.bin", b"one", stored()).unwrap();
    let report = writer.finish().unwrap();
    assert_eq!(report.primary_path(), single);
    assert_eq!(report.volume_paths(), std::slice::from_ref(&single));
    assert_eq!(report.into_volume_paths(), vec![single]);

    let multi = dir.path().join("multi.rar");
    let payload: Vec<u8> = (0..9 * 32 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();
    let mut writer =
        ArchiveWriter::create_with(&multi, WriterOptions::new().volume_size(32 * 1024)).unwrap();
    writer.add_bytes("many.bin", &payload, stored()).unwrap();
    let report = writer.finish().unwrap();
    assert_eq!(report.volume_paths().len(), 10);
    let expected: Vec<PathBuf> = (1..=10)
        .map(|part| dir.path().join(format!("multi.part{part:02}.rar")))
        .collect();
    assert_eq!(report.volume_paths(), expected);
    assert_eq!(report.primary_path(), expected[0]);
    assert!(expected.iter().all(|path| path.exists()));
}

#[test]
fn exact_recovery_volume_generation_is_disarmed_after_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.rar");
    let payload = vec![7u8; 96 * 1024];
    let mut archive = RarArchive::create_with_options(
        &path,
        rar5::CreateOptions {
            volume_size: Some(32 * 1024),
            recovery_volume_count: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    archive.add_bytes("payload.bin", &payload, 0).unwrap();
    archive.close().unwrap();

    let recovery_path = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "rev"))
        .expect("recovery volume");
    std::fs::remove_file(&recovery_path).unwrap();
    archive.close().unwrap();
    assert!(!recovery_path.exists());
}

#[test]
fn typed_batch_preserves_duplicates_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("batch.rar");
    let source = dir.path().join("source.txt");
    let empty = dir.path().join("empty");
    std::fs::write(&source, b"from disk").unwrap();
    std::fs::create_dir(&empty).unwrap();

    let entries = [
        WriteEntry::Bytes {
            name: "same.txt",
            data: b"first",
            options: stored(),
        },
        WriteEntry::File {
            path: &source,
            name: Some("middle.txt"),
            options: stored(),
        },
        WriteEntry::Bytes {
            name: "same.txt",
            data: b"second",
            options: stored(),
        },
        WriteEntry::Directory {
            path: &empty,
            name: Some("last"),
        },
    ];
    let mut writer = ArchiveWriter::create(&path).unwrap();
    writer.add_batch(&entries).unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    let names: Vec<_> = reader
        .entries()
        .map(|entry| entry.name().to_owned())
        .collect();
    assert_eq!(names, ["same.txt", "middle.txt", "same.txt", "last/"]);
    let duplicate_ids: Vec<_> = reader
        .entries_named("same.txt")
        .map(|entry| entry.id())
        .collect();
    assert_eq!(reader.read_entry(duplicate_ids[0]).unwrap(), b"first");
    assert_eq!(reader.read_entry(duplicate_ids[1]).unwrap(), b"second");
}

#[test]
fn typed_append_roundtrips_and_legacy_drop_still_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("append.rar");
    {
        let mut legacy = RarArchive::create_with_options(&path, Default::default()).unwrap();
        legacy.add_bytes("first.txt", b"first", 0).unwrap();
        // Compatibility behavior: legacy Drop still closes and commits.
    }
    assert!(path.exists());

    let mut writer = ArchiveWriter::append_with(
        &path,
        AppendOptions::new()
            .dictionary_size(DictionarySize::DEFAULT)
            .thread_count(ThreadCount::AUTOMATIC),
    )
    .unwrap();
    writer.add_bytes("second.txt", b"second", stored()).unwrap();
    let report = writer.finish().unwrap();
    assert_eq!(report.volume_paths(), std::slice::from_ref(&path));

    let mut reader = ArchiveReader::open(&path).unwrap();
    assert_eq!(reader.entries().len(), 2);
    let first = reader.unique_entry("first.txt").unwrap();
    let second = reader.unique_entry("second.txt").unwrap();
    assert_eq!(reader.read_entry(first).unwrap(), b"first");
    assert_eq!(reader.read_entry(second).unwrap(), b"second");
}

#[test]
fn typed_options_reject_combos_the_legacy_layer_would_silently_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    // The legacy writer silently skips quick-open for header-encrypted and
    // multi-volume archives, and ignores any dictionary size on RAR4. The
    // typed builder rejects those combinations up front instead of writing
    // an archive that did not honor the requested options.
    for (name, options) in [
        (
            "qo-encrypted.rar",
            WriterOptions::new()
                .quick_open(true)
                .encrypt_headers(true)
                .password("pw"),
        ),
        (
            "qo-volumes.rar",
            WriterOptions::new().quick_open(true).volume_size(32 * 1024),
        ),
        (
            "rar4-dictionary.rar",
            WriterOptions::new()
                .format_version(ArchiveVersion::Rar40)
                .dictionary_size(DictionarySize::DEFAULT),
        ),
    ] {
        let path = dir.path().join(name);
        assert!(
            matches!(
                ArchiveWriter::create_with(&path, options),
                Err(RarError::InvalidOption(_))
            ),
            "{name} must be rejected as an invalid option"
        );
        assert!(!path.exists(), "{name} was staged despite the rejection");
    }

    // The plain RAR4 create (no dictionary override) still works, so the
    // rejection is specific to the silently-ignored option. RAR4 members
    // are added from disk files (the in-memory add_bytes path is RAR5).
    let source = dir.path().join("a.txt");
    std::fs::write(&source, b"a").unwrap();
    let path = dir.path().join("plain-rar4.rar");
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new().format_version(ArchiveVersion::Rar40),
    )
    .unwrap();
    writer.add_path(&source, stored()).unwrap();
    writer.finish().unwrap();
    assert!(path.exists());
}

#[test]
fn append_rejects_rar4_archives_before_touching_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rar4.rar");
    {
        let mut archive = RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                format_version: ArchiveVersion::Rar40,
                ..Default::default()
            },
        )
        .unwrap();
        let source = dir.path().join("a.txt");
        std::fs::write(&source, b"original").unwrap();
        archive.add(&source, 0).unwrap();
        archive.close().unwrap();
    }
    let original = std::fs::read(&path).unwrap();

    for attempt in [
        ArchiveWriter::append(&path).map(|_| ()),
        RarArchive::open_append(&path).map(|_| ()),
    ] {
        match attempt {
            Err(RarError::Unsupported(_)) => {}
            Err(error) => panic!("expected Unsupported, got {error:?}"),
            Ok(()) => panic!("append of a RAR4 archive must be rejected"),
        }
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "rejected append must leave the original archive untouched"
    );
}

#[test]
fn abort_on_drop_leaves_no_data_or_recovery_volume_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abort-rv.rar");
    {
        let mut writer = ArchiveWriter::create_with(
            &path,
            WriterOptions::new()
                .volume_size(32 * 1024)
                .recovery_volume_count(1),
        )
        .unwrap();
        writer
            .add_bytes("payload.bin", &vec![7u8; 96 * 1024], stored())
            .unwrap();
        // Dropped without finish(): the transaction aborts, so neither the
        // data volumes nor the requested .rev files may ever appear.
    }
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        leftovers.is_empty(),
        "aborted transaction left files: {leftovers:?}"
    );
}
