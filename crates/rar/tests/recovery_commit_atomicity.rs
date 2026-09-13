//! Recovery-volume commit atomicity: a created multi-volume set must carry
//! its `.rev` files, they must be installed by the same transaction as the
//! data volumes, and a failed recovery build must leave the previous set
//! untouched instead of committing data without parity.

use std::path::Path;

use rar_rs::{
    ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions,
    WriterOptions,
};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn patterned(len: usize, modulo: usize) -> Vec<u8> {
    (0..len).map(|index| (index % modulo) as u8).collect()
}

fn rev_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rev"))
        .collect()
}

fn staging_leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.contains("rar5tmp") || name.contains("rar5bak") || name.contains("rar5commit")
        })
        .collect()
}

/// The `.rev` files exist right after a successful close, use the canonical
/// zero-padded names of the set, and commit together with the data volumes
/// (no recovery file appears before the close, when the set is still
/// staged).
#[test]
fn recovery_volumes_are_committed_with_the_data_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.rar");
    let payload = patterned(200_000, 251);

    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .volume_size(32 * 1024)
            .recovery_volume_count(2),
    )
    .unwrap();
    writer.add_bytes("big.bin", &payload, stored()).unwrap();
    // While the set is staged only temporary files exist: no final data
    // volume and no `.rev` sibling of the set.
    assert!(
        !dir.path().join("atomic.part1.rar").exists(),
        "the data set must be staged until close"
    );
    assert!(
        rev_names(dir.path()).is_empty(),
        "recovery volumes must not appear before the commit: {:?}",
        rev_names(dir.path())
    );
    let report = writer.finish().unwrap();

    let volumes = report.volume_paths();
    assert!(volumes.len() >= 2, "expected a volume set: {volumes:?}");
    assert!(
        volumes.iter().all(|path| path.exists()),
        "every data volume must be installed: {volumes:?}"
    );
    // One-digit sets are unpadded (`part1.rev`), matching the data volumes.
    let mut revs = rev_names(dir.path());
    revs.sort();
    assert_eq!(
        revs,
        vec![
            "atomic.part1.rev".to_string(),
            "atomic.part2.rev".to_string()
        ],
        "recovery names must match the set"
    );
    assert!(
        staging_leftovers(dir.path()).is_empty(),
        "the commit leaked staging files: {:?}",
        staging_leftovers(dir.path())
    );

    // The `.rev` files carry the REV5 signature and protect the installed
    // set: a missing volume can be rebuilt from them, and the archive still
    // reads back.
    let first_rev = std::fs::read(dir.path().join("atomic.part1.rev")).unwrap();
    assert!(first_rev.starts_with(b"Rar!\x1aRev"));
    let victim = volumes[1].clone();
    std::fs::remove_file(&victim).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(rebuilt, vec![victim], "the missing volume must be rebuilt");

    let mut reader = ArchiveReader::open(&volumes[0]).unwrap();
    let id = reader.unique_entry("big.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), payload);
}

/// A failure while the `.rev` files are generated (here: a staged volume
/// vanishes before the parity pass) must abort the whole commit: the
/// previous data volumes and their recovery files stay byte-identical, no
/// new set is installed, and no staging garbage is left behind.
#[test]
fn failed_recovery_build_leaves_the_previous_set_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollback.rar");
    let first_payload = patterned(100_000, 251);
    let second_payload = patterned(120_000, 253);

    // First set: commit normally and remember every file it installed.
    let old_volumes = {
        let mut writer = ArchiveWriter::create_with(
            &path,
            WriterOptions::new()
                .volume_size(32 * 1024)
                .recovery_volume_count(2),
        )
        .unwrap();
        writer
            .add_bytes("first.bin", &first_payload, stored())
            .unwrap();
        writer.finish().unwrap().into_volume_paths()
    };
    assert!(old_volumes.len() >= 2, "expected a volume set");
    let old_revs: Vec<std::path::PathBuf> = {
        let mut revs: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rev"))
            .collect();
        revs.sort();
        revs
    };
    assert_eq!(old_revs.len(), 2, "precondition: 2 recovery volumes");
    let old_bytes: Vec<Vec<u8>> = old_volumes
        .iter()
        .chain(old_revs.iter())
        .map(|path| std::fs::read(path).unwrap())
        .collect();

    // Second write over the same base. Remove one staged data volume after
    // the archive is fully staged but before close: the recovery build
    // reads the staged set, so it fails and the commit must not run.
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .volume_size(32 * 1024)
            .recovery_volume_count(2),
    )
    .unwrap();
    writer
        .add_bytes("second.bin", &second_payload, stored())
        .unwrap();
    let staged: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().contains("rar5tmp"))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rar"))
        .collect();
    assert!(!staged.is_empty(), "expected staged volumes");
    std::fs::remove_file(&staged[staged.len() - 1]).unwrap();

    assert!(writer.finish().is_err(), "the commit must fail");

    // The previous set (data + recovery) is byte-for-byte intact, and the
    // new member never became visible.
    for (path, bytes) in old_volumes.iter().chain(old_revs.iter()).zip(&old_bytes) {
        assert_eq!(
            &std::fs::read(path).unwrap(),
            bytes,
            "{} changed after the failed commit",
            path.display()
        );
    }
    let reader = ArchiveReader::open(&old_volumes[0]).unwrap();
    assert!(reader.unique_entry("first.bin").is_ok());
    assert!(
        reader.unique_entry("second.bin").is_err(),
        "the failed set must not be visible"
    );
    assert!(
        staging_leftovers(dir.path()).is_empty(),
        "the failed commit leaked staging files: {:?}",
        staging_leftovers(dir.path())
    );
}

/// A created set past nine volumes must install zero-padded `.rev` names
/// (`part01.rev`), matching the padded data volumes.
#[test]
fn padded_set_installs_padded_recovery_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("padded.rar");
    let payload = patterned(400_000, 251);

    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .volume_size(32 * 1024)
            .recovery_volume_count(2),
    )
    .unwrap();
    writer.add_bytes("big.bin", &payload, stored()).unwrap();
    let report = writer.finish().unwrap();
    let volumes = report.volume_paths();
    assert!(volumes.len() >= 10, "expected a padded set: {volumes:?}");

    let mut revs = rev_names(dir.path());
    revs.sort();
    assert_eq!(
        revs,
        vec![
            "padded.part01.rev".to_string(),
            "padded.part02.rev".to_string()
        ],
        ".rev names must follow the set's zero-padding"
    );
    // The parity protects the set: rebuild a missing padded volume.
    let victim = volumes[5].clone();
    std::fs::remove_file(&victim).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(rebuilt, vec![victim]);
    let mut reader = ArchiveReader::open(&volumes[0]).unwrap();
    let id = reader.unique_entry("big.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), payload);
}

/// The legacy RAR 1.5–4.x writer path builds recovery volumes through the
/// same pre-commit staging: they must be present right after close and
/// rebuild a missing data volume.
#[test]
fn rar4_recovery_volumes_are_staged_and_usable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.rar");
    let payload = patterned(200_000, 251);
    {
        let mut writer = ArchiveWriter::create_with(
            &path,
            WriterOptions::new()
                .compression(ArchiveVersion::V29)
                .volume_size(100_000)
                .recovery_volume_count(1),
        )
        .unwrap();
        writer.add_bytes("big.bin", &payload, stored()).unwrap();
        writer.finish().unwrap();
    }

    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() >= 2, "expected a legacy volume set");
    let revs = rev_names(dir.path());
    assert_eq!(revs.len(), 1, "expected one legacy .rev file: {revs:?}");

    // The `.rev` protects the volumes: deleting one must rebuild it.
    let victim = volumes[1].clone();
    std::fs::remove_file(&victim).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(rebuilt, vec![victim]);

    let mut reader = ArchiveReader::open(&volumes[0]).unwrap();
    let id = reader.unique_entry("big.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), payload);
    assert!(
        staging_leftovers(dir.path()).is_empty(),
        "legacy recovery commit leaked staging files: {:?}",
        staging_leftovers(dir.path())
    );
}
