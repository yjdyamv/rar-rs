//! RAR 1.3/1.4 read support (`RE~^` container): header parsing, stored and
//! compressed members, solid chains, encryption, old-style multi-volume
//! sets, SFX stubs and comments. Fixtures come from the `rars` fixture
//! corpus (`tests/fixtures/rar13`, WTFPL-derived test data).

use rar_rs::{ArchiveReader, OpenOptions};

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rar13/");

fn fixture(name: &str) -> String {
    format!("{FIX}{name}")
}

fn read_member(path: &str, name: &str) -> Vec<u8> {
    let mut reader = ArchiveReader::open(path).unwrap();
    let id = reader.unique_entry(name).unwrap();
    reader
        .read_entry(id)
        .unwrap_or_else(|error| panic!("reading {name} from {path}: {error}"))
}

fn read_member_with_password(path: &str, name: &str, password: &str) -> Vec<u8> {
    let mut reader = ArchiveReader::open_with(path, OpenOptions::new().password(password)).unwrap();
    let id = reader.unique_entry(name).unwrap();
    reader.read_entry(id).unwrap()
}

fn readme_expected() -> Vec<u8> {
    std::fs::read(fixture("README")).unwrap()
}

#[test]
fn rar13_lists_every_single_volume_fixture() {
    let cases = [
        ("EMPTY.RAR", vec!["EMPTY.BIN"]),
        ("README.RAR", vec!["README"]),
        ("README_store.rar", vec!["README"]),
        ("MULTIFIL.RAR", vec!["HELLO.TXT", "TINY.TXT"]),
        ("WITHDIR.RAR", vec!["SUBDIR", "SUBDIR\\INNER.TXT"]),
        ("SOLID.RAR", vec!["BIG80K.TXT", "HELLO.TXT", "TINY.TXT"]),
        ("REPEATB.RAR", vec!["REPEATB.BIN"]),
        ("solid_flag_cleared.rar", vec!["a.txt", "b.txt"]),
    ];
    for (file, expected) in cases {
        let reader = ArchiveReader::open(fixture(file)).unwrap();
        let names: Vec<String> = reader
            .entries()
            .map(|entry| entry.name().to_string())
            .collect();
        assert_eq!(names.join(","), expected.join(","), "{file}");
        for entry in reader.entries() {
            assert_eq!(
                entry.version(),
                rar_rs::ArchiveVersion::V14,
                "{file}: {}",
                entry.name()
            );
        }
    }
}

#[test]
fn rar13_decodes_stored_and_compressed_members() {
    let expected = readme_expected();
    assert_eq!(
        read_member(&fixture("README_store.rar"), "README"),
        expected
    );
    assert_eq!(read_member(&fixture("README.RAR"), "README"), expected);

    assert_eq!(
        read_member(&fixture("EMPTY.RAR"), "EMPTY.BIN"),
        Vec::<u8>::new()
    );
    assert_eq!(
        read_member(&fixture("MULTIFIL.RAR"), "HELLO.TXT"),
        b"Hello, RAR 1.402 fixture world.\r\n"
    );
    assert_eq!(
        read_member(&fixture("MULTIFIL.RAR"), "TINY.TXT"),
        b"AAAAAAAA\r\n"
    );
}

#[test]
fn rar13_window_wrap_and_repeating_members_decode() {
    let big = read_member(&fixture("BIG80K.RAR"), "BIG80K.TXT");
    assert_eq!(big.len(), 80 * 1024);

    let mut expected = Vec::with_capacity(256 * 32);
    for _ in 0..32 {
        expected.extend(0u8..=255);
    }
    assert_eq!(
        read_member(&fixture("REPEATB.RAR"), "REPEATB.BIN"),
        expected
    );
}

#[test]
fn rar13_solid_chain_shares_the_window() {
    let archive = fixture("SOLID.RAR");
    assert_eq!(read_member(&archive, "BIG80K.TXT").len(), 80 * 1024);
    assert_eq!(
        read_member(&archive, "HELLO.TXT"),
        b"Hello, RAR 1.402 fixture world.\r\n"
    );
    assert_eq!(read_member(&archive, "TINY.TXT"), b"AAAAAAAA\r\n");

    // A solid flag cleared on the second member must not break the chain.
    let cleared = fixture("solid_flag_cleared.rar");
    let a = read_member(&cleared, "a.txt");
    let b = read_member(&cleared, "b.txt");
    assert!(!a.is_empty() && !b.is_empty());
}

#[test]
fn rar13_directories_extract_as_empty_entries() {
    let reader = ArchiveReader::open(fixture("WITHDIR.RAR")).unwrap();
    let dir = reader.entries().next().unwrap();
    assert!(dir.is_dir());
    assert_eq!(dir.size(), 0);
    drop(reader);
    assert_eq!(
        read_member(&fixture("WITHDIR.RAR"), "SUBDIR\\INNER.TXT"),
        b"Inside subdir.\r\n"
    );
}

#[test]
fn rar13_encrypted_members_require_the_password() {
    let expected = readme_expected();
    assert_eq!(
        read_member_with_password(
            &fixture("README_password=password.rar"),
            "README",
            "password"
        ),
        expected
    );
    assert_eq!(
        read_member_with_password(&fixture("STOREPWD.RAR"), "SECRET.TXT", "password"),
        b"Stored encrypted fixture.\r\n"
    );

    // No password: the member reports as encrypted, not as corrupt.
    let mut reader = ArchiveReader::open(fixture("STOREPWD.RAR")).unwrap();
    let id = reader.unique_entry("SECRET.TXT").unwrap();
    assert!(matches!(
        reader.read_entry(id),
        Err(rar_rs::RarError::Encrypted(_))
    ));

    // Wrong password: indistinguishable from corruption, like every RAR13
    // reader; the decode must fail rather than return garbage.
    let mut reader = ArchiveReader::open_with(
        fixture("README_password=password.rar"),
        OpenOptions::new().password("wrong-password"),
    )
    .unwrap();
    let id = reader.unique_entry("README").unwrap();
    assert!(reader.read_entry(id).is_err());
}

#[test]
fn rar13_old_style_multivolume_sets_reassemble() {
    let random = read_member(&fixture("MULTIVOL.RAR"), "RANDOM.BIN");
    assert_eq!(random.len(), 65_536);

    let compressed = read_member(&fixture("CMULTIV.RAR"), "CMULTI.TXT");
    assert_eq!(compressed, std::fs::read(fixture("CMULTI.TXT")).unwrap());
}

#[test]
fn rar13_sfx_archive_is_located_and_decoded() {
    assert_eq!(
        read_member(&fixture("SFXSRC.EXE"), "HELLO.TXT"),
        b"Hello, RAR 1.402 fixture world.\r\n"
    );
}

#[test]
fn rar13_archive_and_member_comments_decode() {
    let mut reader = ArchiveReader::open(fixture("COMMENT.RAR")).unwrap();
    assert_eq!(
        reader.comment().unwrap().as_deref(),
        Some(b"This is the archive comment.\r\n".as_slice())
    );
    drop(reader);
    assert_eq!(
        read_member(&fixture("COMMENT.RAR"), "HELLO.TXT"),
        b"Hello, comment fixture.\r\n"
    );

    let reader = ArchiveReader::open(fixture("FCOMM.RAR")).unwrap();
    let entry = reader.entries().next().unwrap();
    assert_eq!(entry.comment(), Some(b"FCOM\r\n".as_slice()));
    drop(reader);
    assert_eq!(
        read_member(&fixture("FCOMM.RAR"), "HELLO.TXT"),
        b"Hello, file comment fixture.\r\n"
    );
}

#[test]
fn rar13_truncated_payload_is_rejected() {
    let bytes = std::fs::read(fixture("README.RAR")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("truncated.rar");
    std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    assert!(ArchiveReader::open(&path).is_err());
}
