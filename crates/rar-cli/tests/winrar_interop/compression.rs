use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, DictionarySize, EntryWriteOptions,
    ExtractOptions, WriterOptions,
};

use crate::support::{
    file_sha256, rar_bin, run, temp_dir, unrar_bin, unrar_extract, unrar_test, write_rep_text,
};

/// `-md` dictionaries interoperate with WinRAR in both directions.
#[test]
fn dictionary_size_md_interops_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("rep32t.bin");
    write_rep_text(&src, 32 * 1024 * 1024);

    // Our -md64m archive (dict log 9): WinRAR must test and extract it.
    let ours = dir.path().join("ours_md.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &ours,
            WriterOptions::default().dictionary_size(DictionarySize::from_rar5_log(9).unwrap()),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let ar = ArchiveReader::open(&ours).unwrap();
    let entry = ar.entry(ar.unique_entry("rep32t.bin").unwrap()).unwrap();
    assert_eq!(entry.comp_dict_size(), 9);
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&ours, None);
        assert!(ok, "WinRAR rejected our -md64m archive:\n{out}");
        let win = dir.path().join("win_ours");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&ours, &win, None);
        assert!(ok, "WinRAR failed to extract our -md64m archive:\n{out}");
        assert_eq!(file_sha256(&win.join("rep32t.bin")), file_sha256(&src));
    }

    // WinRAR's -md64m archive: we must read it back byte-identically.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_md.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-md64m", "-idq"])
            .arg(&theirs)
            .arg(&src));
        assert!(ok, "WinRAR -md64m failed:\n{out}");
        let ar = ArchiveReader::open(&theirs).unwrap();
        // WinRAR may store the member under a path-derived name; find it.
        let name = ar.entries().next().unwrap().name().to_string();
        let entry = ar.entry(ar.unique_entry(&name).unwrap()).unwrap();
        assert_eq!(
            entry.comp_dict_size(),
            9,
            "WinRAR -md64m should record a 64 MiB dictionary"
        );
        let mut ar = ArchiveReader::open(&theirs).unwrap();
        let data = ar.read_entry(ar.unique_entry(&name).unwrap()).unwrap();
        assert_eq!(data, std::fs::read(&src).unwrap());
    }
}

/// Long-range matching (WinRAR `-mcl`, applied automatically for
/// -m2..-m5): a 128 MiB file whose second half copies its random first
/// half must compress almost as well as WinRAR's archive (the 64 MiB
/// match distance is far beyond the near window) and decode everywhere.
/// Correctness at reduced scale is locked into the default suite
/// (`crates/rar/tests/large_paths.rs`); this one gates the compression
/// ratio against WinRAR itself.
#[test]
#[ignore] // slow: 128 MiB source, compression + two extractions
fn long_range_matches_winrar_compression_ratio() {
    let dir = temp_dir();
    let src = dir.path().join("pair.bin");
    let half = 64 * 1024 * 1024usize;
    let mut data = vec![0u8; half * 2];
    {
        // Deterministic pseudo-random first half (LCG).
        let mut state = 42u64;
        for b in data[..half].iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (state >> 33) as u8;
        }
        let first = data[..half].to_vec();
        data[half..].copy_from_slice(&first);
    }
    std::fs::write(&src, &data).unwrap();

    // WinRAR reference archive (-md128m, long range search on by default).
    let win_arc = dir.path().join("win.rar");
    let (ok, out) = run(Command::new(rar_bin().expect("rar"))
        .args(["a", "-md128m", "-m3", "-idq"])
        .arg(&win_arc)
        .arg("pair.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR failed:\n{out}");
    let win_size = std::fs::metadata(&win_arc).unwrap().len();

    // Our archive.
    let our_arc = dir.path().join("ours.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-md128m", "-m3", "-idq"])
        .arg(&our_arc)
        .arg("pair.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar failed:\n{out}");
    let our_size = std::fs::metadata(&our_arc).unwrap().len();

    // Compression ratio must be close to WinRAR's (the 64 MiB distant
    // copy compresses); allow 5% slack for sampling-grid effects.
    assert!(
        our_size <= win_size * 105 / 100,
        "long-range ratio too far from WinRAR: ours {our_size} vs WinRAR {win_size}"
    );

    // Our extractor round-trips it.
    let out_dir = dir.path().join("out_ours");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut ar = ArchiveReader::open(&our_arc).unwrap();
    ar.extract_all_with_options(
        &out_dir,
        ExtractOptions {
            max_unpacked_bytes: None,
            max_total_unpacked_bytes: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(file_sha256(&out_dir.join("pair.bin")), file_sha256(&src));

    // WinRAR's UnRAR decodes it byte-identically too.
    if let Some(unrar) = unrar_bin() {
        let out_dir = dir.path().join("out_unrar");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&our_arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR failed:\n{out}");
        assert_eq!(file_sha256(&out_dir.join("pair.bin")), file_sha256(&src));
    }
}
