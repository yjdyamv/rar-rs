#![cfg(windows)]
use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, ExtractOptions,
    WriterOptions,
};

use crate::support::{rar_bin, run, temp_dir, unrar_bin};

// ── -os NTFS alternate data streams (Windows only) ──────────────────────────

/// `-os` streams interoperate in both directions (Windows only: alternate
/// data streams are an NTFS concept).
#[cfg(windows)]
#[test]
fn os_streams_interop_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("ads.bin");
    std::fs::write(&src, b"main stream data").unwrap();
    let stream_name = ":custom1";
    let stream_data = b"alternate stream payload".to_vec();
    std::fs::write(format!("{}{}", src.display(), stream_name), &stream_data).unwrap();
    // Verify the stream exists before archiving.
    assert_eq!(
        std::fs::read(format!("{}{}", src.display(), stream_name)).unwrap(),
        stream_data
    );

    // Ours -> WinRAR: `UnRAR x -os` must restore the stream.
    let ours = dir.path().join("ours_os.rar");
    {
        let mut rar =
            ArchiveWriter::create_with(&ours, WriterOptions::default().save_streams(true)).unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    if let Some(unrar) = unrar_bin() {
        let win = dir.path().join("win_os");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-os", "-y", "-idq"])
            .arg(&ours)
            .arg(&win));
        assert!(ok, "WinRAR x -os failed on our archive:\n{out}");
        let restored = std::fs::read(format!("{}{}", win.join("ads.bin").display(), stream_name));
        assert_eq!(
            restored.unwrap(),
            stream_data,
            "WinRAR must restore our stream"
        );
    }

    // WinRAR -> ours: we restore the stream from its -os archive.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_os.rar");
        // Use a relative member path so the stored name stays relative.
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-os", "-idq"])
            .arg(&theirs)
            .arg("ads.bin")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -os failed:\n{out}");
        let out_dir = dir.path().join("ours_os");
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut ar = ArchiveReader::open(&theirs).unwrap();
        ar.extract_all_with_options(&out_dir, ExtractOptions::default())
            .unwrap();
        let restored = std::fs::read(format!(
            "{}{}",
            out_dir.join("ads.bin").display(),
            stream_name
        ));
        assert_eq!(
            restored.unwrap(),
            stream_data,
            "we must restore WinRAR's stream"
        );
    }
}
