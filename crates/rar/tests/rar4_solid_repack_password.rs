//! Regression tests: RAR4 solid-repack edits must preserve `-p` member
//! encryption (delete and the deferred solid-append repack).

use rar_rs::{
    AppendOptions, ArchiveEditor, ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel,
    EntryWriteOptions, OpenOptions, RarError, SolidMode, WriterOptions,
};

fn text(len: usize, seed: usize) -> Vec<u8> {
    let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
    (0..len)
        .map(|index| line[(index + seed) % line.len()])
        .collect()
}

fn normal() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL)
}

fn build_solid_encrypted(path: &std::path::Path, payloads: &[(&str, &[u8])]) {
    let mut writer = ArchiveWriter::create_with(
        path,
        WriterOptions::new()
            .compression(ArchiveVersion::V29)
            .solid_mode(SolidMode::Continuous)
            .password("secret"),
    )
    .unwrap();
    for (name, data) in payloads {
        writer.add_bytes(name, data, normal()).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn solid_delete_keeps_member_encryption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid-p.rar");
    let f1 = text(40_000, 0);
    let f2 = text(30_000, 7);
    let f3 = text(20_000, 13);
    build_solid_encrypted(&path, &[("f1.bin", &f1), ("f2.bin", &f2), ("f3.bin", &f3)]);

    let mut editor = ArchiveEditor::open_with_password(&path, "secret").unwrap();
    let f2_id = editor.unique_entry("f2.bin").unwrap();
    assert_eq!(editor.delete_entries(&[f2_id]).unwrap(), 1);
    drop(editor);

    // Listing works without a password (headers stay plaintext)...
    let listed = ArchiveReader::open(&path).unwrap();
    let names: Vec<_> = listed
        .entries()
        .map(|entry| entry.name().to_owned())
        .collect();
    assert_eq!(names, ["f1.bin", "f3.bin"]);
    drop(listed);

    // ...but reading still needs it: the repack must keep `-p`.
    let mut no_password = ArchiveReader::open(&path).unwrap();
    let f1_id = no_password.unique_entry("f1.bin").unwrap();
    assert!(
        no_password.read_entry(f1_id).is_err(),
        "repack stripped member encryption"
    );
    drop(no_password);

    let mut reader =
        ArchiveReader::open_with(&path, OpenOptions::new().password("secret")).unwrap();
    let f1_id = reader.unique_entry("f1.bin").unwrap();
    assert_eq!(reader.read_entry(f1_id).unwrap(), f1);
    let f3_id = reader.unique_entry("f3.bin").unwrap();
    assert_eq!(reader.read_entry(f3_id).unwrap(), f3);
    assert!(reader.verify().unwrap().is_ok());
}

#[test]
fn solid_append_keeps_member_encryption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid-append-p.rar");
    let f1 = text(30_000, 0);
    let f2 = text(30_000, 3);
    build_solid_encrypted(&path, &[("f1.bin", &f1), ("f2.bin", &f2)]);

    // Appending to a solid archive defers to a whole-archive repack at close.
    let f3 = text(20_000, 11);
    {
        let mut writer =
            ArchiveWriter::append_with(&path, AppendOptions::new().password("secret")).unwrap();
        writer.add_bytes("f3.bin", &f3, normal()).unwrap();
        writer.finish().unwrap();
    }

    let mut no_password = ArchiveReader::open(&path).unwrap();
    assert_eq!(no_password.entries().count(), 3);
    let f1_id = no_password.unique_entry("f1.bin").unwrap();
    assert!(
        no_password.read_entry(f1_id).is_err(),
        "deferred solid append stripped member encryption"
    );
    drop(no_password);

    let mut reader =
        ArchiveReader::open_with(&path, OpenOptions::new().password("secret")).unwrap();
    for (name, expected) in [("f1.bin", &f1), ("f2.bin", &f2), ("f3.bin", &f3)] {
        let id = reader.unique_entry(name).unwrap();
        assert_eq!(&reader.read_entry(id).unwrap(), expected, "{name}");
    }
    assert!(reader.verify().unwrap().is_ok());
}

#[test]
fn solid_delete_without_password_refuses_and_preserves_archive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid-refuse.rar");
    let f1 = text(30_000, 0);
    let f2 = text(30_000, 5);
    build_solid_encrypted(&path, &[("f1.bin", &f1), ("f2.bin", &f2)]);
    let before = std::fs::read(&path).unwrap();

    let mut editor = ArchiveEditor::open(&path).unwrap();
    let f2_id = editor.unique_entry("f2.bin").unwrap();
    let error = editor.delete_entries(&[f2_id]).unwrap_err();
    assert!(
        matches!(error, RarError::Encrypted(_)),
        "expected a clear password error, got {error:?}"
    );
    drop(editor);

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a refused repack must leave the archive untouched"
    );
    let mut reader =
        ArchiveReader::open_with(&path, OpenOptions::new().password("secret")).unwrap();
    assert_eq!(
        reader
            .read_entry(reader.unique_entry("f1.bin").unwrap())
            .unwrap(),
        f1
    );
}
