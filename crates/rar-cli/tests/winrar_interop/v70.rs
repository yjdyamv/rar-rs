use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, DictionarySize,
    EntryWriteOptions, ExtractOptions, WriterOptions,
};

use crate::support::{
    file_sha256, rar_bin, run, temp_dir, unrar_bin, unrar_extract, unrar_test, write_pattern_file,
};

// ── RAR7 (v70) archives: dictionary > 4 GiB ────────────────────────────────

/// WinRAR switches to the RAR7 compression algorithm (v70) when the
/// dictionary exceeds 4 GiB (here: `-md8g` with a >4 GiB source). We must
/// refuse such members by default (WinRAR's 4 GiB dictionary cap) and
/// decode them byte-identically once `-mdx` raises the cap.
#[test]
#[ignore] // slow: >4 GiB source and an 8 GiB dictionary window
fn rar7_v70_archives_decode_with_mdx() {
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    let size = 4 * 1024 * 1024 * 1024u64 + 4096; // > 4 GiB triggers v70 with -md8g
    write_pattern_file(&src, size, 3);

    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let arc = dir.path().join("v70.rar");
    let (ok, out) = run(Command::new(&rar)
        .args(["a", "-md8g", "-m3", "-idq"])
        .arg(&arc)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR -md8g failed:\n{out}");
    // Confirm the member really is v70 with a >4 GiB dictionary (WinRAR
    // encodes the exact size, possibly non-power-of-two).
    {
        let ar = ArchiveReader::open(&arc).unwrap();
        let name = ar.entries().next().unwrap().name().to_string();
        let e = ar.entry(ar.unique_entry(&name).unwrap()).unwrap();
        assert_eq!(e.comp_version(), 1, "expected RAR7 (v70) member");
        let bytes = e.dict_size_bytes().expect("v70 must carry the byte count");
        assert!(
            bytes > 4 * 1024 * 1024 * 1024,
            "expected a >4 GiB dictionary, got {bytes}"
        );
    }

    // Default extraction cap (4 GiB dictionary) refuses it (unpacked-size
    // limits raised so the dictionary cap is the one that trips).
    let out_dir = dir.path().join("out_default");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut ar = ArchiveReader::open(&arc).unwrap();
    let err = ar
        .extract_all_with_options(
            &out_dir,
            ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("dictionary size"),
        "default cap must refuse the >4 GiB dictionary, got: {err}"
    );

    // -mdx semantics: raising the cap decodes it byte-identically.
    let out_dir = dir.path().join("out_mdx");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut ar = ArchiveReader::open(&arc).unwrap();
    ar.extract_all_with_options(
        &out_dir,
        ExtractOptions {
            max_unpacked_bytes: None,
            max_total_unpacked_bytes: None,
            max_dict_size: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(file_sha256(&out_dir.join("big.bin")), file_sha256(&src));
}

/// We create RAR7 (v70) archives ourselves: `-md8g` with a >4 GiB source
/// selects the v70 header (compression version 1) with the dictionary
/// capped at 2x the file size (8 GiB here), and the member payload is
/// encoded with the extended 80-entry distance table. Both our extractor
/// and WinRAR's UnRAR must decode it byte-identically.
#[test]
#[ignore] // slow: >4 GiB source and an 8 GiB dictionary window
fn we_create_v70_archives_decode_everywhere() {
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    let size = 4 * 1024 * 1024 * 1024u64 + 4096; // > 4 GiB triggers v70 with -md8g
    write_pattern_file(&src, size, 7);

    // Create with our rar CLI (relative member name, like WinRAR).
    let arc = dir.path().join("v70.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-md8g", "-m3", "-idq"])
        .arg(&arc)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -md8g failed:\n{out}");

    // Confirm the member is v70 with a >4 GiB dictionary.
    {
        let ar = ArchiveReader::open(&arc).unwrap();
        let name = ar.entries().next().unwrap().name().to_string();
        let e = ar.entry(ar.unique_entry(&name).unwrap()).unwrap();
        assert_eq!(e.comp_version(), 1, "expected RAR7 (v70) member");
        let bytes = e.dict_size_bytes().expect("v70 must carry the byte count");
        assert!(
            bytes > 4 * 1024 * 1024 * 1024,
            "expected a >4 GiB dictionary, got {bytes}"
        );
    }

    // Our extractor decodes it byte-identically.
    let out_dir = dir.path().join("out_ours");
    std::fs::create_dir_all(&out_dir).unwrap();
    {
        let mut ar = ArchiveReader::open(&arc).unwrap();
        ar.extract_all_with_options(
            &out_dir,
            ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                max_dict_size: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(file_sha256(&out_dir.join("big.bin")), file_sha256(&src));

    // WinRAR's UnRAR decodes it byte-identically too (it needs `-mdx8g`
    // to allow the >4 GiB dictionary).
    if let Some(unrar) = unrar_bin() {
        let out_dir = dir.path().join("out_unrar");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(unrar)
            .args(["x", "-idq", "-o+", "-y", "-mdx8g"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR -mdx8g failed:\n{out}");
        assert_eq!(file_sha256(&out_dir.join("big.bin")), file_sha256(&src));
    }
}

/// The `compression(V70)` seam writes legal RAR7 (v70) archives at small
/// scale (the real trigger needs a > 4 GiB source). WinRAR must test and
/// extract them byte-identically — no `-mdx` needed below the 4 GiB cap.
#[test]
fn our_small_dict_v70_archives_decode_with_winrar() {
    let Some(_unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("v70.bin");
    write_pattern_file(&src, 4 * 1024 * 1024, 17);

    let arc = dir.path().join("v70.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .dictionary_size(DictionarySize::try_from(8 * 1024 * 1024).unwrap())
                .compression(ArchiveVersion::V70),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    // Confirm the member really is v70.
    let ar = ArchiveReader::open(&arc).unwrap();
    let name = ar
        .entries()
        .find(|e| e.name().ends_with("v70.bin"))
        .unwrap()
        .name()
        .to_string();
    let e = ar.entry(ar.unique_entry(&name).unwrap()).unwrap();
    assert_eq!(e.comp_version(), 1, "expected a v70 member");
    assert_eq!(e.dict_size_bytes(), Some(8 * 1024 * 1024));

    // WinRAR tests and extracts it byte-identically.
    let (ok, out) = unrar_test(&arc, None);
    assert!(ok, "UnRAR rejected our small-dict v70 archive:\n{out}");
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, None);
    assert!(ok, "UnRAR failed to extract our v70 archive:\n{out}");
    assert_eq!(
        file_sha256(&dest.join(&name)),
        file_sha256(&src),
        "WinRAR extracted different bytes from our v70 archive"
    );
}

/// `rar a -ma7` (our extension: force RAR7/v70 at any dictionary size)
/// must produce archives WinRAR tests and extracts byte-identically.
#[test]
fn cli_ma7_archives_decode_with_winrar() {
    let Some(_unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("ma7.bin");
    write_pattern_file(&src, 4 * 1024 * 1024, 19);
    let arc = dir.path().join("ma7.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma7", "-idq"])
        .arg(&arc)
        .arg("ma7.bin")
        .current_dir(dir.path()));
    assert!(ok, "rar a -ma7 failed:\n{out}");
    // The member really is v70 (per-member 2x-file cap floors the dict).
    let ar = ArchiveReader::open(&arc).unwrap();
    let e = ar.entry(ar.unique_entry("ma7.bin").unwrap()).unwrap();
    assert_eq!(e.comp_version(), 1, "-ma7 must force v70");
    let (ok, out) = unrar_test(&arc, None);
    assert!(ok, "UnRAR rejected our -ma7 archive:\n{out}");
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, None);
    assert!(ok, "UnRAR failed to extract our -ma7 archive:\n{out}");
    assert_eq!(
        file_sha256(&dest.join("ma7.bin")),
        file_sha256(&src),
        "WinRAR extracted different bytes"
    );
}

/// RAR5 (v50) vs RAR7 (v70) for the *same* data: both must decode
/// byte-for-byte with WinRAR, and the v70 member must carry `comp_version`
/// 1. This is the byte-level guarantee behind the "RAR5 vs RAR7" parity claim.
#[test]
fn rar5_vs_rar7_same_data_decode_everywhere() {
    let Some(_unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("cmp.bin");
    write_pattern_file(&src, 6 * 1024 * 1024, 23);

    // RAR5 (default).
    let v50 = dir.path().join("v50.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-idq"])
        .arg(&v50)
        .arg("cmp.bin")
        .current_dir(dir.path()));
    assert!(ok, "rar a (v50) failed:\n{out}");
    // RAR7 (v70) forced at small scale.
    let v70 = dir.path().join("v70.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma7", "-idq"])
        .arg(&v70)
        .arg("cmp.bin")
        .current_dir(dir.path()));
    assert!(ok, "rar a -ma7 failed:\n{out}");

    let e50 = {
        let mut ar = ArchiveReader::open(&v50).unwrap();
        let n = ar.entries().next().unwrap().name().to_string();
        let e = ar.entry(ar.unique_entry(&n).unwrap()).unwrap();
        assert_eq!(e.comp_version(), 0, "v50 must stay comp_version 0");
        ar.read_entry(ar.unique_entry(&n).unwrap()).unwrap()
    };
    let e70 = {
        let mut ar = ArchiveReader::open(&v70).unwrap();
        let n = ar.entries().next().unwrap().name().to_string();
        let e = ar.entry(ar.unique_entry(&n).unwrap()).unwrap();
        assert_eq!(e.comp_version(), 1, "v70 must be comp_version 1");
        ar.read_entry(ar.unique_entry(&n).unwrap()).unwrap()
    };
    // Both encode the same source; decoded bytes must match the source.
    assert_eq!(e50, std::fs::read(&src).unwrap(), "v50 decoded mismatch");
    assert_eq!(e70, std::fs::read(&src).unwrap(), "v70 decoded mismatch");

    // WinRAR must decode both byte-for-byte.
    let v50_out = dir.path().join("out_v50");
    let v70_out = dir.path().join("out_v70");
    std::fs::create_dir_all(&v50_out).unwrap();
    std::fs::create_dir_all(&v70_out).unwrap();
    for (arc, dest) in [(&v50, &v50_out), (&v70, &v70_out)] {
        let (ok, out) = unrar_extract(arc, dest, None);
        assert!(ok, "UnRAR failed on {arc:?}:\n{out}");
        // The member is stored flat as `cmp.bin`, so it extracts directly.
        let extracted = dest.join("cmp.bin");
        assert_eq!(
            file_sha256(&extracted),
            file_sha256(&src),
            "WinRAR extracted different bytes from {arc:?}"
        );
    }
}
