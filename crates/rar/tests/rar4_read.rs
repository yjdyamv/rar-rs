//! RAR 1.5–4.x read support: STORE passthrough plus RAR3/4 (unp_ver >= 29)
//! LZSS+Huffman decode with solid chains and RAR30 encryption.
//!
//! No archive is vendored: every case is generated on demand by
//! [`support::corpus`] from the cached official builds (see that module for
//! which release covers which container), so the assertions pin decoding
//! behaviour instead of one build's byte output.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use std::path::Path;

use rar_rs::{ArchiveReader, ArchiveVersion, OpenOptions, RarError};

use support::{corpus, generate};

// ── shared helpers ─────────────────────────────────────────────────────────

/// (name, size, crc32) snapshots so a test can release the `list()` borrow
/// before calling `read`.
fn snapshots(archive: &ArchiveReader) -> Vec<(String, u64, Option<u32>)> {
    archive
        .entries()
        .map(|e| (e.name().to_string(), e.size(), e.crc32()))
        .collect()
}

/// Decode every member of `generated` and require it to equal the payload it
/// was built from, byte for byte, with the header CRC agreeing.
fn assert_all_members_match_payload(generated: &generate::Generated) {
    let mut archive = ArchiveReader::open(generated.path()).expect("open generated archive");
    let snaps = snapshots(&archive);
    assert!(
        !snaps.is_empty(),
        "{}: no members",
        generated.archive.display()
    );
    for (name, size, crc) in snaps {
        let data = archive
            .read_entry(archive.unique_entry(&name).unwrap())
            .unwrap_or_else(|e| panic!("{name}: decode failed: {e}"));
        assert_eq!(data.len() as u64, size, "{name}: size");
        assert_eq!(data, generate::payload(&name), "{name}: content");
        assert_eq!(Some(crc32(&data)), crc, "{name}: header CRC");
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn member_versions(path: &Path) -> Vec<ArchiveVersion> {
    ArchiveReader::open(path)
        .expect("open")
        .entries()
        .map(|e| e.version())
        .collect()
}

// ── STORE ─────────────────────────────────────────────────────────────────

#[test]
fn rar4_store_list_and_read() {
    let Some(made) = corpus::store() else { return };
    let mut archive = ArchiveReader::open(made.path()).expect("open stored archive");
    assert_eq!(archive.entries().count(), 1);
    assert_eq!(archive.entries().next().unwrap().name(), "hello.txt");

    let content = archive
        .read_entry(archive.unique_entry("hello.txt").unwrap())
        .expect("read member");
    assert_eq!(content, generate::payload("hello.txt"));
}

#[test]
fn rar4_store_header_fields() {
    let Some(made) = corpus::store() else { return };
    let archive = ArchiveReader::open(made.path()).expect("open");
    let entry = archive.entries().next().unwrap();
    assert_eq!(entry.method(), 0, "STORE normalizes to method 0");
    assert_eq!(entry.method_name(), "Store");
    assert_eq!(
        entry.compressed_size(),
        entry.size(),
        "stored: packed == unpacked"
    );
    assert_eq!(entry.size(), generate::payload("hello.txt").len() as u64);
    // A stored member declares the minimum version the store method needs,
    // not the container's: `-ma4 -m0` still writes `unp_ver` 20.
    assert_eq!(entry.version(), ArchiveVersion::V20);
}

// ── Compressed members (-ma4 m3/m5 LZSS+Huffman) ──────────────────────────

#[test]
fn rar4_m3_compressed_read() {
    let Some(made) = corpus::compressed_m3() else {
        return;
    };
    let mut archive = ArchiveReader::open(made.path()).expect("open m3");
    let (_, size, _) = snapshots(&archive)
        .into_iter()
        .find(|(name, _, _)| name == "repeat.txt")
        .unwrap();
    assert_eq!(size, generate::payload("repeat.txt").len() as u64);

    let method = archive
        .entries()
        .find(|e| e.name() == "repeat.txt")
        .unwrap()
        .method();
    assert_eq!(method, 3, "m3 method normalized");

    let content = archive
        .read_entry(archive.unique_entry("repeat.txt").unwrap())
        .expect("decode m3 member");
    assert_eq!(content, generate::payload("repeat.txt"));
    assert!(content.len() > 200_000, "payload is the 4000-line repeat");
}

#[test]
fn rar4_m5_compressed_read() {
    // m5 on highly repetitive text still picks LZSS (not PPMd).
    let Some(made) = corpus::compressed_m5() else {
        return;
    };
    let mut archive = ArchiveReader::open(made.path()).expect("open m5");
    assert_eq!(
        archive
            .read_entry(archive.unique_entry("repeat.txt").unwrap())
            .expect("decode m5 member"),
        generate::payload("repeat.txt")
    );
}

#[test]
fn rar4_incompressible_members_store() {
    let Some(made) = corpus::compressed_m3() else {
        return;
    };
    let archive = ArchiveReader::open(made.path()).expect("open");
    let random = archive
        .entries()
        .find(|e| e.name() == "random.bin")
        .unwrap();
    assert!(
        random.compressed_size() >= random.size(),
        "an incompressible member cannot pack smaller"
    );
    assert_all_members_match_payload(&made);
}

// ── Solid chains ──────────────────────────────────────────────────────────

#[test]
fn rar4_solid_chain_decode() {
    let Some(made) = corpus::solid_m3() else {
        return;
    };
    let mut archive = ArchiveReader::open(made.path()).expect("open solid");
    let snaps = snapshots(&archive);
    assert_eq!(snaps.len(), 4);
    // Solid chains group members by extension (measured: `.bin` before `.txt`,
    // alphabetical inside a group), so a bin member sorts ahead of a.txt.
    let names: Vec<&str> = snaps.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, ["random.bin", "a.txt", "b.txt", "repeat.txt"]);

    // Head member, a middle member (decodes through the shared window) and the
    // tail member all have to reproduce their payload.
    for name in ["a.txt", "b.txt", "repeat.txt", "random.bin"] {
        let data = archive
            .read_entry(archive.unique_entry(name).unwrap())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(data, generate::payload(name), "{name}");
    }
    assert!(snaps.iter().all(|(_, _, crc)| crc.is_some()));
}

// ── Member encryption (`-p`, RAR30 AES) ───────────────────────────────────

#[test]
fn rar4_encrypted_members_need_password() {
    let Some(made) = corpus::member_password() else {
        return;
    };
    let mut archive = ArchiveReader::open(made.path()).expect("open");
    match archive.read_entry(archive.unique_entry("repeat.txt").unwrap()) {
        Err(RarError::Encrypted(msg)) => assert!(msg.contains("no password")),
        other => panic!("expected Encrypted without password, got {other:?}"),
    }
}

#[test]
fn rar4_encrypted_members_decode_with_password() {
    let Some(made) = corpus::member_password() else {
        return;
    };
    let mut archive = ArchiveReader::open_with(made.path(), OpenOptions::new().password("pass123"))
        .expect("open");
    assert_eq!(
        archive
            .read_entry(archive.unique_entry("repeat.txt").unwrap())
            .expect("decrypt + decode"),
        generate::payload("repeat.txt")
    );
    assert_eq!(
        archive
            .read_entry(archive.unique_entry("random.bin").unwrap())
            .expect("decrypt + decode random"),
        generate::payload("random.bin")
    );
}

#[test]
fn rar4_wrong_password_fails() {
    let Some(made) = corpus::member_password() else {
        return;
    };
    let mut archive =
        ArchiveReader::open_with(made.path(), OpenOptions::new().password("not-the-password"))
            .expect("open");
    assert!(
        archive
            .read_entry(archive.unique_entry("repeat.txt").unwrap())
            .is_err()
    );
}

// ── Standard VM filters ───────────────────────────────────────────────────

#[test]
fn rar4_standard_vm_filters_decode() {
    // WinRAR picks the filter from the member's shape: audio for the WAV, RGB
    // for the 24-bit BMP, x86 for the `E8 rel32` stream.
    let Some(made) = corpus::vm_filters() else {
        return;
    };
    assert_all_members_match_payload(&made);

    // A filtered member must pack materially smaller than its payload.
    let archive = ArchiveReader::open(made.path()).expect("open");
    for entry in archive.entries() {
        assert!(
            entry.compressed_size() < entry.size(),
            "{}: filter should not expand",
            entry.name()
        );
    }
}

// ── Header encryption (`-hp`) ─────────────────────────────────────────────

#[test]
fn rar4_header_encrypted_archives_decode() {
    let Some(made) = corpus::header_password() else {
        return;
    };
    assert!(matches!(
        ArchiveReader::open(made.path()),
        Err(RarError::Encrypted(_))
    ));

    let mut archive =
        ArchiveReader::open_with(made.path(), OpenOptions::new().password("password"))
            .expect("open with password");
    let snaps = snapshots(&archive);
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].0, "hello.txt");
    let data = archive
        .read_entry(archive.unique_entry("hello.txt").unwrap())
        .expect("decode");
    assert_eq!(data, generate::payload("hello.txt"));

    // Wrong password must not silently list garbage.
    assert!(ArchiveReader::open_with(made.path(), OpenOptions::new().password("nope")).is_err());
}

#[test]
fn rar4_header_encrypted_unicode_names() {
    let Some(made) = corpus::header_password_unicode() else {
        return;
    };
    let archive = ArchiveReader::open_with(made.path(), OpenOptions::new().password("1234"))
        .expect("open with password");
    let names: Vec<String> = archive.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().any(|n| n.contains("中文")), "{names:?}");
}

#[test]
fn rar4_header_encrypted_multivol_decode() {
    let Some(made) = corpus::header_password_multivol() else {
        return;
    };
    let mut archive =
        ArchiveReader::open_with(made.path(), OpenOptions::new().password("password"))
            .expect("open with password");
    assert!(made.files.len() > 1, "expected several volumes");
    let snaps = snapshots(&archive);
    assert!(!snaps.is_empty());
    for (name, size, crc) in snaps {
        let data = archive
            .read_entry(archive.unique_entry(&name).unwrap())
            .expect("decode split -hp member");
        assert_eq!(data.len() as u64, size, "{name} size");
        assert_eq!(Some(crc32(&data)), crc, "{name} CRC");
        assert_eq!(data, generate::payload(&name), "{name} content");
    }
}

// ── Long passwords (slow KDF branches) ────────────────────────────────────

#[test]
fn rar4_long_password_member_kdf_decodes() {
    // Member encryption takes the salted slow KDF once the UTF-16 password
    // plus its 8-byte salt crosses a SHA-1 block (>= 28 chars).
    let Some(made) = corpus::long_password_member() else {
        return;
    };
    let mut archive = ArchiveReader::open(made.path()).expect("open");
    assert!(matches!(
        archive.read_entry(archive.unique_entry("longpw.txt").unwrap()),
        Err(RarError::Encrypted(_))
    ));

    let mut archive = ArchiveReader::open_with(
        made.path(),
        OpenOptions::new().password(corpus::LONG_PASSWORD),
    )
    .expect("open with password");
    assert_eq!(
        archive
            .read_entry(archive.unique_entry("longpw.txt").unwrap())
            .expect("decrypt with the long password"),
        generate::payload("longpw.txt")
    );

    // One character different must fail the integrity check.
    let mut wrong_pw = corpus::LONG_PASSWORD.to_string();
    wrong_pw.pop();
    wrong_pw.push('9');
    let mut wrong =
        ArchiveReader::open_with(made.path(), OpenOptions::new().password(wrong_pw)).expect("open");
    assert!(
        wrong
            .read_entry(wrong.unique_entry("longpw.txt").unwrap())
            .is_err()
    );
}

#[test]
fn rar4_long_password_header_kdf_decodes() {
    // `-hp` uses the same KDF without a salt, so the slow branch starts at 32
    // UTF-16 characters.
    let Some(made) = corpus::long_password_header() else {
        return;
    };
    assert!(matches!(
        ArchiveReader::open(made.path()),
        Err(RarError::Encrypted(_))
    ));
    let mut archive = ArchiveReader::open_with(
        made.path(),
        OpenOptions::new().password(corpus::LONG_PASSWORD),
    )
    .expect("open with password");
    assert_eq!(
        archive
            .read_entry(archive.unique_entry("longpw.txt").unwrap())
            .expect("decrypt header and member"),
        generate::payload("longpw.txt")
    );
}

// ── Multi-volume RAR4 ─────────────────────────────────────────────────────

#[test]
fn rar4_multivolume_sets_decode() {
    // Every member is decoded through its volume segments, with the header CRC
    // as the byte-exactness gate.
    for (label, made) in [
        ("new naming", corpus::multivol_new_names()),
        ("stored", corpus::multivol_stored()),
    ] {
        let Some(made) = made else { continue };
        assert!(made.files.len() > 1, "{label}: expected several volumes");
        assert_all_members_match_payload(&made);
    }

    // Opening a later volume directly must work too.
    let Some(made) = corpus::multivol_new_names() else {
        return;
    };
    let later = made
        .files
        .iter()
        .find(|p| {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            name.ends_with("part02.rar")
        })
        .expect("second volume");
    let archive = ArchiveReader::open(later).expect("any-volume open");
    assert!(!archive.entries().next().is_none());
}

#[test]
fn rar4_multivolume_old_numbering() {
    let Some(made) = corpus::multivol_old_names() else {
        return;
    };
    let names = made.names();
    assert!(
        names.iter().any(|n| n.to_lowercase().ends_with(".r00")),
        "expected legacy .r00 numbering: {names:?}"
    );
    let volumes = rar_rs::discover_volumes(made.path());
    assert!(volumes.len() >= 2, "{volumes:?}");
    let mut archive = ArchiveReader::open(&volumes[0]).expect("open");
    let data = archive
        .read_entry(archive.unique_entry("random.bin").unwrap())
        .expect("decode split member");
    assert_eq!(data, generate::payload("random.bin"));
}

#[test]
fn rar4_multivolume_encrypted_decode() {
    let Some(made) = corpus::multivol_encrypted() else {
        return;
    };
    let mut archive =
        ArchiveReader::open_with(made.path(), OpenOptions::new().password("password"))
            .expect("open with password");
    let snaps = snapshots(&archive);
    assert!(!snaps.is_empty());
    for (name, size, crc) in snaps {
        let data = archive
            .read_entry(archive.unique_entry(&name).unwrap())
            .expect("decode split member");
        assert_eq!(data.len() as u64, size, "{name} size");
        assert_eq!(Some(crc32(&data)), crc, "{name} CRC");
        assert_eq!(data, generate::payload(&name), "{name} content");
    }
}

// ── PPMd members ──────────────────────────────────────────────────────────

#[test]
fn rar4_ppmd_members_decode() {
    let Some(made) = corpus::ppmd_text() else {
        return;
    };
    assert_all_members_match_payload(&made);
    assert_eq!(member_versions(made.path()), [ArchiveVersion::V29]);
}

#[test]
fn rar4_ppmd_mixed_and_phrase() {
    for (label, made) in [
        ("mixed", corpus::ppmd_mixed()),
        ("phrase", corpus::ppmd_phrase()),
    ] {
        let Some(made) = made else { continue };
        assert_all_members_match_payload(&made);
        assert!(!made.names().is_empty(), "{label}");
    }
}

#[test]
fn rar4_ppmd_solid_chain_decodes() {
    // Two members in one solid PPMd chain: the second references the first
    // through the shared window.
    let Some(made) = corpus::ppmd_solid() else {
        return;
    };
    let archive = ArchiveReader::open(made.path()).expect("open");
    assert_eq!(archive.entries().count(), 2);
    assert_all_members_match_payload(&made);
}

// ── RAR 1.5 (unp_ver 15) ─────────────────────────────────────────────────

#[test]
fn rar4_rar154_members_decode() {
    // Our writer emits RAR 1.5; the official UnRAR still has to accept it.
    for (label, made) in [
        ("single", corpus::rar154_normal()),
        ("docset", corpus::rar154_docset()),
    ] {
        let Some(made) = made else { continue };
        assert_all_members_match_payload(&made);
        assert_eq!(
            member_versions(made.path())[0],
            ArchiveVersion::V15,
            "{label} version"
        );
        if let Some(verdict) = corpus::official_accepts("7-23", &made) {
            verdict.unwrap_or_else(|e| panic!("{label}: {e}"));
        }
    }
}

#[test]
fn rar4_rar154_encrypted_members_decode_with_password() {
    let Some(plain) = corpus::rar154_normal() else {
        return;
    };
    let Some(encrypted) = corpus::rar154_password() else {
        return;
    };
    let mut archive = ArchiveReader::open(encrypted.path()).expect("open");
    match archive.read_entry(archive.unique_entry("README.md").unwrap()) {
        Err(RarError::Encrypted(_)) => {}
        other => panic!("expected Encrypted without password, got {other:?}"),
    }
    let mut archive =
        ArchiveReader::open_with(encrypted.path(), OpenOptions::new().password("password"))
            .expect("open with password");
    let data = archive
        .read_entry(archive.unique_entry("README.md").unwrap())
        .expect("decrypt RAR1.5 member");
    assert_eq!(data, generate::payload("README.md"));

    // The decrypted bytes are the same member as the unencrypted archive.
    let mut plain = ArchiveReader::open(plain.path()).expect("open plain");
    assert_eq!(
        plain
            .read_entry(plain.unique_entry("README.md").unwrap())
            .expect("plain member"),
        data
    );
}

#[test]
fn rar4_rar154_wrong_password_fails() {
    let Some(made) = corpus::rar154_password() else {
        return;
    };
    let mut archive =
        ArchiveReader::open_with(made.path(), OpenOptions::new().password("wrong-password"))
            .expect("open");
    assert!(
        archive
            .read_entry(archive.unique_entry("README.md").unwrap())
            .is_err()
    );
}

#[test]
fn rar4_rar154_old_numbered_split_set() {
    let Some(made) = corpus::rar154_split_old() else {
        return;
    };
    let volumes = rar_rs::discover_volumes(made.path());
    assert!(volumes.len() >= 2, "expected several volumes: {volumes:?}");
    let mut archive = ArchiveReader::open(&volumes[0]).expect("open");
    let names: Vec<String> = archive.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(names, vec!["random.bin".to_string()]);
    let data = archive
        .read_entry(archive.unique_entry("random.bin").unwrap())
        .expect("decode split member");
    assert_eq!(data, generate::payload("random.bin"));
}

// ── RAR 2.x (unp_ver 20) ─────────────────────────────────────────────────

#[test]
fn rar4_rar2x_fixtures_decode() {
    // RAR 2.0-era codec: audio-table blocks (WAV) and plain LZ text.
    let Some(made) = corpus::rar20_mixed() else {
        return;
    };
    assert_all_members_match_payload(&made);
    assert!(
        member_versions(made.path())
            .iter()
            .all(|v| *v == ArchiveVersion::V20)
    );
    // The v20 stream comes from the 2.x line; official 2.90 must agree (or the
    // tool is absent and that half of the check is skipped).
    if let Some(verdict) = corpus::official_accepts("2-90", &made) {
        verdict.expect("2.90 accepts its own output");
    }
}

#[test]
fn rar4_rar202_encrypted_members_decode_with_password() {
    let Some(made) = corpus::rar20_password() else {
        return;
    };
    let mut archive = ArchiveReader::open(made.path()).expect("open");
    match archive.read_entry(archive.unique_entry("FILE1.TXT").unwrap()) {
        Err(RarError::Encrypted(_)) => {}
        other => panic!("expected Encrypted without password, got {other:?}"),
    }
    let mut archive =
        ArchiveReader::open_with(made.path(), OpenOptions::new().password("password"))
            .expect("open with password");
    assert_eq!(
        archive
            .read_entry(archive.unique_entry("FILE1.TXT").unwrap())
            .expect("decrypt FILE1"),
        generate::payload("FILE1.TXT")
    );
    assert_eq!(
        archive
            .read_entry(archive.unique_entry("FILE2.TXT").unwrap())
            .expect("decrypt FILE2"),
        generate::payload("FILE2.TXT")
    );
}

#[test]
fn rar4_rar202_wrong_password_fails() {
    let Some(made) = corpus::rar20_password() else {
        return;
    };
    let mut archive =
        ArchiveReader::open_with(made.path(), OpenOptions::new().password("wrong-password"))
            .expect("open");
    assert!(
        archive
            .read_entry(archive.unique_entry("FILE1.TXT").unwrap())
            .is_err()
    );
}

/// RAR 2.0 solid pair: MHD_SOLID on the main header, FHD_SOLID on the second
/// member, and a 64 KiB window that has to carry across.
#[test]
fn rar4_solid20_chain_shared_window() {
    let Some(made) = corpus::rar20_solid() else {
        return;
    };
    let mut archive = ArchiveReader::open(made.path()).expect("open");
    let names: Vec<String> = archive.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(
        names,
        vec!["SOLID1.TXT".to_string(), "SOLID2.TXT".to_string()]
    );
    let b = archive
        .read_entry(archive.unique_entry("SOLID2.TXT").unwrap())
        .expect("second");
    assert_eq!(b, generate::payload("SOLID2.TXT"));
    let a = archive
        .read_entry(archive.unique_entry("SOLID1.TXT").unwrap())
        .expect("first");
    assert_eq!(a, generate::payload("SOLID1.TXT"));
}

// ── Host OS axis ─────────────────────────────────────────────────────────

#[test]
fn rar4_host_os_names_report_the_writer() {
    // RAR stamps the host it ran on — 2 on Windows, 3 on Unix — and the corpus
    // is built by the release for this host, so the expected value depends on
    // where the test runs. Our own writer always stamps Windows, which is what
    // WinRAR did for every DOS/Windows archive it produced.
    let (raw, name) = if cfg!(windows) {
        (2, "Windows")
    } else {
        (3, "Unix")
    };
    let Some(made) = corpus::compressed_m3() else {
        return;
    };
    let archive = ArchiveReader::open(made.path()).unwrap();
    let entry = archive.entries().next().unwrap();
    assert_eq!(entry.host_os_raw(), raw);
    assert_eq!(entry.host_os_name(), name);

    let Some(ours) = corpus::rar154_normal() else {
        return;
    };
    let archive = ArchiveReader::open(ours.path()).unwrap();
    let entry = archive.entries().next().unwrap();
    assert_eq!(entry.host_os_raw(), 2);
    assert_eq!(entry.host_os_name(), "Windows");
}
