//! Archive comments and locking on a multi-volume RAR4 set must target the
//! set's first volume regardless of which part was opened, and the comment
//! must read back from any part.

use rar_rs::{
    ArchiveEditor, ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EditPlan,
    EntryWriteOptions, RarError, WriterOptions,
};

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn patterned(len: usize, modulo: usize) -> Vec<u8> {
    (0..len).map(|index| (index % modulo) as u8).collect()
}

/// Build a multi-volume RAR4 (`unp_ver 29`) set with two stored members and
/// return its volumes in order.
fn build_set(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let base = dir.join("set.rar");
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
    volumes
}

/// RAR4 main-header flags of a non-SFX volume: signature at 0, main header
/// block at 7, flags at bytes 10..12.
fn main_flags(bytes: &[u8]) -> u16 {
    assert_eq!(
        &bytes[..7],
        b"Rar!\x1a\x07\x00",
        "expected a plain RAR4 volume"
    );
    u16::from_le_bytes([bytes[10], bytes[11]])
}

fn comment_of(path: &std::path::Path) -> Option<Vec<u8>> {
    let mut archive = ArchiveReader::open(path).unwrap();
    archive.comment().unwrap()
}

#[test]
fn comment_set_from_a_later_part_lands_on_the_first_volume() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = build_set(dir.path());
    let first = volumes[0].clone();
    let later = volumes[1].clone();
    assert_eq!(comment_of(&first), None);
    assert_eq!(comment_of(&later), None);

    // Open from a later part and set the comment.
    let mut editor = ArchiveEditor::open(&later).unwrap();
    editor
        .apply(EditPlan::new().set_comment(b"first-volume note".to_vec()))
        .unwrap();
    drop(editor);

    // The CMT block landed on the first volume and reads back from any part.
    assert_eq!(comment_of(&first), Some(b"first-volume note".to_vec()));
    for volume in &volumes {
        assert_eq!(
            comment_of(volume),
            Some(b"first-volume note".to_vec()),
            "{} must see the comment",
            volume.display()
        );
    }

    // The members survive the per-volume rewrite.
    let mut reader = ArchiveReader::open(&first).unwrap();
    let a = reader.unique_entry("a.bin").unwrap();
    assert_eq!(reader.read_entry(a).unwrap(), patterned(150_000, 251));
    let b = reader.unique_entry("b.bin").unwrap();
    assert_eq!(reader.read_entry(b).unwrap(), patterned(150_000, 253));
}

#[test]
fn lock_from_a_later_part_targets_the_first_volume() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = build_set(dir.path());
    let first = volumes[0].clone();
    let later = volumes[1].clone();

    let mut editor = ArchiveEditor::open(&later).unwrap();
    editor.lock().unwrap();
    drop(editor);

    let first_bytes = std::fs::read(&first).unwrap();
    assert_ne!(
        main_flags(&first_bytes) & 0x0004,
        0,
        "MHD_LOCK must land on the first volume"
    );
    let later_bytes = std::fs::read(&later).unwrap();
    assert_eq!(
        main_flags(&later_bytes) & 0x0004,
        0,
        "later volumes stay untouched"
    );

    // The locked set refuses further edits through any part: the lock lives
    // on the first volume, which is what every edit checks.
    for opened in [&first, &later] {
        let mut editor = ArchiveEditor::open(opened).unwrap();
        assert!(
            matches!(
                editor.apply(EditPlan::new().set_comment(b"x".to_vec())),
                Err(RarError::ArchiveLocked)
            ),
            "{} must observe the lock",
            opened.display()
        );
    }
}
