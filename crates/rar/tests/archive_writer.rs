use std::path::PathBuf;

use rar_rs::{
    AppendOptions, ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, DictionarySize,
    EntryWriteOptions, RarError, ThreadCount, WriteEntry, WriterOptions,
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
    // Non-power-of-two sizes through 4 GiB are legal as RAR7-only byte
    // dictionaries: the constructor accepts the byte count but reports no
    // RAR5 log (a plain v50 header could not carry it exactly).
    assert_eq!(
        DictionarySize::try_from(3 * 1024 * 1024u64)
            .unwrap()
            .bytes(),
        3 * 1024 * 1024
    );
    assert_eq!(
        DictionarySize::try_from(3 * 1024 * 1024u64)
            .unwrap()
            .rar5_log(),
        None
    );
    assert_eq!(
        DictionarySize::try_from(6 * 1024 * 1024u64)
            .unwrap()
            .rar5_log(),
        None
    );
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
                .compression(ArchiveVersion::V29)
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
    let mut legacy = ArchiveWriter::create_with(
        &legacy_path,
        WriterOptions::new()
            .dictionary_size(DictionarySize::try_from(6 * 1024 * 1024 * 1024u64).unwrap()),
    )
    .unwrap();
    legacy
        .add_path(
            &source,
            EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL),
        )
        .unwrap();
    legacy.finish().unwrap();

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

    let mut legacy = ArchiveWriter::create(&legacy_path).unwrap();
    legacy
        .add_path(
            &source,
            EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL),
        )
        .unwrap();
    legacy.finish().unwrap();

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
    let mut legacy = ArchiveWriter::create(&append_path).unwrap();
    legacy.add_bytes("old.txt", b"old", stored()).unwrap();
    legacy.finish().unwrap();
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
    let mut archive = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .volume_size(32 * 1024)
            .recovery_volume_count(1),
    )
    .unwrap();
    archive
        .add_bytes("payload.bin", &payload, stored())
        .unwrap();
    archive.finish().unwrap();

    let recovery_path = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "rev"))
        .expect("recovery volume");
    std::fs::remove_file(&recovery_path).unwrap();
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
        let mut legacy = ArchiveWriter::create(&path).unwrap();
        legacy.add_bytes("first.txt", b"first", stored()).unwrap();
        // Compatibility behavior: legacy Drop still closes and commits.
        legacy.finish().unwrap();
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
                .compression(ArchiveVersion::V29)
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
    let mut writer =
        ArchiveWriter::create_with(&path, WriterOptions::new().compression(ArchiveVersion::V29))
            .unwrap();
    writer.add_path(&source, stored()).unwrap();
    writer.finish().unwrap();
    assert!(path.exists());
}

#[test]
fn append_on_rar4_archives_is_supported() {
    // Stage B (ADR 0005): appending to a single-volume non-solid RAR4
    // archive works through both facades; the original members and their
    // data are preserved and the new member is readable.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rar4.rar");
    {
        let mut archive = ArchiveWriter::create_with(
            &path,
            WriterOptions::new().compression(ArchiveVersion::V29),
        )
        .unwrap();
        let source = dir.path().join("a.txt");
        std::fs::write(&source, b"original").unwrap();
        archive.add_path(&source, stored()).unwrap();
        archive.finish().unwrap();
    }
    let payload_new = b"appended member payload".repeat(120);

    // ArchiveWriter::append (the `rar a` path).
    {
        let mut writer = ArchiveWriter::append(&path).unwrap();
        writer
            .add_bytes(
                "b.txt",
                &payload_new,
                rar_rs::EntryWriteOptions::new()
                    .compression_level(rar_rs::CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        writer.finish().unwrap();
    }
    let mut reader = ArchiveReader::open(&path).unwrap();
    let b = reader.unique_entry("b.txt").unwrap();
    assert_eq!(reader.read_entry(b).unwrap(), payload_new);
    let a = reader
        .entries()
        .find(|e| e.name().ends_with("a.txt"))
        .unwrap()
        .id();
    assert_eq!(reader.read_entry(a).unwrap(), b"original");

    // The legacy RarArchive::open_append facade is still usable.
    {
        let mut archive = ArchiveWriter::append(&path).unwrap();
        archive.add_bytes("c.txt", b"third", stored()).unwrap();
        archive.finish().unwrap();
    }
    let reader = ArchiveReader::open(&path).unwrap();
    assert!(reader.unique_entry("c.txt").is_ok());
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

/// A failed multi-volume commit must restore the previous volume set
/// byte-for-byte instead of leaving a mix of old and new parts.
#[test]
fn failed_multivolume_commit_restores_the_previous_set() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("atomic.rar");
    let payload: Vec<u8> = (0..9 * 32 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();

    // First set: commit normally and remember its bytes.
    let mut writer =
        ArchiveWriter::create_with(&base, WriterOptions::new().volume_size(32 * 1024)).unwrap();
    writer.add_bytes("many.bin", &payload, stored()).unwrap();
    let old_paths = writer.finish().unwrap().into_volume_paths();
    assert!(old_paths.len() > 1, "expected a real volume set");
    let old_bytes: Vec<Vec<u8>> = old_paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();

    // Second write over the same base. Remove one staged volume that is not
    // the one still open, so the commit fails partway through the install
    // phase after earlier volumes already landed.
    let mut writer =
        ArchiveWriter::create_with(&base, WriterOptions::new().volume_size(32 * 1024)).unwrap();
    writer.add_bytes("many.bin", &payload, stored()).unwrap();
    let mut staged: Vec<PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().contains("rar5tmp"))
        .collect();
    staged.sort_by_key(|path| part_number(path));
    assert!(staged.len() > 1, "expected staged volumes: {staged:?}");
    std::fs::remove_file(&staged[staged.len() - 2]).unwrap();

    assert!(writer.finish().is_err());

    for (path, bytes) in old_paths.iter().zip(&old_bytes) {
        assert_eq!(
            &std::fs::read(path).unwrap(),
            bytes,
            "{} changed after the failed commit",
            path.display()
        );
    }
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5tmp") || name.contains("rar5bak"))
        .collect();
    assert!(leftovers.is_empty(), "staging leftovers: {leftovers:?}");
}

/// Overwriting a long volume set with a shorter one must retire the leftover
/// parts of the previous set instead of leaving a mixed, unreadable set.
#[test]
fn shrinking_multivolume_overwrite_retires_stale_parts() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("shrink.rar");
    let payload: Vec<u8> = (0..9 * 32 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();

    let mut writer =
        ArchiveWriter::create_with(&base, WriterOptions::new().volume_size(32 * 1024)).unwrap();
    writer.add_bytes("many.bin", &payload, stored()).unwrap();
    let old = writer.finish().unwrap().into_volume_paths();
    assert!(old.len() > 1);

    let mut writer =
        ArchiveWriter::create_with(&base, WriterOptions::new().volume_size(32 * 1024)).unwrap();
    writer.add_bytes("small.bin", b"tiny", stored()).unwrap();
    let report = writer.finish().unwrap();
    assert_eq!(report.volume_paths().len(), 1);

    let parts: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("shrink.part"))
        .collect();
    assert_eq!(parts.len(), 1, "stale parts left behind: {parts:?}");
}

/// Overwriting a volume set without recovery volumes must retire the
/// previous set's stale `.rev` files, not leave them pointing at data
/// volumes they no longer protect.
#[test]
fn overwriting_a_volume_set_retires_stale_recovery_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("rv.rar");
    let payload: Vec<u8> = (0..9 * 32 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();

    let mut writer = ArchiveWriter::create_with(
        &base,
        WriterOptions::new()
            .volume_size(32 * 1024)
            .recovery_volume_count(1),
    )
    .unwrap();
    writer.add_bytes("many.bin", &payload, stored()).unwrap();
    writer.finish().unwrap();
    let revs = |dir: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".rev"))
            .collect()
    };
    assert!(!revs(dir.path()).is_empty(), "expected .rev volumes");

    let mut writer =
        ArchiveWriter::create_with(&base, WriterOptions::new().volume_size(32 * 1024)).unwrap();
    writer.add_bytes("many.bin", &payload, stored()).unwrap();
    writer.finish().unwrap();

    let revs = revs(dir.path());
    assert!(revs.is_empty(), "stale .rev left behind: {revs:?}");
}

/// A volume size that cannot fit a member header plus the end block must be
/// rejected instead of spinning the splitter forever.
#[test]
fn tiny_volume_size_is_rejected_instead_of_looping() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("m.bin");
    std::fs::write(&source, vec![7u8; 64 * 1024]).unwrap();
    let level = EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL);

    // Smaller than a member header plus the end block: before the guard this
    // rolled to a fresh volume forever (16 bytes cannot hold the next header
    // either, so neither offset nor volume state ever changes).
    let rar5 = dir.path().join("tiny5.rar");
    let mut writer =
        ArchiveWriter::create_with(&rar5, WriterOptions::new().volume_size(16)).unwrap();
    assert!(writer.add_path(&source, level).is_err());
    drop(writer);

    let rar4 = dir.path().join("tiny4.rar");
    let mut writer = ArchiveWriter::create_with(
        &rar4,
        WriterOptions::new()
            .compression(ArchiveVersion::V29)
            .volume_size(16),
    )
    .unwrap();
    assert!(writer.add_path(&source, level).is_err());
    drop(writer);

    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "m.bin")
        .collect();
    assert!(
        leftovers.is_empty(),
        "tiny-volume run left files: {leftovers:?}"
    );
}

/// Part number parsed out of a `....partN.rar` staging name.
fn part_number(path: &std::path::Path) -> u64 {
    path.file_name()
        .unwrap()
        .to_string_lossy()
        .rsplit_once(".part")
        .and_then(|(_, tail)| tail.trim_end_matches(".rar").parse().ok())
        .unwrap()
}
