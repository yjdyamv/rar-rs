//! RAR 1.5–4.x read support: STORE passthrough plus RAR3/4 (unp_ver >= 29)
//! LZSS+Huffman decode with solid chains and RAR30 encryption, verified
//! against genuine WinRAR 5.91 `-ma4` fixtures and legacy RAR 3.0 archives
//! (from the rars fixture corpus).

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar5::{RarArchive, RarError};

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rar40/");
const RAR300: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rar40/rar300/");
const W591: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/rar40/winrar591/"
);

/// The exact bytes WinRAR 5.91 `-ma4` stored/compressed in the fixtures: 4000
/// lines of the same sentence + counter tail.
fn repeat_content() -> Vec<u8> {
    let line = b"The quick brown fox jumps over the lazy dog. 0123456789\n";
    let mut out = Vec::with_capacity(line.len() * 4000);
    for _ in 0..4000 {
        out.extend_from_slice(line);
    }
    out
}

/// Owned (name, size, crc32) snapshots so tests can release the `list()`
/// borrow before calling `read`.
fn snapshots(archive: &RarArchive) -> Vec<(String, u64, Option<u32>)> {
    archive
        .list()
        .iter()
        .map(|e| (e.name().to_string(), e.size(), e.crc32()))
        .collect()
}

// ── STORE (full source path in the member name) ───────────────────────────

const RAR4_STORE_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/rar40/winrar591_store_m0.rar"
);

/// WinRAR 5.91 `-ma4 -m0` stores the full source path in the member name.
const MEMBER_NAME: &str = "Users\\yuan\\AppData\\Local\\Temp\\opencode\\rar4store\\hello.txt";

#[test]
fn rar4_store_list_and_read() {
    let mut archive = RarArchive::open(RAR4_STORE_FIXTURE).expect("open RAR4 STORE fixture");
    let entries = archive.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), MEMBER_NAME);

    let content = archive.read(MEMBER_NAME).expect("read member");
    assert_eq!(&content, b"store hello content");
}

#[test]
fn rar4_store_header_fields() {
    let archive = RarArchive::open(RAR4_STORE_FIXTURE).expect("open");
    let entry = &archive.list()[0];
    assert_eq!(entry.method(), 0, "STORE normalizes to method 0");
    assert_eq!(entry.method_name(), "Store");
    assert_eq!(entry.compressed_size(), 19, "packed size");
    assert_eq!(entry.size(), 19, "unpacked size");
}

// ── WinRAR 5.91 `-ma4` compressed members (m3/m5 LZSS+Huffman) ────────────

#[test]
fn rar4_winrar591_m3_compressed_read() {
    let mut archive = RarArchive::open(&format!("{W591}c_m3.rar")).expect("open c_m3");
    let repeat = snapshots(&archive)
        .into_iter()
        .find(|(name, _, _)| name == "repeat.txt")
        .unwrap();
    assert_eq!(repeat.1, 224_000);
    // method is part of list(); assert it here via a fresh borrow.
    let repeat_method = archive
        .list()
        .iter()
        .find(|e| e.name() == "repeat.txt")
        .unwrap()
        .method();
    assert_eq!(repeat_method, 3, "m3 method normalized");

    let content = archive.read("repeat.txt").expect("decode m3 member");
    assert_eq!(content, repeat_content());
}

#[test]
fn rar4_winrar591_m5_lz_compressed_read() {
    // -ma4 -m5 on this highly repetitive text picks LZSS (not PPMd).
    let mut archive = RarArchive::open(&format!("{W591}c_m5.rar")).expect("open c_m5");
    assert_eq!(
        archive.read("repeat.txt").expect("decode m5 member"),
        repeat_content()
    );
}

#[test]
fn rar4_winrar591_random_and_tiny_members() {
    // Incompressible and tiny members store even under -m3/-m5.
    for file in ["c_m3.rar", "c_m5.rar"] {
        let mut archive = RarArchive::open(&format!("{W591}{file}")).expect("open");
        let snaps = snapshots(&archive);
        for (name, size, crc) in snaps {
            let data = archive.read(&name).expect("read member");
            assert_eq!(data.len() as u64, size, "{file}/{name}");
            assert_eq!(rar5_crc32(&data), crc.unwrap(), "{file}/{name} CRC");
        }
    }
}

// ── Solid chains (WinRAR 5.91 `-ma4 -s -m3`) ──────────────────────────────

#[test]
fn rar4_solid_chain_winrar591() {
    let mut archive = RarArchive::open(&format!("{W591}c_solid.rar")).expect("open c_solid");
    let snaps = snapshots(&archive);
    assert_eq!(snaps.len(), 4);
    // WinRAR sorts solid archives by name: a.txt, b.txt, repeat.txt, random.bin.
    assert_eq!(snaps[0].0, "a.txt");
    assert_eq!(snaps[3].0, "random.bin");

    // Head member, a middle member (decodes through the shared window) and
    // the tail member.
    assert_eq!(
        archive.read("a.txt").expect("head member"),
        b"text-only content line one\n"
    );
    assert_eq!(
        archive.read("b.txt").expect("middle solid member"),
        b"second file content here\n"
    );
    assert_eq!(
        archive.read("repeat.txt").expect("solid member"),
        repeat_content()
    );
    let random = archive.read("random.bin").expect("tail solid member");
    assert_eq!(random.len(), 65_536);
    assert_eq!(rar5_crc32(&random), snaps[3].2.unwrap());
}

// ── Legacy RAR 3.0 archives (rars fixture corpus) ─────────────────────────

#[test]
fn rar4_rar300_compressed_text() {
    let mut archive =
        RarArchive::open(&format!("{RAR300}compressed_text_rar300.rar")).expect("open");
    let (name, size, crc) = snapshots(&archive).into_iter().next().unwrap();
    assert_eq!(name, "text.txt");
    assert_eq!(size, 2_400);
    let data = archive.read("text.txt").expect("decode RAR3.0 m3 text");
    assert_eq!(rar5_crc32(&data), crc.unwrap());
}

#[test]
fn rar4_rar300_solid_archives() {
    for file in ["solid_simple_rar300.rar", "solid_rar300.rar"] {
        let mut archive = RarArchive::open(&format!("{RAR300}{file}")).expect("open");
        let snaps = snapshots(&archive);
        assert!(!snaps.is_empty());
        for (name, size, crc) in snaps {
            let data = archive.read(&name).expect("decode solid member");
            assert_eq!(data.len() as u64, size, "{file}/{name} size");
            assert_eq!(rar5_crc32(&data), crc.unwrap(), "{file}/{name} CRC");
        }
    }
}

// ── Encryption (`-p`, RAR30 AES) ──────────────────────────────────────────

#[test]
fn rar4_encrypted_members_need_password() {
    let mut archive = RarArchive::open(&format!("{W591}c_pw.rar")).expect("open");
    match archive.read("repeat.txt") {
        Err(RarError::Encrypted(msg)) => assert!(msg.contains("no password")),
        other => panic!("expected Encrypted without password, got {other:?}"),
    }
}

#[test]
fn rar4_encrypted_compressed_members_decode_with_password() {
    let mut archive = RarArchive::open(&format!("{W591}c_pw.rar")).expect("open");
    archive.set_password("pass123");
    assert_eq!(
        archive.read("repeat.txt").expect("decrypt + decode"),
        repeat_content()
    );
    let random = archive.read("random.bin").expect("decrypt + decode random");
    assert_eq!(random.len(), 65_536);
}

#[test]
fn rar4_wrong_password_fails() {
    let mut archive = RarArchive::open(&format!("{W591}c_pw.rar")).expect("open");
    archive.set_password("not-the-password");
    assert!(archive.read("repeat.txt").is_err());
}

// ── Feature gates (clear errors, not garbage) ─────────────────────────────

#[test]
fn rar4_vm_filtered_members_report_unsupported() {
    let mut archive = RarArchive::open(&format!("{RAR300}rarvm_x86_e8_rar300.rar")).expect("open");
    match archive.read("x86_e8_stream.bin") {
        Err(RarError::Unsupported(msg)) => assert!(
            msg.contains("filter"),
            "expected a VM-filter message, got: {msg}"
        ),
        other => panic!("expected Unsupported(VM filter), got {other:?}"),
    }
}

// ── PPMd (RAR 3.0 m5 members from the rars fixture corpus) ────────────────

#[test]
fn rar4_ppmd_members_decode() {
    // A 127 KiB lorem member compressed with PPMd by genuine RAR 3.0.
    let mut archive = RarArchive::open(&format!("{RAR300}ppmd_lorem_rar300.rar")).expect("open");
    let (name, size, crc) = snapshots(&archive).into_iter().next().unwrap();
    assert_eq!(name, "lorem_127k.txt");
    assert_eq!(size, 130_048);
    let data = archive.read("lorem_127k.txt").expect("decode PPMd member");
    assert_eq!(data.len(), 130_048);
    assert_eq!(rar5_crc32(&data), crc.unwrap(), "PPMd lorem CRC");
}

#[test]
fn rar4_ppmd_mixed_and_escape_fixtures() {
    for (file, member) in [
        ("ppmd_mixed_rar300.rar", "binary_64k.bin"),
        ("ppmd_escape_rar300.rar", "escape_64k.bin"),
        ("ppmd_lz_match_rar300.rar", "repeated_phrase_64k.txt"),
    ] {
        let mut archive = RarArchive::open(&format!("{RAR300}{file}")).expect("open");
        let snaps = snapshots(&archive);
        for (name, size, crc) in &snaps {
            let data = archive.read(name).expect("decode PPMd member");
            assert_eq!(data.len() as u64, *size, "{file}/{name} size");
            assert_eq!(rar5_crc32(&data), crc.unwrap(), "{file}/{name} CRC");
        }
        let names: Vec<&str> = snaps.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&member), "{file} member shape");
    }
}

#[test]
fn rar4_ppmd_solid_chain_decodes() {
    // Two 96 KiB lorem files in one solid PPMd chain: the second member
    // references the first through the shared window.
    let mut archive = RarArchive::open(&format!("{RAR300}ppmd_solid_rar300.rar")).expect("open");
    let snaps = snapshots(&archive);
    assert_eq!(snaps.len(), 2);
    for (name, size, crc) in snaps {
        let data = archive.read(&name).expect("decode solid PPMd member");
        assert_eq!(data.len() as u64, size, "{name} size");
        assert_eq!(rar5_crc32(&data), crc.unwrap(), "{name} CRC");
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn rar5_crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}
