//! RAR4 creation (legacy `Rar!\x1a\x07\x00` container): STORE members,
//! single-volume and multi-volume, verified by reading back through the
//! RAR4 reader.

#![allow(deprecated)] // legacy constructor family; use create_with_options

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar5::{ArchiveVersion, CreateOptions, RarArchive, discover_volumes};

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
        format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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

    let mut archive = RarArchive::open(&dir.path().join("ppmd_m5.rar")).expect("reopen");
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
        format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
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
            format_version: ArchiveVersion::Rar40,
            solid: true,
            solid_reset: rar5::SolidReset::PerExtension,
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
