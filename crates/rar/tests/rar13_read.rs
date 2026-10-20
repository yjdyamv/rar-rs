//! RAR 1.3/1.4 read support (`RE~^` container): header parsing, stored and
//! compressed members, solid chains, encryption, old-style multi-volume sets,
//! SFX stubs, directories and comments.
//!
//! No released WinRAR can still write this container (the oldest cached build,
//! 2.90, writes `unp_ver 20`), so the archives here are produced by our own
//! V14 writer and two byte-level mutations on its output. UnRAR 7.23 decodes
//! the same writer output byte-identically in
//! `crates/rar-cli/tests/winrar_interop/rar13.rs`, which is what keeps the
//! writer honest; these tests check the read side against those archives.

use rar_rs::{
    ArchiveReader, ArchiveVersion, CompressionLevel, EntryWriteOptions, OpenOptions, SolidMode,
    WriterOptions,
};

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use support::generate::{payload, scratch_root};

fn store() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn level(level: u8) -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::try_from(level).unwrap())
}

fn v14() -> WriterOptions {
    WriterOptions::new().compression(ArchiveVersion::V14)
}

/// A fresh scratch file path (the writer needs a target that does not exist).
fn target(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = scratch_root().join(format!("rar13-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Build a V14 archive from `(member, payload)` pairs.
fn build(name: &str, options: WriterOptions, members: &[(&str, &[u8])]) -> std::path::PathBuf {
    let path = target(name);
    let mut archive = rar_rs::ArchiveWriter::create_with(&path, options).unwrap();
    for (member, data) in members {
        archive.add_bytes(member, data, level(3)).unwrap();
    }
    if members.is_empty() {
        // RAR 1.3 has no end-of-archive marker; a lone main header is legal.
        archive.finish().unwrap();
    } else {
        archive.finish().unwrap();
    }
    path
}

fn read_member(path: &std::path::Path, name: &str) -> Vec<u8> {
    let mut reader = ArchiveReader::open(path).unwrap();
    let id = reader.unique_entry(name).unwrap();
    reader
        .read_entry(id)
        .unwrap_or_else(|error| panic!("reading {name} from {}: {error}", path.display()))
}

fn read_member_with_password(path: &std::path::Path, name: &str, password: &str) -> Vec<u8> {
    let mut reader = ArchiveReader::open_with(path, OpenOptions::new().password(password)).unwrap();
    let id = reader.unique_entry(name).unwrap();
    reader.read_entry(id).unwrap()
}

/// Offset of the first file header: the main header's own `HEAD_SIZE` (read
/// from offset 4) already covers the 4-byte signature it starts with.
fn first_file_header(bytes: &[u8]) -> usize {
    usize::from(u16::from_le_bytes([bytes[4], bytes[5]]))
}

#[test]
fn rar14_lists_members_and_reports_v14() {
    let text = payload("text.txt");
    let path = build(
        "list.rar",
        v14(),
        &[
            ("HELLO.TXT", b"Hello, RAR 1.402 world.\r\n"),
            ("text.txt", &text),
        ],
    );
    let reader = ArchiveReader::open(&path).unwrap();
    let names: Vec<String> = reader
        .entries()
        .map(|entry| entry.name().to_string())
        .collect();
    assert_eq!(names, vec!["HELLO.TXT".to_string(), "text.txt".to_string()]);
    for entry in reader.entries() {
        assert_eq!(entry.version(), ArchiveVersion::V14, "{}", entry.name());
    }
}

#[test]
fn rar14_decodes_stored_and_compressed_members() {
    let text = payload("text.txt");
    let stored = build("store.rar", v14(), &[("README", &text)]);
    assert_eq!(read_member(&stored, "README"), text);

    let compressed = build("m3.rar", v14(), &[("README", &text)]);
    assert_eq!(read_member(&compressed, "README"), text);

    let empty = build("empty.rar", v14(), &[("EMPTY.BIN", b"")]);
    assert_eq!(read_member(&empty, "EMPTY.BIN"), Vec::<u8>::new());
}

#[test]
fn rar14_window_wrap_and_repeating_members_decode() {
    // 80 KiB is past the 64 KiB Unpack15 window, so the decoder has to wrap.
    let big: Vec<u8> = payload("text.txt").repeat(2);
    let path = build("big.rar", v14(), &[("BIG80K.TXT", &big)]);
    assert_eq!(read_member(&path, "BIG80K.TXT"), big);

    let mut repeating = Vec::with_capacity(256 * 32);
    for _ in 0..32 {
        repeating.extend(0u8..=255);
    }
    let path = build("repeat.rar", v14(), &[("REPEATB.BIN", &repeating)]);
    assert_eq!(read_member(&path, "REPEATB.BIN"), repeating);
}

#[test]
fn rar14_solid_chain_shares_the_window() {
    let a = payload("text.txt");
    let b = payload("repeat.txt");
    let path = build(
        "solid.rar",
        v14().solid_mode(SolidMode::Continuous),
        &[("a.txt", &a), ("b.txt", &b)],
    );
    assert_eq!(read_member(&path, "a.txt"), a);
    // The second member decodes only if it resumes the shared window.
    assert_eq!(read_member(&path, "b.txt"), b);
}

#[test]
fn rar14_solid_chain_ignores_a_cleared_solid_flag() {
    let a = payload("text.txt");
    let b = payload("repeat.txt");
    let path = build(
        "cleared.rar",
        v14().solid_mode(SolidMode::Continuous),
        &[("a.txt", &a), ("b.txt", &b)],
    );
    // Clear LHD_SOLID (0x10) on the second member's file header. RAR 1.3
    // headers carry no header CRC, so the flag byte can be flipped in place.
    let mut bytes = std::fs::read(&path).unwrap();
    let first = first_file_header(&bytes);
    let first_size = usize::from(u16::from_le_bytes([bytes[first + 10], bytes[first + 11]]));
    let first_pack = u32::from_le_bytes(bytes[first..first + 4].try_into().unwrap()) as usize;
    let second = first + first_size + first_pack;
    assert!(bytes[second] != 0, "second member header missing");
    bytes[second + 17] &= !0x10;
    let mutated = target("cleared-mutated.rar");
    std::fs::write(&mutated, &bytes).unwrap();

    assert_eq!(read_member(&mutated, "a.txt"), a);
    assert_eq!(read_member(&mutated, "b.txt"), b);
}

#[test]
fn rar14_directories_extract_as_empty_entries() {
    let dir = target("dir-src");
    std::fs::create_dir_all(&dir).unwrap();
    let path = target("withdir.rar");
    let mut writer = rar_rs::ArchiveWriter::create_with(&path, v14()).unwrap();
    writer.add_directory(&dir, "SUBDIR").unwrap();
    writer
        .add_bytes("SUBDIR\\INNER.TXT", b"Inside subdir.\r\n", store())
        .unwrap();
    writer.finish().unwrap();

    let reader = ArchiveReader::open(&path).unwrap();
    let first = reader.entries().next().unwrap();
    assert!(first.is_dir());
    assert_eq!(first.size(), 0);
    let inner = reader
        .entries()
        .map(|entry| entry.name().to_string())
        .find(|name| name.ends_with("INNER.TXT"))
        .expect("inner member is listed");
    drop(reader);
    assert_eq!(read_member(&path, &inner), b"Inside subdir.\r\n");
}

#[test]
fn rar14_encrypted_members_require_the_password() {
    let text = payload("text.txt");
    let path = build("pw.rar", v14().password("password"), &[("README", &text)]);
    assert_eq!(read_member_with_password(&path, "README", "password"), text);

    let stored = build(
        "pws.rar",
        v14().password("password"),
        &[("SECRET.TXT", b"Stored encrypted payload.\r\n")],
    );
    assert_eq!(
        read_member_with_password(&stored, "SECRET.TXT", "password"),
        b"Stored encrypted payload.\r\n"
    );

    // No password: the member reports as encrypted, not as corrupt.
    let mut reader = ArchiveReader::open(&stored).unwrap();
    let id = reader.unique_entry("SECRET.TXT").unwrap();
    assert!(matches!(
        reader.read_entry(id),
        Err(rar_rs::RarError::Encrypted(_))
    ));

    // Wrong password: indistinguishable from corruption, like every RAR13
    // reader; the decode must fail rather than return garbage.
    let mut reader =
        ArchiveReader::open_with(&path, OpenOptions::new().password("wrong-password")).unwrap();
    let id = reader.unique_entry("README").unwrap();
    assert!(reader.read_entry(id).is_err());
}

#[test]
fn rar14_old_style_multivolume_sets_reassemble() {
    // Incompressible so the member really splits across volumes.
    let random = payload("random.bin");
    let path = build(
        "mvol.rar",
        v14().volume_size(32 * 1024).old_numbering(true),
        &[("RANDOM.BIN", &random)],
    );
    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() > 1, "{} volumes", volumes.len());
    assert!(
        path.extension().unwrap().eq_ignore_ascii_case("rar")
            && path.with_extension("r00").exists(),
        "old numbering writes .rar/.r00"
    );
    assert_eq!(read_member(&path, "RANDOM.BIN"), random);
}

#[test]
fn rar14_sfx_stub_archive_is_located_and_decoded() {
    let path = build("plain.rar", v14(), &[("HELLO.TXT", b"Hello, sfx.\r\n")]);
    let mut stub = vec![0u8; 4096];
    stub[..2].copy_from_slice(b"MZ");
    stub[0x3c] = 0x80; // e_lfanew, arbitrary
    stub.extend(std::fs::read(&path).unwrap());
    let sfx = target("stub.exe");
    std::fs::write(&sfx, &stub).unwrap();
    assert_eq!(read_member(&sfx, "HELLO.TXT"), b"Hello, sfx.\r\n");
}

#[test]
fn rar14_archive_comment_decodes() {
    let path = target("comment.rar");
    let mut writer = rar_rs::ArchiveWriter::create_with(&path, v14()).unwrap();
    writer
        .set_archive_comment(Some(b"This is the archive comment.\r\n".to_vec()))
        .unwrap();
    writer
        .add_bytes("HELLO.TXT", b"Hello, comment fixture.\r\n", store())
        .unwrap();
    writer.finish().unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        reader.comment().unwrap().as_deref(),
        Some(b"This is the archive comment.\r\n".as_slice())
    );
    drop(reader);
    assert_eq!(
        read_member(&path, "HELLO.TXT"),
        b"Hello, comment fixture.\r\n"
    );
}

/// A member comment (`LHD_COMMENT`) is a length-prefixed header extension; no
/// current writer emits one, so the extension is spliced into a generated
/// header. RAR 1.3 headers carry no header CRC, which keeps the splice exact.
#[test]
fn rar14_member_comment_extension_decodes() {
    let path = build("fcomm.rar", v14(), &[("HELLO.TXT", b"Hello.\r\n")]);
    let mut bytes = std::fs::read(&path).unwrap();
    let header = first_file_header(&bytes);
    let head_size = usize::from(u16::from_le_bytes([bytes[header + 10], bytes[header + 11]]));
    let comment = b"FCOM\r\n";
    let mut extension = Vec::new();
    extension.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    extension.extend_from_slice(comment);
    let mut spliced = bytes[..header + head_size].to_vec();
    spliced.extend_from_slice(&extension);
    spliced.extend_from_slice(&bytes[header + head_size..]);
    // Grow HEAD_SIZE and set LHD_COMMENT (0x08) on the file header.
    let new_size = (head_size + extension.len()) as u16;
    spliced[header + 10..header + 12].copy_from_slice(&new_size.to_le_bytes());
    spliced[header + 17] |= 0x08;
    bytes.clear();
    let spliced_path = target("fcomm-mutated.rar");
    std::fs::write(&spliced_path, &spliced).unwrap();

    let reader = ArchiveReader::open(&spliced_path).unwrap();
    let entry = reader.entries().next().unwrap();
    assert_eq!(entry.comment(), Some(comment.as_slice()));
    drop(reader);
    assert_eq!(read_member(&spliced_path, "HELLO.TXT"), b"Hello.\r\n");
}

#[test]
fn rar14_truncated_payload_is_rejected() {
    let path = build("trunc.rar", v14(), &[("README", &payload("text.txt"))]);
    let bytes = std::fs::read(&path).unwrap();
    let truncated = target("truncated.rar");
    std::fs::write(&truncated, &bytes[..bytes.len() - 1]).unwrap();
    assert!(ArchiveReader::open(&truncated).is_err());
}
