//! RAR 1.3/1.4 (`RE~^`) creation: single-volume STORE / m1-m5 compression,
//! solid chains, archive comments and `-p` member encryption, all read back
//! through our own reader (the `winrar_interop` suite compares against
//! official UnRAR).

use rar_rs::{
    ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions, OpenOptions,
    RarError, SolidMode, WriterOptions,
};

fn writer_options() -> WriterOptions {
    WriterOptions::new().compression(ArchiveVersion::V14)
}

fn level(n: u8) -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::try_from(n).unwrap())
}

fn repetitive(seed: &[u8], repeats: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(seed.len() * repeats);
    for _ in 0..repeats {
        out.extend_from_slice(seed);
    }
    out
}

/// Incompressible bytes so volume splitting is exercised deterministically.
fn pseudo_random(len: usize) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect()
}

#[test]
fn rar13_create_roundtrips_stored_and_compressed_members() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.rar");
    let stored = b"stored bytes, untouched\r\n".to_vec();
    let text = repetitive(b"the quick brown fox jumps over the lazy dog. ", 512);

    let mut writer = ArchiveWriter::create_with(&path, writer_options()).unwrap();
    writer.add_bytes("store.bin", &stored, level(0)).unwrap();
    for compression in 1..=5 {
        println!("compression level {compression}");
        writer
            .add_bytes(&format!("c{compression}.txt"), &text, level(compression))
            .unwrap();
    }
    writer.add_bytes("empty.bin", &[], level(5)).unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    assert_eq!(reader.entries().count(), 7);
    for entry in reader.entries() {
        assert_eq!(entry.version(), ArchiveVersion::V14, "{}", entry.name());
    }
    let bytes = |reader: &mut ArchiveReader, name: &str| {
        let id = reader.unique_entry(name).unwrap();
        reader.read_entry(id).unwrap()
    };
    assert_eq!(bytes(&mut reader, "store.bin"), stored);
    assert_eq!(bytes(&mut reader, "empty.bin"), Vec::<u8>::new());
    for compression in 1..=5 {
        let name = format!("c{compression}.txt");
        assert_eq!(
            bytes(&mut reader, &name),
            text,
            "level {compression} roundtrip"
        );
    }
    let store = reader
        .entry(reader.unique_entry("store.bin").unwrap())
        .unwrap();
    assert_eq!(store.method(), 0, "level 0 must be stored");
}

#[test]
fn rar13_create_solid_chain_links_members() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid.rar");
    let first = repetitive(b"solid-chain seed block; ", 400);
    let second = repetitive(b"solid-chain seed block; ", 10);
    let third = b"unique tail data".to_vec();

    let mut writer =
        ArchiveWriter::create_with(&path, writer_options().solid_mode(SolidMode::Continuous))
            .unwrap();
    writer.add_bytes("a.txt", &first, level(5)).unwrap();
    writer.add_bytes("b.txt", &second, level(5)).unwrap();
    writer.add_bytes("c.txt", &third, level(5)).unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    let bytes = |reader: &mut ArchiveReader, name: &str| {
        let id = reader.unique_entry(name).unwrap();
        reader.read_entry(id).unwrap()
    };
    assert_eq!(bytes(&mut reader, "a.txt"), first);
    assert_eq!(bytes(&mut reader, "b.txt"), second);
    assert_eq!(bytes(&mut reader, "c.txt"), third);
    let b_id = reader.unique_entry("b.txt").unwrap();
    assert!(reader.entry(b_id).unwrap().comp_solid());
}

#[test]
fn rar13_create_directory_and_payload_order_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.rar");
    let subdir = dir.path().join("SUB");
    std::fs::create_dir(&subdir).unwrap();

    let mut writer = ArchiveWriter::create_with(&path, writer_options()).unwrap();
    writer
        .add_bytes("SUB/INNER.TXT", b"inside\r\n", level(5))
        .unwrap();
    writer.add_directory(&subdir, "SUB").unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    let names: Vec<String> = reader
        .entries()
        .map(|entry| entry.name().to_string())
        .collect();
    assert_eq!(names.join(","), "SUB/INNER.TXT,SUB/");
    let id = reader.unique_entry("SUB/INNER.TXT").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), b"inside\r\n");
    let dir_id = reader.unique_entry("SUB/").unwrap();
    assert!(reader.entry(dir_id).unwrap().is_dir());
}

#[test]
fn rar13_create_archive_comment_is_stored_before_members() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("comment.rar");
    let comment = b"archive comment\r\nsecond line\r\n".to_vec();

    let mut writer = ArchiveWriter::create_with(&path, writer_options()).unwrap();
    writer.set_archive_comment(Some(comment.clone())).unwrap();
    writer.add_bytes("a.txt", b"payload", level(5)).unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    assert_eq!(reader.comment().unwrap(), Some(comment));
    let id = reader.unique_entry("a.txt").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), b"payload");
}

#[test]
fn rar13_create_empty_archive_writes_a_main_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.rar");
    let writer = ArchiveWriter::create_with(&path, writer_options()).unwrap();
    writer.finish().unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..4], b"RE~^");
    assert!(bytes.len() >= 7);
    let reader = ArchiveReader::open(&path).unwrap();
    assert_eq!(reader.entries().count(), 0);
}

#[test]
fn rar13_create_encrypted_members_require_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret.rar");
    let text = repetitive(b"encrypted payload; ", 200);

    let mut writer = ArchiveWriter::create_with(&path, writer_options().password("pw")).unwrap();
    writer.add_bytes("secret.txt", &text, level(5)).unwrap();
    writer
        .add_bytes("stored.txt", b"plaintext?", level(0))
        .unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open_with(&path, OpenOptions::new().password("pw")).unwrap();
    let id = reader.unique_entry("secret.txt").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), text);
    let id = reader.unique_entry("stored.txt").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), b"plaintext?");

    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry("secret.txt").unwrap();
    assert!(matches!(reader.read_entry(id), Err(RarError::Encrypted(_))));
}

#[test]
fn rar13_create_rejects_options_the_container_cannot_express() {
    let dir = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, WriterOptions)> = vec![
        ("quick-open", writer_options().quick_open(true)),
        ("blake2", writer_options().blake2(true)),
        ("header encryption", writer_options().encrypt_headers(true)),
        ("recovery", writer_options().recovery_percent(5)),
        (
            "recovery volumes",
            writer_options().recovery_volume_count(1),
        ),
        ("owner", writer_options().save_owner(true)),
        ("streams", writer_options().save_streams(true)),
        (
            "dictionary",
            writer_options().dictionary_size(rar_rs::DictionarySize::DEFAULT),
        ),
    ];
    for (name, options) in cases {
        let path = dir.path().join(format!("{name}.rar"));
        let error = ArchiveWriter::create_with(&path, options).unwrap_err();
        assert!(
            matches!(error, RarError::InvalidOption(_)),
            "{name}: {error:?}"
        );
    }
}

/// Volume sets fill every volume to the exact `volume_size` (only the last
/// one is shorter) and split members carry one fragment header per volume.
#[test]
fn rar13_multivolume_store_fills_volumes_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mv.rar");
    let mut seed = Vec::new();
    for i in 0..40_000u32 {
        seed.push((i.wrapping_mul(2654435761) >> 24) as u8);
    }
    let tail = repetitive(b"tail member; ", 100);

    let mut writer =
        ArchiveWriter::create_with(&path, writer_options().volume_size(20_000)).unwrap();
    writer.add_bytes("big.bin", &seed, level(0)).unwrap();
    writer.add_bytes("tail.txt", &tail, level(5)).unwrap();
    let report = writer.finish().unwrap();

    let volumes = report.volume_paths();
    assert!(volumes.len() >= 3, "{} volumes", volumes.len());
    assert_eq!(volumes[0].file_name().unwrap(), "mv.rar");
    assert_eq!(volumes[1].file_name().unwrap(), "mv.r00");
    for (index, volume) in volumes.iter().enumerate() {
        let len = std::fs::metadata(volume).unwrap().len();
        if index + 1 == volumes.len() {
            assert!(len <= 20_000, "last volume {len} > 20000");
        } else {
            assert_eq!(len, 20_000, "volume {index} is not exact");
        }
    }

    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry("big.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), seed);
    let id = reader.unique_entry("tail.txt").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), tail);
}

/// Compressed solid chains keep their window across volume boundaries, and
/// the RAR13 cipher is one stream over each member's packed data.
#[test]
fn rar13_multivolume_solid_and_encrypted_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mv.rar");
    let first = pseudo_random(24_000);
    let second = pseudo_random(9_000);
    let third = pseudo_random(7_000);

    let mut writer = ArchiveWriter::create_with(
        &path,
        writer_options()
            .solid_mode(SolidMode::Continuous)
            .password("pw")
            .volume_size(6_000),
    )
    .unwrap();
    writer.add_bytes("a.txt", &first, level(5)).unwrap();
    writer.add_bytes("b.txt", &second, level(5)).unwrap();
    writer.add_bytes("c.txt", &third, level(5)).unwrap();
    let report = writer.finish().unwrap();
    assert!(report.volume_paths().len() >= 2);

    let mut reader = ArchiveReader::open_with(&path, OpenOptions::new().password("pw")).unwrap();
    assert_eq!(reader.entries().count(), 3);
    for (name, expected) in [("a.txt", &first), ("b.txt", &second), ("c.txt", &third)] {
        let id = reader.unique_entry(name).unwrap();
        assert_eq!(&reader.read_entry(id).unwrap(), expected, "{name}");
    }

    let error = ArchiveReader::open(&path)
        .and_then(|mut plain| {
            let id = plain.unique_entry("a.txt").unwrap();
            plain.read_entry(id)
        })
        .unwrap_err();
    assert!(matches!(error, RarError::Encrypted(_)), "{error:?}");
}

/// The archive comment lives in the first volume's main header; directories
/// and empty members move to the next volume when their header no longer
/// fits.
#[test]
fn rar13_multivolume_comment_directories_and_empty_members() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mv.rar");
    let comment = b"multi-volume comment".to_vec();
    let data = pseudo_random(18_000);

    let mut writer =
        ArchiveWriter::create_with(&path, writer_options().volume_size(4_000)).unwrap();
    writer.set_archive_comment(Some(comment.clone())).unwrap();
    writer.add_bytes("first.txt", &data, level(5)).unwrap();
    writer.add_bytes("empty.bin", &[], level(5)).unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    writer.add_directory(&sub, "sub").unwrap();
    let report = writer.finish().unwrap();
    assert!(report.volume_paths().len() >= 2);

    let mut reader = ArchiveReader::open(&path).unwrap();
    assert_eq!(reader.comment().unwrap(), Some(comment));
    let names: Vec<String> = reader
        .entries()
        .map(|entry| entry.name().to_string())
        .collect();
    assert_eq!(names, ["first.txt", "empty.bin", "sub/"]);
    let id = reader.unique_entry("first.txt").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), data);
    let id = reader.unique_entry("empty.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), Vec::<u8>::new());
}

/// The legacy `.rNN` naming tops out at 901 volumes; larger sets are
/// rejected cleanly and leave no staged files behind.
#[test]
fn rar13_multivolume_rejects_sets_beyond_the_naming_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.rar");
    let data = repetitive(b"many tiny volumes; ", 6_000);

    let mut writer = ArchiveWriter::create_with(&path, writer_options().volume_size(64)).unwrap();
    let error = writer
        .add_bytes("big.bin", &data, level(0))
        .expect_err("volume naming must be bounded");
    assert!(matches!(error, RarError::InvalidOption(_)), "{error:?}");
    assert!(!path.exists(), "a failed create must leave no archive");
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "staged volumes must be cleaned up"
    );
}

/// A volume smaller than one member header is rejected with a clear error.
#[test]
fn rar13_multivolume_rejects_volumes_below_the_header_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.rar");
    let mut writer = ArchiveWriter::create_with(&path, writer_options().volume_size(30)).unwrap();
    let error = writer
        .add_bytes("member.bin", b"payload", level(0))
        .expect_err("volume size must fit a header");
    assert!(matches!(error, RarError::InvalidOption(_)), "{error:?}");
    assert!(!path.exists());
}

#[test]
fn rar13_append_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("append.rar");
    let mut writer = ArchiveWriter::create_with(&path, writer_options()).unwrap();
    writer.add_bytes("a.txt", b"data", level(5)).unwrap();
    writer.finish().unwrap();

    let error = ArchiveWriter::append(&path).unwrap_err();
    assert!(matches!(error, RarError::Unsupported(_)), "{error:?}");
}

#[test]
fn rar5_rejects_a_queued_archive_comment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rar5.rar");
    let mut writer = ArchiveWriter::create_with(&path, WriterOptions::new()).unwrap();
    let error = writer
        .set_archive_comment(Some(b"nope".to_vec()))
        .unwrap_err();
    assert!(matches!(error, RarError::Unsupported(_)), "{error:?}");
}
