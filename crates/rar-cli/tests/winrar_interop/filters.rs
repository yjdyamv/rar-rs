use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, OpenOptions, WriterOptions,
};

use crate::support::{
    file_sha256, rar_bin, run, temp_dir, unrar_bin, unrar_extract, unrar_test,
    write_correlated_pcm, write_wav,
};

#[test]
fn unrar_reads_our_delta_filtered_output() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let _ = unrar;
    let dir = temp_dir();
    let src = dir.path().join("audio.bin");
    // 16-bit stereo interleaved correlated PCM — our auto-delta detector
    // should select channels=2 and emit a delta-filtered (non-solid) member.
    write_correlated_pcm(&src, 2, 120_000);

    let arc = dir.path().join("delta.rar");
    {
        let mut rar = ArchiveWriter::create_with(&arc, WriterOptions::default()).unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }

    // The real UnRAR must accept and verify our delta-filtered archive.
    let (ok, out) = unrar_test(&arc, None);
    assert!(ok, "UnRAR rejected our delta-filtered archive:\n{out}");

    // And extract it byte-for-byte identical to the source.
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, None);
    assert!(
        ok,
        "UnRAR failed to extract our delta-filtered archive:\n{out}"
    );
    assert_eq!(
        file_sha256(&dest.join("audio.bin")),
        file_sha256(&src),
        "UnRAR extracted different bytes from our delta-filtered archive"
    );

    // rar-rs must read its own delta output back too.
    let ours = dir.path().join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    let mut rar = ArchiveReader::open(&arc).unwrap();
    rar.extract_entry(rar.unique_entry("audio.bin").unwrap(), &ours)
        .unwrap();
    assert_eq!(
        file_sha256(&ours.join("audio.bin")),
        file_sha256(&src),
        "rar-rs round-trip mismatch for our delta-filtered archive"
    );
}

#[test]
fn rar_rs_reads_winrar_delta_filtered_wav() {
    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let _ = rar;
    let dir = temp_dir();
    let src = dir.path().join("sample.wav");
    // Real WinRAR applies its delta (audio) filter to WAV PCM by default.
    write_wav(&src, 2, 120_000);

    let arc = dir.path().join("winrar-delta.rar");
    let (ok, out) = run(Command::new(rar_bin().unwrap())
        .arg("a")
        .arg("-m5")
        .arg("-idq")
        .arg(&arc)
        .arg("sample.wav")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR failed to create the archive:\n{out}");

    // Our reader must extract WinRAR's delta-filtered WAV byte-for-byte. Look
    // the member up by suffix because WinRAR may store a path prefix.
    let mut r = ArchiveReader::open(&arc).unwrap();
    let names: Vec<String> = r.entries().map(|e| e.name().to_string()).collect();
    let member = names
        .iter()
        .find(|n| n.ends_with("sample.wav"))
        .unwrap_or_else(|| panic!("sample.wav not found in {names:?}"))
        .clone();
    let data = r.read_entry(r.unique_entry(&member).unwrap()).unwrap();
    assert_eq!(
        data,
        std::fs::read(&src).unwrap(),
        "rar-rs read a different WAV than WinRAR archived"
    );
}

// ── Phase 2.2: extended interaction matrix ───────────────────────────────
//
// Combinations called out as "untested but cheap to expose real byte-level
// deviations": filter + encrypted header, filter + multi-volume, RAR5 vs
// RAR7, recovery record + encryption, symlinks / ADS streams, >4 GiB single
// file, and the solid + filter boundary. Every direction is gated on a real
// WinRAR install so the default `cargo test` suite still runs anywhere.

/// Filter (delta/x86) + encrypted header (`-hp`): both directions.
///
/// WinRAR creates a delta-filtered WAV under a `-hp` archive; rar-rs must
/// decrypt the headers and decode the delta filter byte-for-byte. rar-rs
/// creates a delta-filtered member under header encryption; WinRAR's
/// `UnRAR` must test and extract it byte-for-byte (it needs the password).
#[test]
fn filtered_member_with_encrypted_header_interops() {
    let dir = temp_dir();
    let src = dir.path().join("audio.wav");
    write_wav(&src, 2, 120_000);

    // WinRAR -> ours (header-encrypted, delta-filtered).
    if let Some(rar) = rar_bin() {
        let arc = dir.path().join("win_hp.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-hpsecret", "-m5", "-idq"])
            .arg(&arc)
            .arg("audio.wav")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -hp delta failed:\n{out}");
        let mut ar = ArchiveReader::open_with(&arc, OpenOptions::new().password("secret")).unwrap();
        let name = ar.entries().next().unwrap().name().to_string();
        assert_eq!(
            ar.read_entry(ar.unique_entry(&name).unwrap()).unwrap(),
            std::fs::read(&src).unwrap(),
            "rar-rs read a different WAV from WinRAR's -hp archive"
        );
    }

    // Ours -> WinRAR (header-encrypted, auto-delta-filtered).
    let arc = dir.path().join("ours_hp.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .password("secret")
                .encrypt_headers(true),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(5u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&arc, Some("secret"));
        assert!(ok, "UnRAR rejected our -hp delta archive:\n{out}");
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&arc, &dest, Some("secret"));
        assert!(ok, "UnRAR failed to extract our -hp delta archive:\n{out}");
        assert_eq!(
            file_sha256(&dest.join("audio.wav")),
            file_sha256(&src),
            "WinRAR extracted different bytes from our -hp delta archive"
        );
    }
}

/// Filter (delta/x86) + multi-volume (`-v`): both directions.
#[test]
fn filtered_member_with_multivolume_interops() {
    let dir = temp_dir();
    // A 20 MiB correlated-PCM WAV: compresses hard (delta filter) and spans
    // several 4 MiB volumes, exercising the filter + volume-boundary path.
    let src = dir.path().join("big.wav");
    write_wav(&src, 2, 2_500_000);
    let vol_size = 4 * 1024 * 1024;

    // WinRAR -> ours (delta-filtered, multi-volume).
    if let Some(rar) = rar_bin() {
        let arc = dir.path().join("win_fv.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-m5", "-v4m", "-idq"])
            .arg(&arc)
            .arg("big.wav")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -v delta failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&arc);
        let mut ar = ArchiveReader::open(&volumes[0]).unwrap();
        let name = ar.entries().next().unwrap().name().to_string();
        assert_eq!(
            ar.read_entry(ar.unique_entry(&name).unwrap()).unwrap(),
            std::fs::read(&src).unwrap(),
            "rar-rs read a different WAV from WinRAR's multi-volume delta archive"
        );
    }

    // Ours -> WinRAR (auto-delta-filtered, multi-volume).
    let arc = dir.path().join("ours_fv.rar");
    {
        let mut rar =
            ArchiveWriter::create_with(&arc, WriterOptions::default().volume_size(vol_size))
                .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(5u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&arc);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected our multi-volume delta archive:\n{out}");
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &dest, None);
        assert!(
            ok,
            "UnRAR failed to extract our multi-volume delta archive:\n{out}"
        );
        assert_eq!(
            file_sha256(&dest.join("big.wav")),
            file_sha256(&src),
            "WinRAR extracted different bytes from our multi-volume delta archive"
        );
    }
}
