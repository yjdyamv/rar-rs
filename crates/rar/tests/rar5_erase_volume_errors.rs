//! RAR5 erase-everything regressions: deleting the last member must remove
//! every volume of the set — including `.rev` recovery volumes — and must
//! report an error instead of success when a file cannot be removed.

use rar_rs::{ArchiveEditor, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn patterned(len: usize, modulo: usize) -> Vec<u8> {
    (0..len).map(|index| (index % modulo) as u8).collect()
}

/// Build a small multi-volume RAR5 set with `.rev` recovery volumes.
fn create_set(path: &std::path::Path) {
    let mut writer = ArchiveWriter::create_with(
        path,
        WriterOptions::new()
            .volume_size(30_000)
            .recovery_volumes_percent(50),
    )
    .unwrap();
    for index in 0..3 {
        let data = patterned(28_000, 251 + index);
        writer
            .add_bytes(&format!("m{index}.bin"), &data, stored())
            .unwrap();
    }
    writer.finish().unwrap();
}

fn set_files(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rar") || name.ends_with(".rev"))
        .collect();
    names.sort();
    names
}

fn staging_leftovers(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.contains("rar5tmp") || name.contains("rar5bak") || name.contains("rar5commit")
        })
        .collect()
}

fn entry_ids(editor: &ArchiveEditor) -> Vec<rar_rs::EntryId> {
    editor.entries().map(|entry| entry.id()).collect()
}

#[test]
fn erase_everything_removes_all_volumes_and_recovery_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("erase.rar");
    create_set(&path);

    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    assert!(
        set_files(dir.path())
            .iter()
            .any(|name| name.ends_with(".rev")),
        "precondition: .rev files present"
    );

    let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
    let ids = entry_ids(&editor);
    assert_eq!(ids.len(), 3);
    assert_eq!(editor.delete_entries(&ids).unwrap(), 3);
    drop(editor);

    assert_eq!(
        set_files(dir.path()),
        Vec::<String>::new(),
        "every volume and .rev file must be erased"
    );
    assert!(staging_leftovers(dir.path()).is_empty());
}

/// A recovery file that cannot be removed (here: a directory occupying a
/// `.rev` name, which `remove_file` refuses on every platform) must surface
/// as an error and must not be silently skipped while reporting success.
#[test]
fn erase_surfaces_a_recovery_file_that_cannot_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stuck.rar");
    create_set(&path);

    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    let stuck = dir.path().join("stuck.part01.rev");
    std::fs::create_dir(&stuck).unwrap();

    let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
    let ids = entry_ids(&editor);
    match editor.delete_entries(&ids) {
        Err(rar_rs::RarError::Io(_)) => {}
        other => panic!("an unremovable recovery file must surface an I/O error, got {other:?}"),
    }
    assert!(stuck.is_dir(), "the unremovable path must remain");
}

/// Windows lock regression: a volume held open without sharing must surface
/// an error (the removal used to be ignored and the erase reported success).
#[cfg(windows)]
#[test]
fn erase_reports_a_locked_volume_instead_of_success() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locked.rar");
    create_set(&path);

    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() > 1, "precondition: multi-volume set");

    // Collect the catalog before locking a later volume (the open scan
    // itself reads every volume).
    let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
    let ids = entry_ids(&editor);

    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&volumes[1])
        .unwrap();

    match editor.delete_entries(&ids) {
        Err(rar_rs::RarError::Io(_)) => {}
        other => panic!("a locked volume must surface an I/O error, got {other:?}"),
    }
    assert!(volumes[1].exists(), "the locked volume must not disappear");
    drop(locked);
}
