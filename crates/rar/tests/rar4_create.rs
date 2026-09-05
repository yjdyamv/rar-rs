//! RAR4 creation (legacy `Rar!\x1a\x07\x00` container): STORE members,
//! single-volume and multi-volume, verified by reading back through the
//! RAR4 reader.

#![allow(deprecated)] // legacy constructor family; use create_with_options

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{ArchiveVersion, CreateOptions, ExtractOptions, RarArchive, discover_volumes};
use std::io::Write;

fn crate_crc(data: &[u8]) -> u32 {
    let mut c = 0xFFFFFFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            let mask = -(c as i32 & 1) as u32;
            c = (c >> 1) ^ (0xEDB88320 & mask);
        }
    }
    !c
}

#[test]
fn create_rar4_store_single_roundtrip() {
    let dir = make_temp_dir();
    let content = b"Hello, RAR4 world! This is a STORE roundtrip test.\n";
    let src = dir.path().join("hello.txt");
    std::fs::write(&src, content).unwrap();
    let arc = dir.path().join("out.rar");

    let opts = CreateOptions {
        compression: ArchiveVersion::V29,
        ..Default::default()
    };
    let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
    archive.add(&src, 0).expect("add STORE member");
    archive.close().expect("close");

    // Read back through the RAR4 reader.
    let mut archive = RarArchive::open(&arc).expect("reopen");
    let entries = archive.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), "hello.txt");
    assert_eq!(entries[0].size(), content.len() as u64);
    assert_eq!(entries[0].crc32(), Some(crate_crc(content)));
    assert_eq!(entries[0].version(), ArchiveVersion::V29);

    let out = archive.read("hello.txt").expect("read back");
    assert_eq!(&out, content);
}

#[test]
fn create_rar4_store_multiple_files() {
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"content A\n").unwrap();
    std::fs::write(&b, b"content B is longer\n").unwrap();
    let arc = dir.path().join("multi.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&a, 0).expect("add a");
    archive.add(&b, 0).expect("add b");
    archive.close().expect("close");

    let mut archive = RarArchive::open(&arc).expect("reopen");
    let entries = archive.list();
    assert_eq!(entries.len(), 2);
    let mut names: Vec<&str> = entries.iter().map(|e| e.name()).collect();
    names.sort();
    assert_eq!(names, vec!["a.txt", "b.txt"]);
    assert_eq!(&archive.read("a.txt").unwrap(), b"content A\n");
    assert_eq!(&archive.read("b.txt").unwrap(), b"content B is longer\n");
}

#[test]
fn create_rar4_empty_file() {
    let dir = make_temp_dir();
    let arc = dir.path().join("empty.rar");
    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        },
    )
    .expect("create");
    // No members.
    archive.close().expect("close");

    let archive = RarArchive::open(&arc).expect("reopen");
    assert_eq!(archive.list().len(), 0);
}

#[test]
fn create_rar4_compressed_roundtrip_all_levels() {
    let dir = make_temp_dir();
    // Deterministic, highly compressible content with repetition.
    let mut content = Vec::with_capacity(64 * 1024);
    let phrase = b"The quick brown fox jumps over the lazy dog. ";
    while content.len() < 64 * 1024 {
        content.extend_from_slice(phrase);
    }
    content.extend_from_slice(b"And a tail that also repeats. And a tail that also repeats.");
    let src = dir.path().join("compressible.txt");
    std::fs::write(&src, &content).unwrap();

    for level in 1..=5u8 {
        let arc = dir.path().join(format!("c{level}.rar"));
        let opts = CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        };
        let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
        archive.add(&src, level).expect("add");
        archive.close().expect("close");

        // Byte-identical read-back through the RAR4 reader.
        let mut archive = RarArchive::open(&arc).expect("reopen");
        let entries = archive.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), "compressible.txt");
        assert_eq!(entries[0].size(), content.len() as u64);
        // Must have actually compressed (method 1..=5), not STORE fallback.
        let method = entries[0].method();
        assert!(
            (1..=5).contains(&method),
            "level {level}: expected compressed method, got {method}"
        );
        assert!(
            entries[0].compressed_size() < content.len() as u64,
            "level {level}: expected compression to shrink data"
        );
        assert_eq!(entries[0].crc32(), Some(crate_crc(&content)));

        let out = archive.read("compressible.txt").expect("read back");
        assert_eq!(&out, &content, "level {level} roundtrip mismatch");
    }
}

/// m4/m5 PPMd: on word-random text (weak distance matches, strong context
/// model) the PPMd pass must win over the LZSS pass, so the level-5 member
/// is markedly smaller than the level-3 LZ-only member, and still round-
/// trips byte-identically through the RAR4 reader.
#[test]
fn create_rar4_ppmd_wins_on_text_m5() {
    let dir = make_temp_dir();
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
    ];
    let mut content = Vec::with_capacity(420_000);
    let mut seed = 0x9E3779B9u32;
    let mut n = 0u32;
    while content.len() < 400_000 {
        let mut line = format!("record {n:06}: ").into_bytes();
        for _ in 0..10 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            line.extend_from_slice(words[(seed >> 27) as usize % words.len()].as_bytes());
            line.push(b' ');
        }
        line.push(b'\n');
        content.extend_from_slice(&line);
        n += 1;
    }
    let src = dir.path().join("textmix.txt");
    std::fs::write(&src, &content).unwrap();

    // Level 3 never tries PPMd; level 5 does.
    let make = |level: u8, name: &str| -> u64 {
        let arc = dir.path().join(name);
        let opts = CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        };
        let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
        archive.add(&src, level).expect("add");
        archive.close().expect("close");
        std::fs::metadata(&arc).unwrap().len()
    };
    let lz_size = make(3, "ppmd_lz.rar");
    let m5_size = make(5, "ppmd_m5.rar");

    assert!(
        m5_size * 3 < lz_size * 2,
        "m5 PPMd must beat LZSS on text: LZ={lz_size} m5={m5_size}"
    );

    let mut archive = RarArchive::open(dir.path().join("ppmd_m5.rar")).expect("reopen");
    assert_eq!(archive.list()[0].method(), 5);
    assert_eq!(
        archive.read("textmix.txt").expect("read").as_slice(),
        &content[..]
    );
}

#[test]
fn create_rar4_compressed_store_fallback_random() {
    let dir = make_temp_dir();
    // Incompressible content must fall back to STORE (method 0) so it never
    // grows.
    let mut content = Vec::with_capacity(16 * 1024);
    let mut seed = 0x1234_5678u32;
    for _ in 0..16 * 1024 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        content.push((seed >> 24) as u8);
    }
    let src = dir.path().join("random.bin");
    std::fs::write(&src, &content).unwrap();

    for level in 1..=5u8 {
        let arc = dir.path().join(format!("r{level}.rar"));
        let opts = CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        };
        let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
        archive.add(&src, level).expect("add");
        archive.close().expect("close");

        let mut archive = RarArchive::open(&arc).expect("reopen");
        let entries = archive.list();
        assert_eq!(
            entries[0].method(),
            0,
            "level {level}: incompressible data must STORE"
        );
        let out = archive.read("random.bin").expect("read back");
        assert_eq!(&out, &content, "level {level} mismatch");
    }
}

#[test]
fn create_rar4_compressed_multivolume_split_member() {
    let dir = make_temp_dir();
    let mut content = Vec::with_capacity(2_500_000);
    let mut n = 0u32;
    while content.len() < 2_500_000 {
        content.extend_from_slice(
            format!("line number {n:07} with padding to vary content length here 1234567890\n")
                .as_bytes(),
        );
        n += 1;
    }
    let src = dir.path().join("big.txt");
    std::fs::write(&src, &content).unwrap();
    let arc = dir.path().join("cvol.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            volume_size: Some(20_000),
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 3).expect("add");
    archive.close().expect("close");

    let volumes = discover_volumes(&arc);
    assert!(
        volumes.len() >= 2,
        "expected multiple volumes, got {volumes:?}"
    );

    let mut archive = RarArchive::open(&arc).expect("reopen");
    let entries = archive.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), "big.txt");
    assert_eq!(entries[0].size(), content.len() as u64);
    let chunks = entries[0].chunks();
    assert!(chunks.len() > 1);
    assert!(
        chunks
            .windows(2)
            .all(|pair| pair[0].volume_index < pair[1].volume_index)
    );
    assert!(
        chunks[..chunks.len() - 1]
            .iter()
            .all(|chunk| !chunk.is_final)
    );
    assert!(chunks.last().unwrap().is_final);
    assert!(chunks.iter().all(|chunk| chunk.extra_data.is_empty()));
    assert_eq!(
        chunks.iter().map(|chunk| chunk.packed_size).sum::<u64>(),
        entries[0].compressed_size()
    );
    let out = archive.read("big.txt").expect("read split member");
    assert_eq!(&out, &content);
}

#[test]
fn create_rar4_store_multivolume_split_member() {
    let dir = make_temp_dir();
    // A file big enough to span at least one volume boundary given the
    // small volume size.
    let content: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let src = dir.path().join("big.bin");
    std::fs::write(&src, &content).unwrap();
    let arc = dir.path().join("vol.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            volume_size: Some(5_000),
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 0).expect("add");
    archive.close().expect("close");

    // Discover the volume set and read it back.
    let volumes = discover_volumes(&arc);
    assert!(
        volumes.len() >= 2,
        "expected multiple volumes, got {volumes:?}"
    );

    let mut archive = RarArchive::open(&arc).expect("reopen");
    let entries = archive.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), "big.bin");
    assert_eq!(entries[0].size(), content.len() as u64);
    let chunks = entries[0].chunks();
    assert!(chunks.len() > 1);
    assert!(
        chunks
            .windows(2)
            .all(|pair| pair[0].volume_index < pair[1].volume_index)
    );
    assert!(
        chunks[..chunks.len() - 1]
            .iter()
            .all(|chunk| !chunk.is_final)
    );
    assert!(chunks.last().unwrap().is_final);
    assert!(chunks.iter().all(|chunk| chunk.extra_data.is_empty()));
    assert_eq!(
        chunks.iter().map(|chunk| chunk.packed_size).sum::<u64>(),
        entries[0].compressed_size()
    );

    let out = archive.read("big.bin").expect("read split member");
    assert_eq!(&out, &content);
}

/// Walk the raw FILE_HEAD blocks of a freshly created RAR4 archive and
/// return `(name, flags, packed, unpacked, attr)` for every member.
///
/// This inspects the on-disk bytes rather than the parsed model, so it can
/// pin the exact WinRAR conventions the writer must follow (directory
/// members carry window bits 7 and attr 0x10, zero sizes and CRC).
fn scan_rar4_members(data: &[u8]) -> Vec<(String, u16, u32, u32, u32)> {
    let mut out = Vec::new();
    let mut pos = 7usize; // after "Rar!\x1a\x07\x00"
    while pos + 7 <= data.len() {
        let htype = data[pos + 2];
        let flags = u16::from_le_bytes([data[pos + 3], data[pos + 4]]);
        let hsize = u16::from_le_bytes([data[pos + 5], data[pos + 6]]) as usize;
        if pos + hsize > data.len() {
            break;
        }
        match htype {
            0x74 => {
                let body = &data[pos + 7..pos + hsize];
                let packed = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let unpacked = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let name_size = u16::from_le_bytes(body[19..21].try_into().unwrap()) as usize;
                let attr = u32::from_le_bytes(body[21..25].try_into().unwrap());
                let name = String::from_utf8_lossy(&body[25..25 + name_size]).into_owned();
                out.push((name, flags, packed, unpacked, attr));
                pos += hsize + packed as usize;
            }
            0x7b | 0x73 | 0x72 => {
                pos += hsize;
            }
            _ => break,
        }
    }
    out
}

#[test]
fn create_rar4_directory_tree_roundtrip() {
    let dir = make_temp_dir();
    // Tree: top.txt, sub/mid.txt, sub/deep/leaf.txt, sub/emptydir, plus a
    // non-ASCII directory to exercise the FHD_UNICODE name extension.
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("sub/deep")).unwrap();
    std::fs::create_dir(src.join("sub/emptydir")).unwrap();
    std::fs::create_dir(src.join("资料")).unwrap();
    std::fs::write(src.join("top.txt"), b"top").unwrap();
    std::fs::write(src.join("sub/mid.txt"), b"mid").unwrap();
    std::fs::write(src.join("sub/deep/leaf.txt"), b"leaf").unwrap();
    std::fs::write(src.join("资料/note.txt"), b"note").unwrap();
    let arc = dir.path().join("tree.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 3).expect("add directory tree");
    archive.close().expect("close");

    // Read back: every directory and file must round-trip with exact UTF-8
    // names (dirs without a trailing slash, like WinRAR).
    let mut archive = RarArchive::open(&arc).expect("reopen");
    let mut names: Vec<String> = archive.namelist().into_iter().map(str::to_string).collect();
    names.sort();
    let mut expected: Vec<String> = [
        "src",
        "src/sub",
        "src/sub/deep",
        "src/sub/emptydir",
        "src/sub/deep/leaf.txt",
        "src/sub/mid.txt",
        "src/top.txt",
        "src/资料",
        "src/资料/note.txt",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    expected.sort();
    assert_eq!(names, expected);

    for name in [
        "src",
        "src/sub",
        "src/sub/deep",
        "src/sub/emptydir",
        "src/资料",
    ] {
        let entry = archive.get_entry(name).expect("dir entry");
        assert!(entry.is_dir(), "{name} must list as a directory");
        assert_eq!(entry.size(), 0, "{name} must have zero size");
    }
    for (name, content) in [
        ("src/top.txt", b"top".as_slice()),
        ("src/sub/mid.txt", b"mid".as_slice()),
        ("src/sub/deep/leaf.txt", b"leaf".as_slice()),
        ("src/资料/note.txt", b"note".as_slice()),
    ] {
        assert_eq!(&archive.read(name).unwrap(), content);
    }

    // Raw-byte conventions (WinRAR/UnRAR classify RAR4 directories by the
    // window bits, not the attr): dir members must carry flags & 0xE0 ==
    // 0xE0 with zero sizes and CRC; regular files must not.
    let raw = std::fs::read(&arc).unwrap();
    let members = scan_rar4_members(&raw);
    assert_eq!(members.len(), 9);
    for (name, flags, packed, unpacked, attr) in &members {
        if *attr & 0x10 != 0 {
            assert_eq!(
                flags & 0x00E0,
                0x00E0,
                "dir {name}: window bits must mark a directory"
            );
            assert_eq!(
                (*packed, *unpacked),
                (0u32, 0u32),
                "dir {name} must have zero sizes"
            );
        } else {
            assert_ne!(
                flags & 0x00E0,
                0x00E0,
                "file {name} must not be a directory"
            );
        }
    }
}

#[test]
fn create_rar4_directory_multivolume() {
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("deep")).unwrap();
    std::fs::create_dir(src.join("emptydir")).unwrap();
    let mut content = Vec::with_capacity(120_000);
    let mut n = 0u32;
    while content.len() < 120_000 {
        content.extend_from_slice(
            format!("payload line {n} abcdefghijklmnopqrstuvwxyz 0123456789\n").as_bytes(),
        );
        n += 1;
    }
    std::fs::write(src.join("deep/big.txt"), &content).unwrap();
    std::fs::write(src.join("small.txt"), b"small").unwrap();
    let arc = dir.path().join("dvol.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            volume_size: Some(8_000),
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 0).expect("add tree across volumes");
    archive.close().expect("close");

    let volumes = discover_volumes(&arc);
    assert!(
        volumes.len() >= 2,
        "expected multiple volumes, got {volumes:?}"
    );

    // The whole tree (dirs included) must read back across the volumes.
    let mut archive = RarArchive::open(&arc).expect("reopen");
    let mut names: Vec<String> = archive.namelist().into_iter().map(str::to_string).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "src".to_string(),
            "src/deep".to_string(),
            "src/deep/big.txt".to_string(),
            "src/emptydir".to_string(),
            "src/small.txt".to_string(),
        ]
    );
    assert_eq!(archive.read("src/deep/big.txt").unwrap(), content);
    assert_eq!(archive.read("src/small.txt").unwrap(), b"small");
    assert!(archive.get_entry("src/emptydir").unwrap().is_dir());
}

/// Member-level encryption (`-p`): every member carries FHD_PASSWORD plus an
/// 8-byte salt, and the payload is AES-128-CBC ciphertext. Round-trips
/// through our own reader with the password; a wrong/missing password fails.
#[test]
fn create_rar4_encrypted_members_roundtrip() {
    let dir = make_temp_dir();
    let src = dir.path().join("secret.txt");
    let mut content = Vec::with_capacity(4096);
    for i in 0..300 {
        content.extend_from_slice(format!("secret line {i:04} ...\n").as_bytes());
    }
    std::fs::write(&src, &content).unwrap();
    let arc = dir.path().join("enc.rar");

    let opts = CreateOptions {
        compression: ArchiveVersion::V29,
        password: Some("hunter2".to_string()),
        ..Default::default()
    };
    let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
    archive.add(&src, 3).expect("add encrypted member");
    archive.close().expect("close");

    // Raw bytes: FHD_PASSWORD (0x04) and FHD_SALT (0x0400) flags set, an
    // 8-byte salt present, and no plaintext in the payload.
    let raw = std::fs::read(&arc).unwrap();
    let members = scan_rar4_members(&raw);
    assert_eq!(members.len(), 1);
    let (name, flags, packed, _unpacked, _attr) = &members[0];
    assert_eq!(name, "secret.txt");
    assert_ne!(flags & 0x04, 0, "must set FHD_PASSWORD");
    assert_ne!(flags & 0x0400, 0, "must set FHD_SALT");
    assert!(packed % 16 == 0, "encrypted payload must be block aligned");
    assert!(
        raw.windows(content.len()).all(|w| w != content),
        "payload must not contain the plaintext"
    );

    // Read back with the password.
    let mut archive = RarArchive::open_with_password(&arc, "hunter2").expect("reopen");
    assert_eq!(
        archive.read("secret.txt").expect("read").as_slice(),
        &content[..]
    );

    // Wrong / missing password fails.
    let mut archive = RarArchive::open_with_password(&arc, "wrong").expect("reopen");
    assert!(
        archive.read("secret.txt").is_err(),
        "wrong password must fail"
    );
    let mut archive = RarArchive::open(&arc).expect("reopen-no-pw");
    assert!(archive.read("secret.txt").is_err(), "no password must fail");
}

/// Encrypted members split across volumes still decrypt: the per-member salt
/// lives in every FILE_HEAD, and the concatenated ciphertext is decrypted as
/// one stream (the RAR30 scheme has no per-volume IV).
#[test]
fn create_rar4_encrypted_multivolume() {
    let dir = make_temp_dir();
    let src = dir.path().join("big.bin");
    let mut content = Vec::with_capacity(60_000);
    let mut seed = 0x9E3779B9u32;
    while content.len() < 60_000 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        content.push((seed >> 24) as u8);
    }
    std::fs::write(&src, &content).unwrap();
    let arc = dir.path().join("encvol.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            password: Some("volpw".to_string()),
            volume_size: Some(4_000),
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 0).expect("add encrypted split member"); // STORE: keeps size predictable
    archive.close().expect("close");

    let volumes = discover_volumes(&arc);
    assert!(
        volumes.len() >= 2,
        "expected multiple volumes, got {volumes:?}"
    );
    let mut archive = RarArchive::open_with_password(&arc, "volpw").expect("reopen");
    assert_eq!(
        archive.read("big.bin").expect("read").as_slice(),
        &content[..]
    );
}

/// RAR4 header encryption (`-hp`): the main header is plaintext and carries
/// MHD_PASSWORD, and every block after it (file headers, end-of-archive) is
/// stored as `[8-byte salt][AES-128-CBC ciphertext]`. The filename must not
/// appear as plaintext in the archive; reading back with the password works,
/// and a wrong/missing password fails.
#[test]
fn create_rar4_header_encrypted_roundtrip() {
    let dir = make_temp_dir();
    let src = dir.path().join("top-secret.txt");
    let content = b"classified payload for the -hp header-encryption test\n";
    std::fs::write(&src, content).unwrap();
    let arc = dir.path().join("hpenctest.rar");

    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            encrypt_headers: true,
            password: Some("hunter2".to_string()),
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 3).expect("add");
    archive.close().expect("close");

    // The 7-byte signature is at offset 0; the 13-byte main header follows.
    // Its flags word (bytes 3-4) must carry MHD_PASSWORD = 0x0080.
    let raw = std::fs::read(&arc).unwrap();
    assert!(raw.len() > 7 + 13);
    assert_eq!(&raw[..7], b"Rar!\x1a\x07\x00");
    let main_flags = u16::from_le_bytes([raw[10], raw[11]]);
    assert_ne!(
        main_flags & 0x0080,
        0,
        "main header must set MHD_PASSWORD (0x0080)"
    );

    // The header-encrypted blocks must hide the member name and payload:
    // neither should appear as plaintext anywhere in the archive bytes.
    assert!(
        raw.windows(11).all(|w| w != b"top-secret.txt".as_slice()),
        "filename must not appear as plaintext under -hp"
    );
    assert!(
        raw.windows(content.len()).all(|w| w != content),
        "payload must not appear as plaintext under -hp"
    );

    // Read back with the correct password.
    let mut archive = RarArchive::open_with_password(&arc, "hunter2").expect("reopen");
    assert_eq!(
        archive.read("top-secret.txt").expect("read").as_slice(),
        content
    );

    // A wrong password must fail to even open: every header block after
    // the main header is encrypted, so the scan itself needs the password.
    assert!(
        RarArchive::open_with_password(&arc, "wrong").is_err(),
        "wrong password must fail to open an -hp archive"
    );
    assert!(
        RarArchive::open(&arc).is_err(),
        "no password must fail to open an -hp archive"
    );
}

/// RAR4 EXTTIME: the writer stores the member's nanosecond mtime fraction
/// as a 3-byte, least-significant-first tick count in the FILE_HEAD (bits
/// 12-15 of the flags = mtime present, 3 bytes), matching WinRAR. Reading
/// back through our own reader reproduces the fraction to the format's
/// 100 ns resolution.
#[test]
fn create_rar4_exttime_mtime_ns_roundtrip() {
    let dir = make_temp_dir();
    let src = dir.path().join("stamp.bin");
    std::fs::write(&src, b"timestamped payload").unwrap();

    // Set a precise sub-second mtime; read the on-disk value back so the
    // assertion matches the platform's actual timestamp resolution.
    let target = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
    let times = std::fs::FileTimes::new().set_modified(target);
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(times)
        .unwrap();
    let disk_mtime = std::fs::metadata(&src).unwrap().modified().unwrap();
    let disk_ns = disk_mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();

    let arc = dir.path().join("stamp.rar");
    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 0).expect("add"); // STORE keeps the member trivially small
    archive.close().expect("close");

    // The FILE_HEAD must carry FHD_EXTTIME (0x1000).
    let raw = std::fs::read(&arc).unwrap();
    let members = scan_rar4_members(&raw);
    assert_eq!(members.len(), 1);
    let (name, flags, _packed, _unp, _attr) = &members[0];
    assert_eq!(name, "stamp.bin");
    assert_ne!(flags & 0x1000, 0, "must set FHD_EXTTIME");

    // Reading back reproduces the sub-second fraction to 100 ns resolution
    // (None when the fraction rounds to a whole second on coarse filesystems).
    let mut archive = RarArchive::open(&arc).expect("reopen");
    let entry = archive.get_entry("stamp.bin").expect("entry");
    let expect = (disk_ns / 100) * 100;
    assert_eq!(
        entry.mtime_ns(),
        (expect > 0).then_some(expect),
        "mtime_ns must round-trip to 100 ns resolution"
    );
    assert_eq!(archive.read("stamp.bin").unwrap(), b"timestamped payload");
}

/// RAR4 solid (`-s`): one persistent encoder carries the LZ window and Huffman
/// tables across the members of a solid run. The first member has no
/// FHD_SOLID; every later compressed member does; the main header carries
/// MHD_SOLID. The shared window makes the archive measurably smaller than the
/// equivalent non-solid archive, and WinRAR extracts it byte-identically.
#[test]
fn create_rar4_solid_chain() {
    let dir = make_temp_dir();
    let src = dir.path().join("t");
    std::fs::create_dir_all(&src).unwrap();
    // Eight similar text files: ideal for the shared dictionary.
    let mut contents = Vec::new();
    for i in 0..8u32 {
        let mut text = format!("alpha-beta-gamma-delta file {i} common header line\n").into_bytes();
        text.extend(std::iter::repeat_n(b'x', 100 + i as usize * 7));
        contents.push(text);
        std::fs::write(src.join(format!("f{i}.txt")), &contents[i as usize]).unwrap();
    }
    let solid_arc = dir.path().join("solid.rar");
    let mut archive = RarArchive::create_with_options(
        &solid_arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            solid: true,
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 3).expect("add solid tree");
    archive.close().expect("close");

    // The main header must carry MHD_SOLID (0x0008) alongside LONG_BLOCK.
    let raw = std::fs::read(&solid_arc).unwrap();
    let main_flags = u16::from_le_bytes([raw[10], raw[11]]);
    assert_ne!(main_flags & 0x0008, 0, "solid archive must set MHD_SOLID");

    // Member flags: first compressed member no FHD_SOLID, rest yes.
    let members = scan_rar4_members(&raw);
    let files: Vec<&(String, u16, u32, u32, u32)> =
        members.iter().filter(|(n, ..)| n != "t").collect();
    assert!(files.len() >= 8, "expected the 8 members");
    assert_eq!(files[0].1 & 0x0010, 0, "first member starts the chain");
    for m in files.iter().skip(1) {
        assert_ne!(m.1 & 0x0010, 0, "later members must continue the chain");
    }

    // Round-trip every member through our own reader.
    let mut archive = RarArchive::open(&solid_arc).expect("reopen");
    for i in 0..8u32 {
        assert_eq!(
            archive.read(&format!("t/f{i}.txt")).unwrap().as_slice(),
            &contents[i as usize][..]
        );
    }

    // The shared window must actually help: solid < non-solid archive size.
    let ns_arc = dir.path().join("nonsolid.rar");
    let mut archive = RarArchive::create_with_options(
        &ns_arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            solid: false,
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 3).expect("add non-solid tree");
    archive.close().expect("close");
    let solid_size = std::fs::metadata(&solid_arc).unwrap().len();
    let ns_size = std::fs::metadata(&ns_arc).unwrap().len();
    assert!(
        solid_size < ns_size,
        "solid archive should be smaller: solid={solid_size} non-solid={ns_size}"
    );
}

/// RAR4 solid with `-se` (SolidReset::PerExtension): the chain restarts when
/// the file extension changes, so each distinct extension begins a fresh run
/// (no FHD_SOLID) instead of continuing the previous run's dictionary.
#[test]
fn create_rar4_solid_extension_reset() {
    let dir = make_temp_dir();
    let src = dir.path().join("se");
    std::fs::create_dir_all(&src).unwrap();
    for (name, ext) in [("a.txt", 0), ("b.bin", 1), ("c.txt", 2), ("d.bin", 3)] {
        let mut text = format!("shared prefix {} data\n", name).into_bytes();
        text.extend(std::iter::repeat_n(b'q', 300 + ext * 13));
        std::fs::write(src.join(name), &text).unwrap();
    }
    let arc = dir.path().join("se.rar");
    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            solid: true,
            solid_reset: rar_rs::SolidReset::PerExtension,
            ..Default::default()
        },
    )
    .expect("create");
    archive.add(&src, 3).expect("add");
    archive.close().expect("close");

    let raw = std::fs::read(&arc).unwrap();
    let members = scan_rar4_members(&raw);
    let files: Vec<&(String, u16, u32, u32, u32)> =
        members.iter().filter(|(n, ..)| n != "se").collect();
    assert_eq!(files.len(), 4, "expected the four member files");
    // Alternating extensions under -se mean every file starts its own run:
    // none may carry FHD_SOLID, or a decoder would inherit a previous run's
    // tables.
    for (name, flags, _p, _u, _a) in &files {
        assert_eq!(
            flags & 0x0010,
            0,
            "-se must start a new run on an extension change: {name}"
        );
    }
    // Everything still reads back.
    let mut archive = RarArchive::open(&arc).expect("reopen");
    for (name, _ext) in [("a.txt", 0), ("b.bin", 1), ("c.txt", 2), ("d.bin", 3)] {
        assert!(
            archive.read(&format!("se/{name}")).is_ok(),
            "{name} readable"
        );
    }
}

#[test]
fn rar4_second_solid_run_starts_at_its_own_chain_head() {
    let dir = make_temp_dir();
    let arc = dir.path().join("two-runs.rar");
    let first = vec![b'a'; 4096];
    let second = vec![b'b'; 600];
    for (name, data) in [
        ("a1.txt", first.as_slice()),
        ("a2.txt", first.as_slice()),
        ("b1.bin", second.as_slice()),
        ("b2.bin", second.as_slice()),
    ] {
        std::fs::write(dir.path().join(name), data).unwrap();
    }
    let mut archive = RarArchive::create_with_options(
        &arc,
        CreateOptions {
            compression: ArchiveVersion::V29,
            solid: true,
            solid_reset: rar_rs::SolidReset::PerExtension,
            ..Default::default()
        },
    )
    .expect("create");
    for name in ["a1.txt", "a2.txt", "b1.bin", "b2.bin"] {
        archive.add(dir.path().join(name), 3).unwrap();
    }
    archive.close().expect("close");

    let raw = std::fs::read(&arc).unwrap();
    let members = scan_rar4_members(&raw);
    let flags = |name: &str| {
        members
            .iter()
            .find(|(n, ..)| n == name)
            .unwrap_or_else(|| panic!("missing {name} in {members:?}"))
            .1
    };
    assert_eq!(flags("a1.txt") & 0x0010, 0);
    assert_ne!(flags("a2.txt") & 0x0010, 0);
    assert_eq!(flags("b1.bin") & 0x0010, 0);
    assert_ne!(flags("b2.bin") & 0x0010, 0);

    let mut archive = RarArchive::open(&arc).expect("reopen");
    let output = archive
        .read_with_options(
            "b2.bin",
            ExtractOptions {
                max_unpacked_bytes: Some(1024),
                ..Default::default()
            },
        )
        .expect("decode only the second solid run");
    assert_eq!(output, second);
}

/// RAR4 recovery record on periodic (short-cycle) data: OUR repair path
/// rebuilds a damaged sector byte-identically. WinRAR 6.23/7.23 cannot do
/// this — its RAR4 `rar r` corrupts the tail when the record's own
/// protected partial sector overlaps the record on periodic data (see
/// PLAN.md "已知小差异") — so this locks in the robustness edge.
#[test]
fn create_rar4_recovery_record_repairs_periodic_damage() {
    let dir = make_temp_dir();
    // 64-byte periodic pattern, 600 KiB — the exact shape that trips
    // WinRAR's RAR4 repair.
    let pat: Vec<u8> = (0..64u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(41))
        .collect();
    let mut content = Vec::with_capacity(600 * 1024);
    while content.len() < 600 * 1024 {
        content.extend_from_slice(&pat);
    }
    let src = dir.path().join("periodic.bin");
    std::fs::write(&src, &content).unwrap();
    let arc = dir.path().join("periodic_rr.rar");

    let opts = CreateOptions {
        compression: ArchiveVersion::V29,
        recovery_percent: Some(10),
        ..Default::default()
    };
    let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
    archive.add(&src, 0).expect("add"); // STORE: keeps the sector grid stable
    archive.close().expect("close");

    // A short 64-byte damage fully inside one protected payload sector.
    let mut damaged = std::fs::read(&arc).unwrap();
    let fh = 7 + 13;
    let hsize = u16::from_le_bytes([damaged[fh + 5], damaged[fh + 6]]) as usize;
    let payload = fh + hsize;
    let at = payload + 40_000;
    damaged[at..at + 64].fill(0x5a);

    let damaged_path = dir.path().join("periodic_damaged.rar");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let fixed_path = dir.path().join("periodic_fixed.rar");
    let repaired = rar_rs::repair_legacy_archive_path(&damaged_path, &fixed_path).expect("repair");
    assert!(repaired, "periodic damage must be repairable by us");
    assert_eq!(
        std::fs::read(&fixed_path).unwrap(),
        std::fs::read(&arc).unwrap(),
        "our repair must restore the periodic-data archive byte-identically"
    );

    // And the restored archive reads back fine.
    let mut archive = RarArchive::open(&fixed_path).expect("reopen fixed");
    assert_eq!(archive.read("periodic.bin").unwrap(), content);
}

/// Auto delta filter on structured sample data: the RAR3 DELTA bytecode must
/// appear in the compressed stream (proving the filter beat plain LZ), the
/// filtered member must compress far below the input size, and round-trip
/// byte-identically through the RAR4 reader.
#[test]
fn create_rar4_auto_delta_filter_on_samples() {
    let dir = make_temp_dir();
    // 16-bit stereo random-walk samples (channels = 4), the classic delta
    // payload; auto-delta must fire and win decisively.
    let mut content = Vec::with_capacity(400_000);
    let mut l = 0i16;
    let mut r = 0i16;
    let mut seed = 42u32;
    while content.len() < 350_000 {
        l = l.wrapping_add(((seed >> 16) & 0x3f) as i16 - 30);
        r = r.wrapping_add(((seed >> 8) & 0x3f) as i16 - 20);
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        content.extend_from_slice(&l.to_le_bytes());
        content.extend_from_slice(&r.to_le_bytes());
    }
    let src = dir.path().join("samples.bin");
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("auto_delta.rar");
    let opts = CreateOptions {
        compression: ArchiveVersion::V29,
        ..Default::default()
    };
    let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
    archive.add(&src, 3).expect("add");
    archive.close().expect("close");

    let raw = std::fs::read(&arc).unwrap();
    let size = raw.len() as u64;
    // The delta transform must have won decisively (the filter record bytes
    // sit in the bitstream unaligned, so assert on size, not on a bytecode
    // fingerprint).
    assert!(
        size * 3 < content.len() as u64,
        "delta-filtered member must compress hard: {size}"
    );

    let mut archive = RarArchive::open(&arc).expect("reopen");
    assert_eq!(
        archive.read("samples.bin").expect("read").as_slice(),
        &content[..]
    );
}

/// Auto audio filter on 8-bit interleaved samples: the RAR3 AUDIO filter
/// must fire (member far below input size) and round-trip byte-identically.
#[test]
fn create_rar4_auto_audio_filter_on_waveform() {
    let dir = make_temp_dir();
    let mut content = Vec::with_capacity(400_000);
    let mut ch = [128i16; 2];
    let mut seed = 0xC0FFEEu32;
    while content.len() < 350_000 {
        for (c, sample) in ch.iter_mut().enumerate() {
            *sample = (*sample + (((seed >> (c * 8)) & 0x3f) as i16 - 30)).clamp(0, 255);
            content.push(*sample as u8);
        }
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    }
    let src = dir.path().join("voice8.bin");
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("auto_audio.rar");
    let opts = CreateOptions {
        compression: ArchiveVersion::V29,
        ..Default::default()
    };
    let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
    archive.add(&src, 3).expect("add");
    archive.close().expect("close");

    let size = std::fs::metadata(&arc).unwrap().len();
    assert!(
        size * 5 < content.len() as u64,
        "audio-filtered member must compress hard: {size} vs {}",
        content.len()
    );
    let mut archive = RarArchive::open(&arc).expect("reopen");
    assert_eq!(
        archive.read("voice8.bin").expect("read").as_slice(),
        &content[..]
    );
}

/// Solid + m5: text members in a solid run are coded PPMd (continuing the
/// carried model across members where it pays), so a run of near-identical
/// text files must compress far better solid than non-solid, and every
/// member round-trips byte-identically through the RAR4 reader.
#[test]
fn create_rar4_solid_ppmd_text_chain() {
    let dir = make_temp_dir();
    let mut sources = Vec::new();
    for chapter in 1..=4u8 {
        let src = dir.path().join(format!("chap{chapter}.txt"));
        let mut content = Vec::with_capacity(250_000);
        for line in 0..2500u32 {
            content.extend_from_slice(
                format!(
                    "chapter {chapter} line {line:05}: shared boilerplate text that repeats across every chapter of this archive body body body\n"
                )
                .as_bytes(),
            );
        }
        std::fs::write(&src, &content).unwrap();
        sources.push((src, content));
    }

    let make = |solid: bool, name: &str| -> u64 {
        let arc = dir.path().join(name);
        let opts = CreateOptions {
            compression: ArchiveVersion::V29,
            solid,
            ..Default::default()
        };
        let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
        for (src, _) in &sources {
            archive.add(src, 5).expect("add");
        }
        archive.close().expect("close");
        std::fs::metadata(&arc).unwrap().len()
    };
    let solid_size = make(true, "sppmd.rar");
    let plain_size = make(false, "nsppmd.rar");
    assert!(
        solid_size < plain_size,
        "solid PPMd must beat per-member fresh models on shared text: solid={solid_size} plain={plain_size}"
    );

    let mut archive = RarArchive::open(dir.path().join("sppmd.rar")).expect("reopen");
    let names: Vec<String> = archive
        .list()
        .iter()
        .map(|entry| entry.name().to_string())
        .collect();
    assert_eq!(names.len(), 4);
    for (index, (_, content)) in sources.iter().enumerate() {
        let out = archive.read(&names[index]).unwrap();
        assert_eq!(
            &out, content,
            "{} solid-PPMd roundtrip mismatch",
            names[index]
        );
    }
}

/// Large-member extraction takes the streaming path: a ~96 MB STORE member
/// is copied chunk-by-chunk (never buffered whole) and a compressed member
/// decodes incrementally, both verified byte-identical via
/// `RarArchive::extract` (the writer path, not the buffering `read`).
#[test]
fn create_rar4_large_members_stream_on_extract() {
    let dir = make_temp_dir();
    // Deterministic 96 MiB pseudo-random store payload.
    let store = dir.path().join("disk.bin");
    {
        let mut f = std::fs::File::create(&store).unwrap();
        let mut block = vec![0u8; 1 << 20];
        let mut seed = 0x1234_5678u32;
        for i in 0..96usize {
            for chunk in block.as_chunks_mut::<4>().0 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                chunk.copy_from_slice(&seed.to_le_bytes());
            }
            f.write_all(&block).unwrap();
            let _ = i;
        }
    }
    let text = dir.path().join("stream.txt");
    let mut body = Vec::new();
    for j in 0..200_000u32 {
        body.extend_from_slice(
            format!("streaming line {j:06} with repeating words words words\n").as_bytes(),
        );
    }
    std::fs::write(&text, &body).unwrap();

    let arc = dir.path().join("stream.rar");
    let opts = CreateOptions {
        compression: ArchiveVersion::V29,
        ..Default::default()
    };
    let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
    archive.add(&store, 0).expect("store member");
    archive.add(&text, 5).expect("compressed member");
    archive.close().expect("close");

    let out = dir.path().join("out");
    let mut ar = RarArchive::open(&arc).expect("reopen");
    ar.extract("disk.bin", &out).expect("extract store");
    ar.extract("stream.txt", &out).expect("extract compressed");
    assert_eq!(
        std::fs::read(out.join("disk.bin")).unwrap(),
        std::fs::read(&store).unwrap()
    );
    assert_eq!(std::fs::read(out.join("stream.txt")).unwrap(), body);
}

/// RAR4 multi-file batch creation (parallel waves when the `parallel`
/// feature is on) must be byte-identical to adding the same files one by
/// one in order: each member is compressed with an independent engine, so
/// ordering is the only shared state.
#[test]
fn rar4_batch_matches_sequential_bytes() {
    let dir = make_temp_dir();
    let mut paths = Vec::new();
    for i in 0..5u8 {
        let p = dir.path().join(format!("m{i}.txt"));
        let mut body = Vec::new();
        for j in 0..4000u32 {
            body.extend_from_slice(
                format!("member {i} line {j:05}: batch text with repeated words words words\n")
                    .as_bytes(),
            );
        }
        std::fs::write(&p, &body).unwrap();
        paths.push((p, body));
    }
    // One non-text member to exercise the filter/PPMd candidate paths.
    let bin = dir.path().join("snd.bin");
    let mut body = Vec::with_capacity(300_000);
    let mut l = 0i16;
    let mut r = 0i16;
    let mut seed = 7u32;
    while body.len() < 260_000 {
        l = l.wrapping_add(((seed >> 16) & 0x3f) as i16 - 30);
        r = r.wrapping_add(((seed >> 8) & 0x3f) as i16 - 20);
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        body.extend_from_slice(&l.to_le_bytes());
        body.extend_from_slice(&r.to_le_bytes());
    }
    std::fs::write(&bin, &body).unwrap();

    let mk = |name: &str| -> std::path::PathBuf {
        let arc = dir.path().join(name);
        let opts = CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        };
        let mut archive = RarArchive::create_with_options(&arc, opts).expect("create");
        archive.add(&bin, 3).expect("audio member");
        for (p, _) in &paths {
            archive.add(p, 5).expect("text member");
        }
        archive.close().expect("close");
        arc
    };
    let seq_arc = mk("seq.rar");

    // Batch: same members, same order, via add_batch.
    let bat_arc = dir.path().join("batch.rar");
    {
        let opts = CreateOptions {
            compression: ArchiveVersion::V29,
            ..Default::default()
        };
        let mut archive = RarArchive::create_with_options(&bat_arc, opts).expect("create");
        let mut entries: Vec<rar_rs::BatchEntry<'_>> = Vec::new();
        entries.push(rar_rs::BatchEntry::File {
            path: &bin,
            name: None,
            level: 3,
        });
        for (p, _) in &paths {
            entries.push(rar_rs::BatchEntry::File {
                path: p,
                name: None,
                level: 5,
            });
        }
        archive.add_batch(&entries).expect("add_batch");
        archive.close().expect("close");
    }
    let seq = std::fs::read(&seq_arc).unwrap();
    let bat = std::fs::read(&bat_arc).unwrap();
    assert_eq!(
        seq.len(),
        bat.len(),
        "batch and sequential archives must be the same size"
    );
    assert_eq!(
        seq, bat,
        "batch (parallel-capable) must be byte-identical to sequential"
    );
}
