use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, ExtractOptions, OpenOptions,
    WriterOptions,
};

use crate::support::{create_sparse, file_sha256, temp_dir, unrar_bin, unrar_extract, unrar_test};

#[test]
#[ignore = "slow: compresses >4 GiB and needs >4 GiB of temp space; the 512 MiB sibling runs in the default suite (crates/rar/tests/large_paths.rs)"]
fn huge_sparse_file_streamed_compression_roundtrips() {
    let dir = temp_dir();
    let size = 4 * 1024 * 1024 * 1024u64 + 4096; // > 4 GiB
    let src = dir.path().join("huge.bin");
    create_sparse(&src, size);

    // rar-rs creates a compressed single-volume archive (the all-zero
    // input compresses to a few MiB, but the encoder must stream all
    // 4 GiB through the spill file).
    let arc = dir.path().join("huge.rar");
    {
        let mut rar = ArchiveWriter::create_with(&arc, WriterOptions::default()).unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    assert!(
        std::fs::metadata(&arc).unwrap().len() < 64 * 1024 * 1024,
        "all-zero input must compress well"
    );

    // rar-rs round-trip: streamed extraction to disk (raise the default
    // per-member limit; extraction itself is streaming).
    let ours = dir.path().join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    {
        let mut rar = ArchiveReader::open(&arc).unwrap();
        rar.extract_entry_with_options(
            rar.unique_entry("huge.bin").unwrap(),
            &ours,
            ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    let meta = std::fs::metadata(ours.join("huge.bin")).unwrap();
    assert_eq!(meta.len(), size, "extracted size");
    assert_eq!(file_sha256(&ours.join("huge.bin")), file_sha256(&src));

    // WinRAR must test and extract it too.
    if unrar_bin().is_some() {
        let (ok, out) = unrar_test(&arc, None);
        assert!(ok, "WinRAR rejected the >4 GiB archive:\n{out}");
        let win = dir.path().join("win");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&arc, &win, None);
        assert!(ok, "WinRAR failed to extract the >4 GiB archive:\n{out}");
        let meta = std::fs::metadata(win.join("huge.bin")).unwrap();
        assert_eq!(meta.len(), size, "WinRAR extracted size");
        assert_eq!(
            file_sha256(&win.join("huge.bin")),
            file_sha256(&src),
            "WinRAR extracted different bytes"
        );
    }
}

#[test]
#[ignore = "slow: stores >4 GiB; the 256 MiB encrypted multi-volume sibling runs in the default suite (crates/rar/tests/large_paths.rs)"]
fn huge_sparse_file_streamed_encrypted_multivolume_roundtrips() {
    let dir = temp_dir();
    let size = 4 * 1024 * 1024 * 1024u64 + 8192; // > 4 GiB
    let src = dir.path().join("huge.bin");
    create_sparse(&src, size);

    // STORE (level 0): the all-zero input compresses to a few MiB and
    // would fit a single volume; stored raw it actually spans the volume
    // set. The compressed >4 GiB case is covered by the single-volume
    // test; this one exercises the streaming encrypted multi-volume path
    // (per-chunk ciphertext CRCs, per-chunk encryption records, CBC chain
    // across volume boundaries).
    let arc = dir.path().join("huge.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .password("s3cret")
                .volume_size(256 * 1024 * 1024),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    // Many exact-sized volumes; each carries a ciphertext CRC and an
    // encryption record on every chunk.
    let volumes = rar_rs::discover_volumes(&arc);
    assert!(
        volumes.len() >= 4,
        "expected several volumes, got {}",
        volumes.len()
    );
    for vol in &volumes[..volumes.len() - 1] {
        assert_eq!(
            std::fs::metadata(vol).unwrap().len(),
            256 * 1024 * 1024,
            "non-final volume must be byte-exact"
        );
    }

    // rar-rs self round-trip (streamed extraction, raised limits).
    let ours = dir.path().join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    {
        let mut rar =
            ArchiveReader::open_with(&arc, OpenOptions::new().password("s3cret")).unwrap();
        rar.extract_entry_with_options(
            rar.unique_entry("huge.bin").unwrap(),
            &ours,
            ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::metadata(ours.join("huge.bin")).unwrap().len(),
        size
    );
    assert_eq!(file_sha256(&ours.join("huge.bin")), file_sha256(&src));

    // WinRAR test + extract.
    if unrar_bin().is_some() {
        let (ok, out) = unrar_test(&volumes[0], Some("s3cret"));
        assert!(
            ok,
            "WinRAR rejected the >4 GiB encrypted volume set:\n{out}"
        );
        let win = dir.path().join("win");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, Some("s3cret"));
        assert!(ok, "WinRAR failed to extract the >4 GiB volume set:\n{out}");
        assert_eq!(std::fs::metadata(win.join("huge.bin")).unwrap().len(), size);
        assert_eq!(
            file_sha256(&win.join("huge.bin")),
            file_sha256(&src),
            "WinRAR extracted different bytes"
        );
    }
}

/// RAR5 creation of a >4 GiB single file: an all-zero source compresses to a few
/// MiB but the encoder must stream all 4 GiB through the spill file. WinRAR
/// must test and extract byte-for-byte. `#[ignore]`d: needs >4 GiB of temp
/// space and a few minutes.
#[test]
#[ignore = "slow: compresses >4 GiB and needs >4 GiB of temp space"]
fn rar5_huge_single_file_decodes_with_winrar() {
    let dir = temp_dir();
    let size = 4 * 1024 * 1024 * 1024u64 + 4096; // > 4 GiB
    let src = dir.path().join("huge.bin");
    create_sparse(&src, size);

    let arc = dir.path().join("huge.rar");
    {
        let mut rar = ArchiveWriter::create_with(&arc, WriterOptions::default()).unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    assert!(
        std::fs::metadata(&arc).unwrap().len() < 64 * 1024 * 1024,
        "all-zero input must compress well"
    );

    // rar-rs round-trip (streamed extraction, raised limits).
    let ours = dir.path().join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    {
        let mut rar = ArchiveReader::open(&arc).unwrap();
        rar.extract_entry_with_options(
            rar.unique_entry("huge.bin").unwrap(),
            &ours,
            ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::metadata(ours.join("huge.bin")).unwrap().len(),
        size
    );
    assert_eq!(file_sha256(&ours.join("huge.bin")), file_sha256(&src));

    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&arc, None);
        assert!(ok, "WinRAR rejected the >4 GiB archive:\n{out}");
        let win = dir.path().join("win");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&arc, &win, None);
        assert!(ok, "WinRAR failed to extract the >4 GiB archive:\n{out}");
        assert_eq!(std::fs::metadata(win.join("huge.bin")).unwrap().len(), size);
        assert_eq!(
            file_sha256(&win.join("huge.bin")),
            file_sha256(&src),
            "WinRAR extracted different bytes"
        );
    }
}
