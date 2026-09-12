use rar_rs::{ArchiveWriter, CompressionLevel, EntryWriteOptions, SolidMode, WriterOptions};

use crate::support::{
    STREAM_SIZE, file_sha256, opts_password, streamed_compressed_case, temp_dir, unrar_bin,
    unrar_extract, unrar_test, write_pattern_file,
};

/// Single-volume compressed streaming (spill-file path).
#[test]
fn winrar_validates_streamed_compressed_single_volume() {
    streamed_compressed_case(WriterOptions::default(), None, 3);
}

/// Multi-volume compressed streaming (chunk splits mid-stream).
#[test]
fn winrar_validates_streamed_compressed_multivolume() {
    streamed_compressed_case(
        WriterOptions::default().volume_size(16 * 1024 * 1024),
        None,
        3,
    );
}

/// Encrypted streaming (single-volume, chained CBC).
#[test]
fn winrar_validates_streamed_encrypted() {
    streamed_compressed_case(
        WriterOptions::default().password("s3cret"),
        Some("s3cret"),
        3,
    );
}

/// Encrypted streaming multi-volume: per-chunk ciphertext CRCs and
/// per-chunk encryption records.
#[test]
fn winrar_validates_streamed_encrypted_multivolume() {
    streamed_compressed_case(
        WriterOptions::default()
            .password("s3cret")
            .volume_size(16 * 1024 * 1024),
        Some("s3cret"),
        3,
    );
}

/// Header-encrypted multi-volume + STORE (level 0): exercises the on-disk
/// header accounting in the streaming writer.
#[test]
fn winrar_validates_streamed_hp_store_multivolume() {
    streamed_compressed_case(
        WriterOptions::default()
            .password("s3cret")
            .encrypt_headers(true)
            .volume_size(16 * 1024 * 1024),
        Some("s3cret"),
        0,
    );
}

/// Header-encrypted multi-volume + compressed.
#[test]
fn winrar_validates_streamed_hp_compressed_multivolume() {
    streamed_compressed_case(
        WriterOptions::default()
            .password("s3cret")
            .encrypt_headers(true)
            .volume_size(16 * 1024 * 1024),
        Some("s3cret"),
        3,
    );
}

#[test]
fn winrar_validates_streamed_solid_archive() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let _ = unrar;
    let dir = temp_dir();
    // Solid chain with a > threshold member: encoder state must carry
    // across chunks and across members.
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    write_pattern_file(&a, STREAM_SIZE, 5);
    write_pattern_file(&b, STREAM_SIZE, 7);
    let arc = dir.path().join("solid.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .solid_mode(SolidMode::Continuous)
                .blake2(true)
                .quick_open(true),
        )
        .unwrap();
        rar.add_path(
            &a,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.add_path(
            &b,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let (ok, out) = unrar_test(&arc, None);
    assert!(ok, "WinRAR rejected the solid streaming archive:\n{out}");
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, None);
    assert!(
        ok,
        "WinRAR failed to extract the solid streaming archive:\n{out}"
    );
    assert_eq!(file_sha256(&dest.join("a.bin")), file_sha256(&a));
    assert_eq!(file_sha256(&dest.join("b.bin")), file_sha256(&b));
}

/// Volumes must be byte-exact (`volume_size`, except the last), matching
/// WinRAR's own behavior, for both plain and header-encrypted streaming
/// members.
#[test]
fn streamed_volumes_are_byte_exact() {
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, STREAM_SIZE, 9);
    let vol_size = 16 * 1024 * 1024;

    for (name, opts) in [
        ("plain.rar", WriterOptions::default().volume_size(vol_size)),
        (
            "hp.rar",
            WriterOptions::default()
                .password("pw")
                .encrypt_headers(true)
                .volume_size(vol_size),
        ),
        (
            "enc.rar",
            WriterOptions::default()
                .password("pw")
                .volume_size(vol_size),
        ),
    ] {
        let arc = dir.path().join(name);
        {
            let mut rar = ArchiveWriter::create_with(&arc, opts).unwrap();
            // STORE: the compressible pattern would fit one volume; stored
            // raw it actually fills the volumes.
            rar.add_path(
                &src,
                EntryWriteOptions::new()
                    .compression_level(CompressionLevel::try_from(0u8).unwrap()),
            )
            .unwrap();
            rar.finish().unwrap();
        }
        let volumes = rar_rs::discover_volumes(&arc);
        assert!(volumes.len() > 2, "{name}: expected several volumes");
        for vol in &volumes[..volumes.len() - 1] {
            let len = std::fs::metadata(vol).unwrap().len();
            assert_eq!(
                len,
                vol_size,
                "{name}: non-final volume {} must be exactly {vol_size} bytes",
                vol.display()
            );
        }
        // Everything must still extract byte-identically (WinRAR-gated).
        if unrar_bin().is_some() {
            let password = opts_password(name);
            let (ok, out) = unrar_test(&volumes[0], password);
            assert!(ok, "WinRAR rejected {name}:\n{out}");
        }
    }
}
