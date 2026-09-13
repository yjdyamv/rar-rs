//! RAR4 delete/rename on a moderately large archive: the rewrite streams
//! the member copy path through bounded buffers and still round-trips.

use rar_rs::{
    ArchiveEditor, ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EditPlan,
    EntryWriteOptions, WriterOptions,
};

const MIB: usize = 1 << 20;

fn stored() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn pattern() -> Vec<u8> {
    (0..MIB).map(|index| (index % 251) as u8).collect()
}

#[test]
fn large_rar4_member_delete_and_rename_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.rar");
    let member = dir.path().join("big.bin");
    // 128 MiB written through a small repeating pattern, so the fixture
    // itself never holds the whole member.
    {
        let mut file = std::fs::File::create(&member).unwrap();
        let pattern = pattern();
        for _ in 0..128 {
            std::io::Write::write_all(&mut file, &pattern).unwrap();
        }
    }
    {
        let mut writer = ArchiveWriter::create_with(
            &path,
            WriterOptions::new().compression(ArchiveVersion::V29),
        )
        .unwrap();
        writer.add_path(&member, stored()).unwrap();
        writer
            .add_bytes("small.txt", b"small member", stored())
            .unwrap();
        writer.finish().unwrap();
    }
    assert!(std::fs::metadata(&path).unwrap().len() >= 128 * MIB as u64);

    let mut editor = ArchiveEditor::open(&path).unwrap();
    let big = editor.unique_entry("big.bin").unwrap();
    let small = editor.unique_entry("small.txt").unwrap();
    let report = editor
        .apply(EditPlan::new().rename(big, "renamed.bin").delete(small))
        .unwrap();
    assert_eq!((report.deleted(), report.renamed()), (1, 1));
    drop(editor);

    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry("renamed.bin").unwrap();
    let data = reader.read_entry(id).unwrap();
    assert_eq!(data.len(), 128 * MIB);
    let pattern = pattern();
    for chunk in data.as_chunks::<MIB>().0 {
        assert_eq!(chunk, pattern.as_slice());
    }
    assert!(reader.unique_entry("small.txt").is_err());
}
