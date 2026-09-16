//! Quick-open fast path vs. NTFS alternate data streams: the QO record
//! caches file headers only, so extraction from a quick-open catalog must
//! still discover the "STM" service records and restore the streams.

#![cfg(windows)]

use rar_rs::{
    ArchiveReader, ArchiveWriter, EntryWriteOptions, ExtractOptions, OpenOptions, ScanStrategy,
    WriterOptions,
};

/// Index of the volume holding the first "STM" service block (block type
/// 0x03 with `BLOCK_FLAG_DEPENDS_PREV` 0x20), if any.
fn stream_record_volume(volumes: &[std::path::PathBuf]) -> Option<usize> {
    use std::io::{Seek, SeekFrom};

    for (index, volume) in volumes.iter().enumerate() {
        let Ok(mut file) = std::fs::File::open(volume) else {
            continue;
        };
        // Every volume carries its own 8-byte RAR5 signature.
        file.seek(SeekFrom::Start(8)).ok()?;
        while let Ok(Some(meta)) = rar_rs::wire::read_block(&mut file, None) {
            if meta.block_type == 0x03 && meta.flags & 0x20 != 0 {
                return Some(index);
            }
            // `read_block` stops at the data area; skip it explicitly.
            file.seek(SeekFrom::Start(meta.data_end)).ok()?;
        }
    }
    None
}

/// A multi-volume `-os` set can place the "STM" block in a later volume
/// (the writer rolls to the next volume before emitting it). Extraction
/// must read the record from the volume that holds it; the old reader
/// always sought the primary volume and silently dropped the stream.
#[test]
fn multivolume_extraction_restores_ntfs_streams() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir");
    let src = src_dir.join("owner.bin");
    // A compressible head plus an incompressible tail keeps the packed
    // member spanning several 64 KiB volumes (the random tail dominates the
    // packed size while the head keeps compression a net win).
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut payload = vec![0u8; 128 * 1024];
    payload.extend((0..300_000).map(|_| {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
    }));
    std::fs::write(&src, &payload).expect("write member");
    let stream = b"alternate stream payload for a multi-volume set".repeat(20);
    std::fs::write(format!("{}{}", src.display(), ":ads"), &stream).expect("write stream");

    let archive = dir.path().join("streams-mv.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &archive,
            WriterOptions::default()
                .save_streams(true)
                .volume_size(64 * 1024),
        )
        .expect("create");
        rar.add_path_as(&src, "owner.bin", EntryWriteOptions::new())
            .expect("add");
        rar.finish().expect("close");
    }

    let volumes = rar_rs::discover_volumes(&archive);
    assert!(volumes.len() >= 2, "expected a split set");
    assert!(
        matches!(stream_record_volume(&volumes), Some(volume) if volume > 0),
        "the STM record must sit in a later volume for this regression test"
    );

    let out = dir.path().join("out");
    let mut reader = ArchiveReader::open(&archive).expect("open");
    reader
        .extract_all_with_options(&out, ExtractOptions::default())
        .expect("extract multi-volume");
    assert_eq!(std::fs::read(out.join("owner.bin")).unwrap(), payload);
    assert_eq!(
        std::fs::read(format!("{}{}", out.join("owner.bin").display(), ":ads"))
            .expect("restored stream"),
        stream,
        "the stream must be restored from its own volume"
    );
}

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

/// The multi-volume re-split rewrite must carry surviving "STM" records
/// over to the rebuilt set: a delete of a stream-free member used to drop
/// the surviving member's streams (then briefly refused the edit).
#[test]
fn multivolume_delete_keeps_ntfs_streams() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir");
    let src = src_dir.join("owner.bin");
    let payload = vec![0x4Cu8; 100 * 1024];
    std::fs::write(&src, &payload).expect("write member");
    std::fs::write(format!("{}{}", src.display(), ":ads"), b"stream").expect("write stream");
    let plain = src_dir.join("plain.bin");
    std::fs::write(&plain, b"no streams here").expect("write plain member");

    let archive = dir.path().join("streams-edit.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &archive,
            WriterOptions::default()
                .save_streams(true)
                .volume_size(32 * 1024),
        )
        .expect("create");
        let stored = || EntryWriteOptions::new().compression_level(rar_rs::CompressionLevel::STORE);
        rar.add_path_as(&src, "owner.bin", stored())
            .expect("add owner");
        rar.add_path_as(&plain, "plain.bin", stored())
            .expect("add plain");
        rar.finish().expect("close");
    }
    let volumes = rar_rs::discover_volumes(&archive);
    assert!(volumes.len() > 1, "precondition: multi-volume set");

    let mut editor = rar_rs::ArchiveEditor::open(&volumes[0]).expect("editor");
    let id = editor.unique_entry("plain.bin").expect("member");
    editor.delete_entries(&[id]).expect("delete");
    drop(editor);

    // The stream survives with its owner and extraction restores it.
    let out = dir.path().join("out");
    let mut reader = ArchiveReader::open(&archive).expect("reopen");
    let names: Vec<String> = reader
        .entries()
        .map(|entry| entry.name().to_owned())
        .collect();
    assert_eq!(names, ["owner.bin"], "plain.bin is gone");
    reader
        .extract_all_with_options(&out, ExtractOptions::default())
        .expect("extract");
    assert_eq!(std::fs::read(out.join("owner.bin")).unwrap(), payload);
    assert_eq!(
        std::fs::read(format!("{}{}", out.join("owner.bin").display(), ":ads"))
            .expect("restored stream"),
        b"stream",
        "the surviving member's stream must be re-emitted"
    );
}

/// `-p` archives store each "STM" payload with its own ENCR record; the
/// rewrite must re-encrypt surviving streams (and only those) so the set
/// still decodes with the password.
#[test]
fn multivolume_delete_keeps_encrypted_ntfs_streams() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir");
    let src = src_dir.join("owner.bin");
    let payload = vec![0x5Du8; 100 * 1024];
    std::fs::write(&src, &payload).expect("write member");
    std::fs::write(format!("{}{}", src.display(), ":ads"), b"secret stream").expect("write stream");
    let plain = src_dir.join("plain.bin");
    std::fs::write(&plain, b"no streams here").expect("write plain member");

    let archive = dir.path().join("streams-enc.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &archive,
            WriterOptions::default()
                .password("pw")
                .save_streams(true)
                .volume_size(32 * 1024),
        )
        .expect("create");
        let stored = || EntryWriteOptions::new().compression_level(rar_rs::CompressionLevel::STORE);
        rar.add_path_as(&src, "owner.bin", stored())
            .expect("add owner");
        rar.add_path_as(&plain, "plain.bin", stored())
            .expect("add plain");
        rar.finish().expect("close");
    }
    let volumes = rar_rs::discover_volumes(&archive);
    assert!(volumes.len() > 1, "precondition: multi-volume set");

    let mut editor = rar_rs::ArchiveEditor::open_with_password(&volumes[0], "pw").expect("editor");
    let id = editor.unique_entry("plain.bin").expect("member");
    editor.delete_entries(&[id]).expect("delete");
    drop(editor);

    let out = dir.path().join("out");
    let mut reader = ArchiveReader::open_with(&archive, OpenOptions::new().password("pw"))
        .expect("reopen with password");
    reader
        .extract_all_with_options(&out, ExtractOptions::default())
        .expect("extract");
    assert_eq!(std::fs::read(out.join("owner.bin")).unwrap(), payload);
    assert_eq!(
        std::fs::read(format!("{}{}", out.join("owner.bin").display(), ":ads"))
            .expect("restored stream"),
        b"secret stream",
        "the re-encrypted stream must decode with the archive password"
    );
}
