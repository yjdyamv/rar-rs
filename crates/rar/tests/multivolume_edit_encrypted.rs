//! Multi-volume RAR5 edit regressions: encrypted payloads must stay
//! encrypted through a delete, a rewrite that grows the staged set must
//! install every volume, and `.rev` recovery volumes must be regenerated
//! for zero-padded sets.

use rar_rs::{
    ArchiveEditor, ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, OpenOptions,
    WriterOptions,
};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn patterned(len: usize, modulo: usize) -> Vec<u8> {
    (0..len).map(|index| (index % modulo) as u8).collect()
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

fn rev_names(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rev"))
        .collect()
}

#[test]
fn delete_from_encrypted_multivolume_set_keeps_survivor_decryptable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enc-set.rar");
    let a = patterned(70_000, 251);
    let b = patterned(70_000, 253);
    {
        let mut writer = ArchiveWriter::create_with(
            &path,
            WriterOptions::new().volume_size(60_000).password("secret"),
        )
        .unwrap();
        writer.add_bytes("a.bin", &a, stored()).unwrap();
        writer.add_bytes("b.bin", &b, stored()).unwrap();
        writer.finish().unwrap();
    }
    let before = rar_rs::discover_volumes(&path);
    assert!(before.len() >= 2, "precondition: multi-volume set");

    // Delete from a later volume, like the reported CLI repro.
    let mut editor =
        ArchiveEditor::open_with_password(before.last().unwrap().as_path(), "secret").unwrap();
    let b_id = editor.unique_entry("b.bin").unwrap();
    editor.delete_entries(&[b_id]).unwrap();
    drop(editor);

    // The surviving member must still verify with the password: it used to
    // be written as plaintext under its ENCR header, failing CRC.
    let volumes = rar_rs::discover_volumes(&path);
    let mut reader =
        ArchiveReader::open_with(&volumes[0], OpenOptions::new().password("secret")).unwrap();
    let a_id = reader.unique_entry("a.bin").unwrap();
    assert_eq!(reader.read_entry(a_id).unwrap(), a);
    assert!(reader.verify().unwrap().is_ok(), "survivor must verify");
    assert!(staging_leftovers(dir.path()).is_empty());
}

#[test]
fn rename_growing_the_staged_set_installs_every_volume() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grow.rar");
    let payload = patterned(102_100, 257);
    {
        let mut writer =
            ArchiveWriter::create_with(&path, WriterOptions::new().volume_size(50 * 1024)).unwrap();
        writer.add_bytes("a.bin", &payload, stored()).unwrap();
        writer.finish().unwrap();
    }
    let before = rar_rs::discover_volumes(&path);
    assert_eq!(before.len(), 2, "precondition: two-volume set");

    // A 254-character name pushes the split past the old volume count; the
    // rewrite used to install only the old number of parts and leak the rest.
    let long = "x".repeat(254);
    let mut editor = ArchiveEditor::open(&before[0]).unwrap();
    let id = editor.unique_entry("a.bin").unwrap();
    assert_eq!(editor.rename_entries(&[(id, long.clone())]).unwrap(), 1);
    drop(editor);

    let after = rar_rs::discover_volumes(&path);
    assert_eq!(
        after.len(),
        3,
        "the grown staged set must be fully installed: {after:?}"
    );
    assert!(
        staging_leftovers(dir.path()).is_empty(),
        "staging leaked: {:?}",
        staging_leftovers(dir.path())
    );
    let mut reader = ArchiveReader::open(&after[0]).unwrap();
    let id = reader.unique_entry(&long).unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), payload);
    assert!(reader.verify().unwrap().is_ok());
}

#[test]
fn delete_on_padded_set_regenerates_padded_recovery_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("padded.rar");
    {
        let mut writer = ArchiveWriter::create_with(
            &path,
            WriterOptions::new()
                .volume_size(30_000)
                .recovery_volumes_percent(50),
        )
        .unwrap();
        for index in 0..12 {
            let member = patterned(28_000, 251 + index);
            writer
                .add_bytes(&format!("m{index:02}.bin"), &member, stored())
                .unwrap();
        }
        writer.finish().unwrap();
    }
    let before = rar_rs::discover_volumes(&path);
    assert!(before.len() >= 10, "precondition: padded volume set");
    assert!(
        !rev_names(dir.path()).is_empty(),
        "precondition: .rev files present"
    );

    let mut editor = ArchiveEditor::open(&before[0]).unwrap();
    let victim = editor.unique_entry("m05.bin").unwrap();
    editor.delete_entries(&[victim]).unwrap();
    drop(editor);

    let after = rar_rs::discover_volumes(&path);
    assert!(after.len() >= 10, "set should stay padded: {after:?}");
    let regenerated = rev_names(dir.path());
    assert!(
        !regenerated.is_empty(),
        "recovery volumes must be regenerated: {regenerated:?}"
    );
    assert!(
        regenerated.iter().any(|name| name == "padded.part01.rev"),
        "expected padded recovery names: {regenerated:?}"
    );
    assert!(staging_leftovers(dir.path()).is_empty());

    let mut reader = ArchiveReader::open(&after[0]).unwrap();
    assert!(reader.verify().unwrap().is_ok());
    assert!(reader.unique_entry("m05.bin").is_err());
    let m06 = reader.unique_entry("m06.bin").unwrap();
    assert_eq!(reader.read_entry(m06).unwrap(), patterned(28_000, 251 + 6));
}
