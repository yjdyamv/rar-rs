#![cfg(windows)]
use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, ExtractOptions, OpenOptions,
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

/// The `rar` CLI's `-os` flag stores streams; WinRAR restores them and our
/// CLI restores WinRAR's. The library path is covered above; this pins the
/// CLI wiring.
#[cfg(windows)]
#[test]
fn os_streams_cli_interop_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("ads.bin");
    std::fs::write(&src, b"main stream data").unwrap();
    let stream_name = ":cli1";
    let stream_data = b"cli alternate payload".to_vec();
    std::fs::write(format!("{}{}", src.display(), stream_name), &stream_data).unwrap();

    let ours = dir.path().join("ours_cli_os.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-os", "-idq"])
        .arg(&ours)
        .arg("ads.bin")
        .current_dir(dir.path()));
    assert!(ok, "our CLI a -os failed:\n{out}");

    if let Some(unrar) = unrar_bin() {
        let win = dir.path().join("win_cli_os");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-os", "-y", "-idq"])
            .arg(&ours)
            .arg(&win));
        assert!(ok, "UnRAR x -os failed on our CLI archive:\n{out}");
        assert_eq!(
            std::fs::read(format!("{}{}", win.join("ads.bin").display(), stream_name)).unwrap(),
            stream_data,
            "WinRAR must restore the stream our CLI stored"
        );
    }

    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_cli_os.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-os", "-idq"])
            .arg(&theirs)
            .arg("ads.bin")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -os failed:\n{out}");
        let out_dir = dir.path().join("ours_cli_out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
            .args(["x", "-os", "-idq", "--dest"])
            .arg(&out_dir)
            .arg(&theirs));
        assert!(ok, "our CLI x -os failed:\n{out}");
        assert_eq!(
            std::fs::read(format!(
                "{}{}",
                out_dir.join("ads.bin").display(),
                stream_name
            ))
            .unwrap(),
            stream_data,
            "our CLI must restore WinRAR's stream"
        );
    }
}

/// `-p` streams: our writer encrypts the "STM" payload with a plaintext
/// CRC32 (matching WinRAR), WinRAR restores it, and WinRAR's encrypted
/// streams decode through our reader.
#[cfg(windows)]
#[test]
fn os_streams_password_interop_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("ads.bin");
    std::fs::write(&src, b"main stream data").unwrap();
    let stream_name = ":secret";
    let stream_data = b"encrypted alternate payload".to_vec();
    std::fs::write(format!("{}{}", src.display(), stream_name), &stream_data).unwrap();

    let ours = dir.path().join("ours_os_p.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &ours,
            WriterOptions::default().password("pw").save_streams(true),
        )
        .unwrap();
        rar.add_path(&src, EntryWriteOptions::new()).unwrap();
        rar.finish().unwrap();
    }
    if let Some(unrar) = unrar_bin() {
        let win = dir.path().join("win_os_p");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-os", "-ppw", "-y", "-idq"])
            .arg(&ours)
            .arg(&win));
        assert!(ok, "UnRAR x -os -ppw failed on our archive:\n{out}");
        assert_eq!(
            std::fs::read(format!("{}{}", win.join("ads.bin").display(), stream_name)).unwrap(),
            stream_data,
            "WinRAR must restore our encrypted stream"
        );
    }

    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_os_p.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-os", "-ppw", "-idq"])
            .arg(&theirs)
            .arg("ads.bin")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR a -os -ppw failed:\n{out}");
        let out_dir = dir.path().join("ours_os_p_out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut ar = ArchiveReader::open_with(&theirs, OpenOptions::new().password("pw")).unwrap();
        ar.extract_all_with_options(&out_dir, ExtractOptions::default())
            .unwrap();
        assert_eq!(
            std::fs::read(format!(
                "{}{}",
                out_dir.join("ads.bin").display(),
                stream_name
            ))
            .unwrap(),
            stream_data,
            "we must restore WinRAR's encrypted stream"
        );
    }
}

/// `-om` propagation matches WinRAR for the same archive Mark of the Web
/// (security zone only by default; `-om1` copies every field).
#[cfg(windows)]
#[test]
fn om_mark_of_the_web_matches_winrar() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"body").unwrap();
    let archive = dir.path().join("motw.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path()));
    assert!(ok, "create failed:\n{out}");
    let motw: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/p\r\nHostUrl=https://example.com/f\r\n";
    std::fs::write(format!("{}{}", archive.display(), ":Zone.Identifier"), motw).unwrap();

    if let Some(unrar) = unrar_bin() {
        for (switch, name) in [("-om", "win_zone"), ("-om1", "win_full")] {
            let win = dir.path().join(name);
            let ours = dir.path().join(format!("ours_{name}"));
            std::fs::create_dir_all(&win).unwrap();
            std::fs::create_dir_all(&ours).unwrap();
            let (ok, out) = run(Command::new(&unrar)
                .args(["x", switch, "-y", "-idq"])
                .arg(&archive)
                .arg(&win));
            assert!(ok, "UnRAR x {switch} failed:\n{out}");
            let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
                .args(["x", switch, "-idq", "--dest"])
                .arg(&ours)
                .arg(&archive));
            assert!(ok, "our x {switch} failed:\n{out}");
            let theirs = std::fs::read(format!(
                "{}{}",
                win.join("f.txt").display(),
                ":Zone.Identifier"
            ))
            .unwrap();
            let ours = std::fs::read(format!(
                "{}{}",
                ours.join("f.txt").display(),
                ":Zone.Identifier"
            ))
            .unwrap();
            assert_eq!(ours, theirs, "our {switch} filter must match WinRAR's");
        }
    }
}
