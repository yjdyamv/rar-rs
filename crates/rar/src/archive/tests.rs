use super::*;
use crate::fs::safe_path::sanitize_archive_path;

fn assert_invalid_option(result: RarResult<RarArchive>) {
    match result {
        Err(RarError::InvalidOption(_)) => {}
        Err(error) => panic!("expected InvalidOption, got {error}"),
        Ok(_) => panic!("expected invalid options to be rejected"),
    }
}

#[test]
fn create_options_reject_invalid_dictionary_and_thread_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.rar");

    assert_invalid_option(RarArchive::create_with_options(
        &path,
        crate::options::CreateOptions {
            dict_size_log: Some(16),
            ..Default::default()
        },
    ));
    assert_invalid_option(RarArchive::create_with_options(
        &path,
        crate::options::CreateOptions {
            dict_size_log: Some(1),
            dict_size_bytes: Some(1024 * 1024),
            ..Default::default()
        },
    ));
    for bytes in [1, crate::options::MAX_RAR7_DICTIONARY_BYTES + 1] {
        assert_invalid_option(RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                dict_size_bytes: Some(bytes),
                ..Default::default()
            },
        ));
    }
    assert_invalid_option(RarArchive::create_with_options(
        &path,
        crate::options::CreateOptions {
            threads: Some(crate::options::MAX_COMPRESSION_THREADS + 1),
            ..Default::default()
        },
    ));
}

#[test]
fn archive_local_zero_threads_use_automatic_sizing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auto-threads.rar");
    let mut archive = RarArchive::create_with_options(
        &path,
        crate::options::CreateOptions {
            threads: Some(0),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(archive.write_ctx().compression_threads, Some(0));
    #[cfg(feature = "parallel")]
    assert_eq!(
        archive.effective_threads(),
        crate::parallel::pool_threads(4)
    );
    archive.close().unwrap();
}

#[test]
fn write_only_setters_return_errors_in_read_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.rar");
    let mut archive = RarArchive::create_with_options(&path, Default::default()).unwrap();
    archive.set_compression_threads(Some(2)).unwrap();
    archive.set_dictionary(Some(3), None).unwrap();
    archive.add_bytes("a.txt", b"a", 0).unwrap();
    archive.close().unwrap();

    let mut archive = RarArchive::open(&path).unwrap();
    assert!(matches!(
        archive.set_compression_threads(Some(2)),
        Err(RarError::InvalidState(_))
    ));
    assert!(matches!(
        archive.set_dictionary(Some(3), None),
        Err(RarError::InvalidState(_))
    ));
}

#[test]
fn setters_reject_invalid_values_without_mutating_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid-setter.rar");
    let mut archive = RarArchive::create_with_options(&path, Default::default()).unwrap();

    archive.set_compression_threads(Some(0)).unwrap();
    assert!(matches!(
        archive.set_compression_threads(Some(crate::options::MAX_COMPRESSION_THREADS + 1)),
        Err(RarError::InvalidOption(_))
    ));
    assert!(matches!(
        archive.set_dictionary(Some(16), None),
        Err(RarError::InvalidOption(_))
    ));
    assert_eq!(archive.write_ctx().compression_threads, Some(0));
    assert_eq!(archive.write_ctx().dict_size_log, None);
}

#[test]
fn encrypted_store_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    let data = b"Hello, encrypted world!";
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                password: Some("secret".into()),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("test.txt", data, 0).unwrap();
        ar.close().unwrap();
    }
    {
        let mut ar = RarArchive::open_with_password(&path, "secret").unwrap();
        assert_eq!(ar.read("test.txt").unwrap(), data);
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn encrypted_compressed_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    let data = b"Compress me! ".repeat(200);
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                password: Some("pw".into()),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("test.txt", &data, 3).unwrap();
        ar.close().unwrap();
    }
    {
        let mut ar = RarArchive::open_with_password(&path, "pw").unwrap();
        assert_eq!(ar.read("test.txt").unwrap(), data);
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn encrypted_wrong_password_fails() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                password: Some("right".into()),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("test.txt", b"data", 0).unwrap();
        ar.close().unwrap();
    }
    {
        let mut ar = RarArchive::open_with_password(&path, "wrong").unwrap();
        assert!(ar.read("test.txt").is_err());
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn encrypted_multiple_files() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                password: Some("multi".into()),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("a.txt", b"First", 0).unwrap();
        ar.add_bytes("b.txt", &b"Second ".repeat(50), 3).unwrap();
        ar.add_bytes("c.bin", &(0..=255u8).collect::<Vec<_>>(), 0)
            .unwrap();
        ar.close().unwrap();
    }
    {
        let mut ar = RarArchive::open_with_password(&path, "multi").unwrap();
        assert_eq!(ar.read("a.txt").unwrap(), b"First");
        assert_eq!(ar.read("b.txt").unwrap(), b"Second ".repeat(50));
        assert_eq!(ar.read("c.bin").unwrap(), (0..=255u8).collect::<Vec<_>>());
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn recovery_volume_count_is_capped_at_data_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("vol.part1.rar");
    let data = b"volume recovery payload ".repeat(4000); // ~100 KiB
    {
        // Ask for 10 .rev files; the archive only splits into 3
        // volumes, so exactly 3 .rev files must be produced.
        let mut ar = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                volume_size: Some(32768),
                recovery_volume_count: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("big.bin", &data, 0).unwrap();
        ar.close().unwrap();
    }
    let revs: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension().is_some_and(|e| e == "rev")).then_some(p)
        })
        .collect();
    let volumes: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension().is_some_and(|e| e == "rar")).then_some(p)
        })
        .collect();
    assert_eq!(volumes.len(), 3, "expected 3 data volumes");
    assert_eq!(
        revs.len(),
        3,
        "recovery volume count must be capped at data volumes"
    );
    std::fs::remove_dir_all(dir.path()).ok();
}

#[test]
fn recovery_volume_exact_count_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("vol.part1.rar");
    let data = b"volume recovery payload ".repeat(4000);
    {
        let mut ar = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                volume_size: Some(32768),
                recovery_volume_count: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("big.bin", &data, 0).unwrap();
        ar.close().unwrap();
    }
    let revs: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension().is_some_and(|e| e == "rev")).then_some(p)
        })
        .collect();
    assert_eq!(revs.len(), 2, "expected exactly 2 .rev files");
    std::fs::remove_dir_all(dir.path()).ok();
}

#[test]
fn recovery_volumes_roundtrip_and_repair() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("vol.part1.rar");
    let data = b"volume recovery payload ".repeat(4000); // ~100 KiB
    {
        let mut ar = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                volume_size: Some(32768),
                recovery_volumes_percent: Some(20),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("big.bin", &data, 0).unwrap();
        ar.close().unwrap();
    }
    // Volumes + at least one .rev file must exist.
    let dir_entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let mut volume_paths: Vec<_> = dir_entries
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rar"))
        .cloned()
        .collect();
    volume_paths.sort();
    let mut volumes: Vec<Vec<u8>> = volume_paths
        .iter()
        .map(|p| std::fs::read(p).unwrap())
        .collect();
    let revs: Vec<_> = dir_entries
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rev"))
        .collect();
    assert!(
        volumes.len() >= 3,
        "expected split volumes, got {}",
        volumes.len()
    );
    assert_eq!(revs.len(), 1, "expected one .rev file");

    // The .rev must carry the REV5 signature, the volume table and a
    // payload whose CRC32 matches the header field.
    let rev = std::fs::read(revs[0]).unwrap();
    assert!(rev.starts_with(b"Rar!\x1aRev"));
    let header_size = u32::from_le_bytes(rev[12..16].try_into().unwrap()) as usize;
    let body = &rev[16..16 + header_size];
    let data_count = u16::from_le_bytes(body[1..3].try_into().unwrap()) as usize;
    assert_eq!(data_count, volumes.len());
    let payload = &rev[16 + header_size..];
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(payload);
    let payload_crc = u32::from_le_bytes(body[7..11].try_into().unwrap());
    assert_eq!(hasher.finalize(), payload_crc, ".rev payload CRC mismatch");
    for (i, vol) in volumes.iter().enumerate() {
        let mut h = crc32fast::Hasher::new();
        h.update(vol);
        let table_crc =
            u32::from_le_bytes(body[11 + i * 12 + 8..11 + i * 12 + 12].try_into().unwrap());
        assert_eq!(
            h.finalize(),
            table_crc,
            ".rev volume table CRC mismatch for volume {i}"
        );
    }

    // Reconstruct a missing middle volume from the .rev parity and the
    // remaining volumes (WinRAR `rc` equivalent).
    let missing = volumes.len() / 2;
    let expected = volumes[missing].clone();
    volumes.remove(missing);

    let maxlen = volumes.iter().map(|v| v.len()).max().unwrap_or(0);
    let maxlen = if maxlen % 2 == 0 { maxlen } else { maxlen + 1 };
    let mut padded: Vec<Vec<u8>> = volumes
        .iter()
        .map(|v| {
            let mut x = v.clone();
            x.resize(maxlen, 0);
            x
        })
        .collect();
    padded.insert(missing, payload.to_vec());

    let gf = crate::recovery::rar50::shared_gf16();
    let matrix = crate::recovery::rar50::make_encoder_matrix(padded.len(), 1).unwrap();
    let mut rebuilt = vec![0u8; maxlen];
    let denom = matrix[0][missing];
    for off in (0..maxlen).step_by(2) {
        let mut symbol = 0u16;
        for (i, shard) in padded.iter().enumerate() {
            let v = u16::from_le_bytes([shard[off], shard[off + 1]]);
            if i == missing {
                // The parity shard participates with coefficient 1.
                symbol ^= v;
            } else {
                symbol ^= gf.mul(matrix[0][i], v);
            }
        }
        let v = gf.div(symbol, denom).unwrap();
        rebuilt[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    rebuilt.truncate(expected.len());
    assert_eq!(rebuilt, expected, "reconstructed volume must match");
    std::fs::remove_dir_all(dir.path()).ok();
}

#[test]
fn recovery_record_roundtrip_and_repair() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    let data = b"recovery test payload ".repeat(1000);
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                recovery_percent: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("a.bin", &data, 0).unwrap();
        ar.close().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    // The RR service header (name "RR") and the {RB} shard magic must
    // be present, and the main header must carry the recovery flag.
    assert!(raw.windows(2).any(|w| w == b"RR"));
    assert!(raw.windows(4).any(|w| w == b"{RB}"));
    // The plaintext must not be touched by the recovery record.
    let mut ar = RarArchive::open(&path).unwrap();
    assert_eq!(ar.read("a.bin").unwrap(), data);

    // Damage bytes inside ONE data shard (NR parity shards can repair
    // up to NR damaged shards; the archive here is ~21 KiB → D=21,
    // NR=1).
    let mut damaged = raw.clone();
    for pos in [100usize, 200, 300] {
        damaged[pos] ^= 0xFF;
    }
    let repaired = crate::recovery::rar50::repair_inline_recovery_archive(&damaged).unwrap();
    assert_eq!(repaired, raw, "repair must restore the original bytes");
    std::fs::remove_file(&path).ok();
}

#[test]
fn recovery_record_relocates_damaged_shards_from_twin_file_blocks() {
    // Two members with identical content pack byte-identically, so a
    // damaged shard inside one member's data block can be relocated
    // from the twin block even when the damage spans more shards than
    // the recovery record can correct (NR=1, damage covers 2 shards).
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    let data = b"twin payload for relocated repair ".repeat(1000);
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                recovery_percent: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("a.bin", &data, 0).unwrap();
        ar.add_bytes("b.bin", &data, 0).unwrap();
        ar.close().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    let m = raw
        .windows(4)
        .position(|w| w == b"{RB}")
        .expect("recovery record present");
    let ds = u16::from_le_bytes([raw[m + 0x3a], raw[m + 0x3b]]) as usize;
    let gc = u64::from_le_bytes(raw[m + 0x2a..m + 0x32].try_into().unwrap()) as usize;
    assert!(ds >= 4, "expected a multi-shard archive, got {ds}");
    let b_name = raw
        .windows(5)
        .position(|w| w == b"b.bin")
        .expect("second member header");

    // Damage two complete shards that fall inside b.bin's data block
    // (their byte-identical copies survive in a.bin's block). The last
    // shard index is chosen so both damaged shards stay inside b.bin's
    // data area and never touch file headers.
    let last = (ds - 1).min(b_name + 5 + gc / 2);
    let s1 = last.saturating_sub(2) * gc;
    let s2 = (last.saturating_sub(1)) * gc;
    assert!(s1 >= b_name, "damaged shards must sit in b.bin data block");
    let mut damaged = raw.clone();
    for byte in damaged.iter_mut().take(s2 + gc).skip(s1) {
        *byte ^= 0xFF;
    }
    let repaired = crate::recovery::rar50::repair_inline_recovery_archive(&damaged).unwrap();
    assert_eq!(
        repaired, raw,
        "relocated repair must restore the original bytes"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn recovery_record_refuses_damage_without_twin_or_parity_capacity() {
    // Distinct member contents have no twin block; two damaged shards
    // exceed the single parity shard, so repair must refuse instead of
    // writing wrong bytes.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    let a = b"alpha payload ".repeat(1000);
    let b = b"beta payload differs ".repeat(1000);
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                recovery_percent: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("a.bin", &a, 0).unwrap();
        ar.add_bytes("b.bin", &b, 0).unwrap();
        ar.close().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    let m = raw
        .windows(4)
        .position(|w| w == b"{RB}")
        .expect("recovery record present");
    let ds = u16::from_le_bytes([raw[m + 0x3a], raw[m + 0x3b]]) as usize;
    let gc = u64::from_le_bytes(raw[m + 0x2a..m + 0x32].try_into().unwrap()) as usize;
    let last = (ds - 1).min(10);
    let s1 = last.saturating_sub(2) * gc;
    let s2 = (last.saturating_sub(1)) * gc;
    let mut damaged = raw.clone();
    for byte in damaged.iter_mut().take(s2 + gc).skip(s1) {
        *byte ^= 0xFF;
    }
    assert!(
        crate::recovery::rar50::repair_inline_recovery_archive(&damaged).is_err(),
        "repair must refuse damage beyond parity capacity without a twin"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn recovery_record_with_password_and_headers() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    let data = b"encrypted + recovery ".repeat(500);
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                password: Some("pw".into()),
                encrypt_headers: true,
                recovery_percent: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("secret.bin", &data, 0).unwrap();
        ar.close().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    assert!(raw.windows(4).any(|w| w == b"{RB}"));
    let mut ar = RarArchive::open_with_password(&path, "pw").unwrap();
    assert_eq!(ar.read("secret.bin").unwrap(), data);
    std::fs::remove_file(&path).ok();
}

#[test]
fn header_encryption_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    let data = b"Hidden content!".repeat(100);
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                password: Some("hdr-pw".into()),
                encrypt_headers: true,
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("secret/name.txt", &data, 3).unwrap();
        ar.close().unwrap();
    }
    // The raw archive must not contain the plaintext file name.
    let raw = std::fs::read(&path).unwrap();
    assert!(
        !raw.windows(b"secret/name.txt".len())
            .any(|w| w == b"secret/name.txt")
    );
    {
        let mut ar = RarArchive::open_with_password(&path, "hdr-pw").unwrap();
        assert_eq!(ar.read("secret/name.txt").unwrap(), data);
    }
    // Wrong password must be rejected by the header check value.
    let err = RarArchive::open_with_password(&path, "nope").err();
    assert!(err.is_some());
    assert!(
        matches!(err, Some(RarError::WrongPassword)),
        "unexpected error: {err:?}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn header_encryption_requires_password() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().with_extension("rar");
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                password: Some("pw".into()),
                encrypt_headers: true,
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("a.txt", b"data", 0).unwrap();
        ar.close().unwrap();
    }
    // Opening without a password must fail: headers are encrypted.
    assert!(RarArchive::open(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn in_memory_sink_archive_is_well_formed() {
    // The stream seam: an archive written into a Cursor (no disk)
    // must be byte-valid — same envelope, quick-open and end blocks a
    // file archive would carry.
    use std::io::{Cursor, Read, Seek, SeekFrom};

    let opts = crate::options::CreateOptions {
        quick_open: true,
        ..Default::default()
    };
    let mut ar = RarArchive::create_with_sink(
        PathBuf::from("mem.rar"),
        opts,
        Box::new(Cursor::new(Vec::new())),
    )
    .unwrap();
    ar.add_bytes("a.txt", b"hello", 3).unwrap();
    ar.add_bytes("b.bin", &vec![7u8; 1000], 0).unwrap();
    let mut sink = ar.finish_into_sink().unwrap();
    sink.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    sink.read_to_end(&mut bytes).unwrap();

    // Structural scan through the same seam the archive scanner uses.
    let mut cursor = Cursor::new(&bytes);
    cursor.set_position(8); // skip signature
    let mut types = Vec::new();
    while let Ok(Some(meta)) = crate::format::rar5::headers::read_block(&mut cursor, None) {
        types.push(meta.block_type);
        cursor.set_position(meta.data_end);
        if meta.block_type == BLOCK_TYPE_END_ARCHIVE {
            break;
        }
    }
    assert_eq!(types.first(), Some(&BLOCK_TYPE_ARCHIVE_HEADER));
    assert!(types.contains(&BLOCK_TYPE_FILE_HEADER), "{types:?}");
    assert!(types.contains(&BLOCK_TYPE_SERVICE_HEADER), "{types:?}");
    assert_eq!(types.last(), Some(&BLOCK_TYPE_END_ARCHIVE), "{types:?}");

    // Persisted, the in-memory archive must open and read back.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mem.rar");
    std::fs::write(&path, &bytes).unwrap();
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("a.txt").unwrap(), b"hello");
    assert_eq!(rar.read("b.bin").unwrap(), vec![7u8; 1000]);
}

#[test]
fn multivolume_create_store_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mv.rar");

    // Generate test data (102400 bytes)
    let mut rng_data = vec![0u8; 102400];
    for (i, b) in rng_data.iter_mut().enumerate() {
        *b = (i.wrapping_mul(7) ^ (i >> 3)) as u8;
    }
    let small = b"Hello from multi-volume test\n";

    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                volume_size: Some(30000),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("big.bin", &rng_data, 0).unwrap();
        ar.add_bytes("small.txt", small, 0).unwrap();
        ar.close().unwrap();
    }

    // Verify volumes were created
    let vols = discover_volumes(&path);
    assert!(vols.len() > 1, "should create multiple volumes");

    // Read back
    {
        let mut ar = RarArchive::open(&vols[0]).unwrap();
        let entries = ar.list().to_vec();
        assert_eq!(entries.len(), 2);

        assert_eq!(ar.read("big.bin").unwrap(), rng_data);
        assert_eq!(ar.read("small.txt").unwrap(), small.to_vec());
    }
}

#[test]
fn multivolume_create_compressed_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mvc.rar");

    let data = b"Compressible data pattern!\n".repeat(3000);
    let small = b"Tiny file";

    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                volume_size: Some(30000),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("data.txt", &data, 3).unwrap();
        ar.add_bytes("small.txt", small, 3).unwrap();
        ar.close().unwrap();
    }

    let vols = discover_volumes(&path);
    assert!(!vols.is_empty());

    {
        let mut ar = RarArchive::open(&vols[0]).unwrap();
        assert_eq!(ar.read("data.txt").unwrap(), data);
        assert_eq!(ar.read("small.txt").unwrap(), small.to_vec());
    }
}

#[test]
fn header_encrypted_multivolume_self_roundtrip() {
    // Read-side support for -hp volume sets (WinRAR repeats the
    // plaintext encryption header on every volume; every block after it
    // is IV + AES-CBC). Covers both STORE and compressed members.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mvhp.rar");

    let store_data = (0..120_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let comp_data = b"header encrypted volume payload ".repeat(4_000);

    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                volume_size: Some(30_000),
                password: Some("pw".into()),
                encrypt_headers: true,
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("store.bin", &store_data, 0).unwrap();
        ar.add_bytes("comp.bin", &comp_data, 3).unwrap();
        ar.close().unwrap();
    }

    let vols = discover_volumes(&path);
    assert!(vols.len() > 1, "precondition: multiple volumes");

    {
        let mut ar = RarArchive::open_with_password(&vols[0], "pw").unwrap();
        assert_eq!(ar.namelist(), ["store.bin", "comp.bin"]);
        assert_eq!(ar.read("store.bin").unwrap(), store_data);
        assert_eq!(ar.read("comp.bin").unwrap(), comp_data);
    }
    // Wrong password must be rejected.
    assert!(RarArchive::open_with_password(&vols[0], "nope").is_err());
}

#[test]
fn multivolume_discover_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disc.rar");

    let data = vec![0u8; 50000];
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                volume_size: Some(20000),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("data.bin", &data, 0).unwrap();
        ar.close().unwrap();
    }

    // Discover from part1
    let vols = discover_volumes(&dir.path().join("disc.part1.rar"));
    assert!(vols.len() > 1);

    // Discover from part2
    let vols2 = discover_volumes(&dir.path().join("disc.part2.rar"));
    assert_eq!(vols2.len(), vols.len());
    assert_eq!(
        vols2[0].file_name().unwrap().to_str().unwrap(),
        "disc.part1.rar"
    );
}

#[test]
fn rar4_volume_path_legacy_naming() {
    // RAR4 multi-volume sets use the legacy `.rar`/`.rNN` naming: the first
    // volume is `x.rar`, then `x.r00`, `x.r01`, … with one extension letter
    // per hundred volumes.
    let parent = std::path::Path::new("set");
    assert_eq!(
        volume_path_rar4(parent, "vol", 1).to_string_lossy(),
        r"set\vol.rar"
    );
    assert_eq!(
        volume_path_rar4(parent, "vol", 2).to_string_lossy(),
        r"set\vol.r00"
    );
    assert_eq!(
        volume_path_rar4(parent, "vol", 3).to_string_lossy(),
        r"set\vol.r01"
    );
    assert_eq!(
        volume_path_rar4(parent, "vol", 100).to_string_lossy(),
        r"set\vol.r98"
    );
    assert_eq!(
        volume_path_rar4(parent, "vol", 101).to_string_lossy(),
        r"set\vol.r99"
    );
    // Past 100 volumes the letter advances: r → s.
    assert_eq!(
        volume_path_rar4(parent, "vol", 102).to_string_lossy(),
        r"set\vol.s00"
    );
}

#[test]
fn multivolume_open_from_any_part() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("anypart.rar");

    let data = vec![42u8; 80000];
    {
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                volume_size: Some(30000),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("data.bin", &data, 0).unwrap();
        ar.close().unwrap();
    }

    // Open from part2
    let part2 = dir.path().join("anypart.part2.rar");
    let mut ar = RarArchive::open(&part2).unwrap();
    assert_eq!(ar.read("data.bin").unwrap(), data);
}

#[test]
fn sanitize_archive_path_rejects_unsafe_names() {
    for bad in [
        "",
        "../evil",
        "a/../../b",
        "/etc/passwd",
        "//server/share",
        "C:/windows",
        "c:\\windows",
        "file.txt\0",
        ".",
        "./",
    ] {
        assert!(
            sanitize_archive_path(bad).is_err(),
            "{bad:?} should be rejected"
        );
    }
}

#[test]
fn sanitize_archive_path_normalizes_safe_names() {
    assert_eq!(sanitize_archive_path("a/b.txt").unwrap(), "a/b.txt");
    assert_eq!(sanitize_archive_path("a\\b.txt").unwrap(), "a/b.txt");
    assert_eq!(
        sanitize_archive_path("./a//b/./c.txt").unwrap(),
        "a/b/c.txt"
    );
    assert_eq!(sanitize_archive_path("dir/").unwrap(), "dir");
}

/// Names of leftover staging files (`.rar5tmp-*`) in `dir`.
fn temp_leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("rar5tmp"))
        .collect()
}

#[test]
fn create_is_not_visible_until_close_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.rar");
    let mut ar =
        RarArchive::create_with_options(&path, crate::options::CreateOptions::default()).unwrap();
    ar.add_bytes("a.txt", b"data", 0).unwrap();
    // Creation is staged: nothing appears at the target path until close.
    assert!(!path.exists());
    ar.close().unwrap();
    assert!(path.exists());
    assert!(temp_leftovers(dir.path()).is_empty());
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("a.txt").unwrap(), b"data");
}

#[test]
fn dropped_write_is_finalized_and_committed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.rar");
    {
        let mut ar =
            RarArchive::create_with_options(&path, crate::options::CreateOptions::default())
                .unwrap();
        ar.add_bytes("a.txt", b"data", 0).unwrap();
    }
    assert!(path.exists());
    assert!(temp_leftovers(dir.path()).is_empty());
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("a.txt").unwrap(), b"data");
}

#[test]
fn failed_commit_leaves_no_archive_or_temp() {
    let dir = tempfile::tempdir().unwrap();
    // A directory at the target path: the final rename must fail.
    let target = dir.path().join("t.rar");
    std::fs::create_dir(&target).unwrap();
    {
        let mut ar =
            RarArchive::create_with_options(&target, crate::options::CreateOptions::default())
                .unwrap();
        ar.add_bytes("a.txt", b"data", 0).unwrap();
        assert!(ar.close().is_err());
    }
    // The target is untouched and the staged temp file was cleaned up.
    assert!(target.is_dir());
    assert!(temp_leftovers(dir.path()).is_empty());
}

#[test]
fn append_keeps_original_untouched_until_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.rar");
    let original = {
        let mut ar =
            RarArchive::create_with_options(&path, crate::options::CreateOptions::default())
                .unwrap();
        ar.add_bytes("a.txt", b"original", 0).unwrap();
        ar.close().unwrap();
        std::fs::read(&path).unwrap()
    };
    {
        let mut ar = RarArchive::open_append(&path).unwrap();
        // The append is staged: the original file stays byte-identical
        // while the append is in progress.
        assert_eq!(std::fs::read(&path).unwrap(), original);
        ar.add_bytes("b.txt", b"appended", 0).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        ar.close().unwrap();
    }
    assert_ne!(std::fs::read(&path).unwrap(), original);
    assert!(temp_leftovers(dir.path()).is_empty());
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("a.txt").unwrap(), b"original");
    assert_eq!(rar.read("b.txt").unwrap(), b"appended");
}

#[test]
fn quick_open_only_archive_can_be_appended() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quick-open.rar");
    {
        let mut archive = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                quick_open: true,
                ..Default::default()
            },
        )
        .unwrap();
        archive.add_bytes("a.txt", b"original", 0).unwrap();
        archive.close().unwrap();
    }
    {
        let mut archive = RarArchive::open_append(&path).unwrap();
        archive.add_bytes("b.txt", b"appended", 0).unwrap();
        archive.close().unwrap();
    }

    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(archive.read("a.txt").unwrap(), b"original");
    assert_eq!(archive.read("b.txt").unwrap(), b"appended");
}

#[test]
fn multivolume_creation_stages_volumes_until_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mv.rar");
    let data = vec![42u8; 80000];
    let mut ar = RarArchive::create_with_options(
        &path,
        crate::options::CreateOptions {
            volume_size: Some(30000),
            ..Default::default()
        },
    )
    .unwrap();
    ar.add_bytes("data.bin", &data, 0).unwrap();
    // Volumes are staged under a temporary base: no final volume exists
    // until close.
    assert!(!dir.path().join("mv.part1.rar").exists());
    assert!(!temp_leftovers(dir.path()).is_empty());
    ar.close().unwrap();
    assert!(dir.path().join("mv.part1.rar").exists());
    assert!(dir.path().join("mv.part2.rar").exists());
    assert!(temp_leftovers(dir.path()).is_empty());
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("data.bin").unwrap(), data);
}

fn all_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn abort_clears_recovery_state_and_disarms_later_close() {
    let dir = tempfile::tempdir().unwrap();

    // Inline recovery record on a single-volume archive.
    let inline_path = dir.path().join("abort-inline.rar");
    {
        let mut archive = RarArchive::create_with_options(
            &inline_path,
            crate::options::CreateOptions {
                recovery_percent: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        archive.add_bytes("a.txt", b"x", 0).unwrap();
        archive.abort();
        // Drop still runs close(); with the recovery trigger cleared it must
        // commit nothing and must not rebuild the inline recovery record.
        assert!(archive.recovery_percent.is_none());
        assert!(archive.close().is_ok());
    }
    assert!(!inline_path.exists());

    // .rev recovery volumes on a multi-volume archive.
    let volumes_path = dir.path().join("abort-volumes.rar");
    {
        let mut archive = RarArchive::create_with_options(
            &volumes_path,
            crate::options::CreateOptions {
                volume_size: Some(32 * 1024),
                recovery_volume_count: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        archive
            .add_bytes("payload.bin", &vec![7u8; 96 * 1024], 0)
            .unwrap();
        assert!(archive.recovery_volumes_count.is_some());
        archive.abort();
        assert!(archive.recovery_volumes_count.is_none());
        assert!(archive.recovery_volumes_percent.is_none());
        assert!(archive.recovery_percent.is_none());
        // Drop's close() must not regenerate .rev files over a volume set
        // that was never committed.
        assert!(archive.close().is_ok());
    }
    assert!(
        all_files(dir.path()).is_empty(),
        "aborted transaction left files: {:?}",
        all_files(dir.path())
    );
}

#[test]
fn abort_disarms_the_legacy_drop_auto_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abort-drop.rar");
    {
        let mut archive = RarArchive::create_with_options(&path, Default::default()).unwrap();
        archive.add_bytes("a.txt", b"x", 0).unwrap();
        // Legacy Drop commits on its own; abort() must turn that off so an
        // aborted transaction never becomes visible at the final path.
        archive.abort();
    }
    assert!(!path.exists());
    assert!(all_files(dir.path()).is_empty());
}
