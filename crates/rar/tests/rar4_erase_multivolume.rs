//! Deleting every member of a multi-volume RAR4 set must erase the whole
//! set — every `.rar`/`.rNN` data volume and the `.rev` recovery volumes
//! for the same base — not just the volume that happened to be opened.

use rar_rs::{
    ArchiveEditor, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions,
    WriterOptions,
};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn patterned(len: usize, modulo: usize) -> Vec<u8> {
    (0..len).map(|index| (index % modulo) as u8).collect()
}

#[test]
fn deleting_every_member_erases_the_whole_rar4_volume_set() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("set.rar");
    {
        let mut writer = ArchiveWriter::create_with(
            &base,
            WriterOptions::new()
                .compression(ArchiveVersion::V29)
                .volume_size(64 * 1024),
        )
        .unwrap();
        writer
            .add_bytes("a.bin", &patterned(150_000, 251), stored())
            .unwrap();
        writer
            .add_bytes("b.bin", &patterned(150_000, 253), stored())
            .unwrap();
        writer.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&base);
    assert!(
        volumes.len() > 1,
        "precondition: multi-volume set: {volumes:?}"
    );
    // `.rev` recovery volumes for the same base must be retired too.
    let revs = rar_rs::build_recovery_volumes_for_set(&volumes, 1).unwrap();
    assert!(!revs.is_empty(), "precondition: .rev files present");

    let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
    let ids: Vec<_> = ["a.bin", "b.bin"]
        .iter()
        .map(|name| editor.unique_entry(name).unwrap())
        .collect();
    assert_eq!(editor.delete_entries(&ids).unwrap(), 2);
    assert_eq!(editor.entries().count(), 0);

    for path in volumes.iter().chain(revs.iter()) {
        assert!(!path.exists(), "{} must be removed", path.display());
    }
    // The journaled commit leaves no staging, journal or backup siblings.
    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.contains("rar5tmp") || name.contains("rar5bak") || name.contains("rar5commit")
        })
        .collect();
    assert!(leftovers.is_empty(), "commit leftovers: {leftovers:?}");
}
