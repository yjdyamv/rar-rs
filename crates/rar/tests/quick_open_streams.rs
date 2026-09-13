//! Quick-open fast path vs. NTFS alternate data streams: the QO record
//! caches file headers only, so extraction from a quick-open catalog must
//! still discover the "STM" service records and restore the streams.

#![cfg(windows)]

use rar_rs::{
    ArchiveReader, ArchiveWriter, EntryWriteOptions, ExtractOptions, OpenOptions, ScanStrategy,
    WriterOptions,
};

#[test]
fn quick_open_extraction_restores_ntfs_streams() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir");
    let src = src_dir.join("owner.bin");
    let payload = b"main stream data".repeat(8);
    std::fs::write(&src, &payload).expect("write member");
    let stream = b"alternate stream payload".repeat(4);
    std::fs::write(format!("{}{}", src.display(), ":ads"), &stream).expect("write stream");

    let archive = dir.path().join("streams.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &archive,
            WriterOptions::default().quick_open(true).save_streams(true),
        )
        .expect("create");
        rar.add_path_as(&src, "owner.bin", EntryWriteOptions::new())
            .expect("add");
        rar.finish().expect("close");
    }

    // Full-scan extraction is the control: it must restore the stream.
    let full_out = dir.path().join("out_full");
    {
        let mut full = ArchiveReader::open(&archive).expect("open");
        full.extract_all_with_options(&full_out, ExtractOptions::default())
            .expect("extract full scan");
    }
    assert_eq!(
        std::fs::read(format!(
            "{}{}",
            full_out.join("owner.bin").display(),
            ":ads"
        ))
        .unwrap(),
        stream,
        "full scan must restore the stream"
    );

    // Quick-open extraction must restore the same stream.
    let quick_out = dir.path().join("out_quick");
    {
        let mut quick = ArchiveReader::open_with(
            &archive,
            OpenOptions::new().scan_strategy(ScanStrategy::PreferQuickOpen),
        )
        .expect("open quick");
        assert_eq!(quick.entries().count(), 1);
        quick
            .extract_all_with_options(&quick_out, ExtractOptions::default())
            .expect("extract quick-open");
    }
    assert_eq!(
        std::fs::read(quick_out.join("owner.bin")).unwrap(),
        payload,
        "quick-open must extract the member data"
    );
    assert_eq!(
        std::fs::read(format!(
            "{}{}",
            quick_out.join("owner.bin").display(),
            ":ads"
        ))
        .unwrap(),
        stream,
        "quick-open must restore the stream"
    );
}
