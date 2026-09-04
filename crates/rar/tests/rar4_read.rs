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
const RAR2: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rar40/rar2/");
const RAR154: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rar40/rar154/");
const MVOL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/rar40/multivol/"
);
const ENC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/rar40/encrypted/"
);
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
    let mut archive = RarArchive::open(format!("{W591}c_m3.rar")).expect("open c_m3");
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
    let mut archive = RarArchive::open(format!("{W591}c_m5.rar")).expect("open c_m5");
    assert_eq!(
        archive.read("repeat.txt").expect("decode m5 member"),
        repeat_content()
    );
}

#[test]
fn rar4_winrar591_random_and_tiny_members() {
    // Incompressible and tiny members store even under -m3/-m5.
    for file in ["c_m3.rar", "c_m5.rar"] {
        let mut archive = RarArchive::open(format!("{W591}{file}")).expect("open");
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
    let mut archive = RarArchive::open(format!("{W591}c_solid.rar")).expect("open c_solid");
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
        RarArchive::open(format!("{RAR300}compressed_text_rar300.rar")).expect("open");
    let (name, size, crc) = snapshots(&archive).into_iter().next().unwrap();
    assert_eq!(name, "text.txt");
    assert_eq!(size, 2_400);
    let data = archive.read("text.txt").expect("decode RAR3.0 m3 text");
    assert_eq!(rar5_crc32(&data), crc.unwrap());
}

#[test]
fn rar4_rar300_solid_archives() {
    for file in ["solid_simple_rar300.rar", "solid_rar300.rar"] {
        let mut archive = RarArchive::open(format!("{RAR300}{file}")).expect("open");
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
    let mut archive = RarArchive::open(format!("{W591}c_pw.rar")).expect("open");
    match archive.read("repeat.txt") {
        Err(RarError::Encrypted(msg)) => assert!(msg.contains("no password")),
        other => panic!("expected Encrypted without password, got {other:?}"),
    }
}

#[test]
fn rar4_encrypted_compressed_members_decode_with_password() {
    let mut archive = RarArchive::open(format!("{W591}c_pw.rar")).expect("open");
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
    let mut archive = RarArchive::open(format!("{W591}c_pw.rar")).expect("open");
    archive.set_password("not-the-password");
    assert!(archive.read("repeat.txt").is_err());
}

// ── Standard VM filters (RAR 3.0 fixtures from the rars corpus) ───────────

#[test]
fn rar4_standard_vm_filters_decode() {
    // One genuine RAR 3.0 fixture per standard filter type: E8, E8E9, delta
    // (4ch), audio (stereo), RGB (24-bit BMP), Itanium.
    for file in [
        "rarvm_x86_e8_rar300.rar",
        "rarvm_x86_e8e9_rar300.rar",
        "rarvm_delta_4ch_rar300.rar",
        "rarvm_audio_stereo_rar300.rar",
        "rarvm_rgb_gradient_rar300.rar",
        "rarvm_itanium_synthetic_rar300.rar",
    ] {
        let mut archive = RarArchive::open(format!("{RAR300}{file}")).expect("open");
        for (name, size, crc) in snapshots(&archive) {
            let data = archive.read(&name).expect("decode filtered member");
            assert_eq!(data.len() as u64, size, "{file}/{name} size");
            assert_eq!(rar5_crc32(&data), crc.unwrap(), "{file}/{name} CRC");
        }
    }
}

#[test]
fn rar4_vm_filter_edge_cases() {
    // Solid chain whose later member carries an E8E9 filter (filter block
    // position is member-relative), a PPMd member with an embedded filter
    // record (escape code 3), a 64-channel delta, and two archives that
    // exercise non-trivial record encodings on a real executable.
    for file in [
        "solid_e8_filter_member_offset.rar",
        "ppmd_embedded_vm_filter.rar",
        "delta_64_channels.rar",
        "filter_bsdcat_exe.rar",
        "vm_encoded_u32_filter.rar",
    ] {
        let path = format!("{FIX}rarvm/{file}");
        let mut archive = RarArchive::open(&path).expect("open");
        let snaps = snapshots(&archive);
        assert!(!snaps.is_empty());
        for (name, size, crc) in snaps {
            let data = archive.read(&name).expect("decode filter member");
            assert_eq!(data.len() as u64, size, "{file}/{name} size");
            assert_eq!(rar5_crc32(&data), crc.unwrap(), "{file}/{name} CRC");
        }
    }
}

// ── Header-encrypted RAR3/4 (`-hp`, from the rars fixture corpus) ────────

#[test]
fn rar4_header_encrypted_archives_decode() {
    // -hp hides file names behind AES-128 header encryption; listing needs
    // the password at open time.
    assert!(matches!(
        RarArchive::open(format!("{ENC}header_rar300_password.rar")),
        Err(RarError::Encrypted(_))
    ));

    let mut archive =
        RarArchive::open_with_password(format!("{ENC}header_rar300_password.rar"), "password")
            .expect("open with password");
    let snaps = snapshots(&archive);
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].0, "hello.txt");
    let data = archive.read("hello.txt").expect("decode");
    assert_eq!(rar5_crc32(&data), snaps[0].2.unwrap());

    // Unicode name inside an encrypted header.
    let archive = RarArchive::open_with_password(format!("{ENC}header_enc_1234.rar"), "1234")
        .expect("open with password");
    let names: Vec<String> = snapshots(&archive).into_iter().map(|(n, _, _)| n).collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().any(|n| n.contains("中文")), "{names:?}");

    // Wrong password must not silently list garbage.
    assert!(
        RarArchive::open_with_password(format!("{ENC}header_rar300_password.rar"), "nope").is_err()
    );
}

#[test]
fn rar4_header_encrypted_multivol_decode() {
    let mut archive = RarArchive::open_with_password(
        format!("{ENC}header_encrypted_multivol_rar300.rar"),
        "password",
    )
    .expect("open with password");
    let (name, size, crc) = snapshots(&archive).into_iter().next().unwrap();
    assert_eq!(name, "bigtext_64k.bin");
    let data = archive.read(&name).expect("decode split -hp member");
    assert_eq!(data.len() as u64, size);
    assert_eq!(rar5_crc32(&data), crc.unwrap());
}

// ── Multi-volume RAR4 (from the rars fixture corpus) ──────────────────────

#[test]
fn rar4_multivolume_sets_decode() {
    // Genuine RAR 3.0 split archives across 2–5 volumes: new `.partN.rar`
    // naming, legacy `.r00` naming, a stored file, a compressed PRNG file,
    // and an encrypted file. Every member is decoded through its volume
    // segments with the header CRC as the byte-exactness gate.
    let cases = [
        ("multivol_newnaming_rar300.part01.rar", None),
        ("multivol_newnaming_rar300.part02.rar", None), // any-volume open
        ("multivol_oldnaming_rar300.rar", None),
        ("multivol_oldnaming_rar300.r00", None),
        ("stored_multivol_rar300.rar", None),
        ("compressed_multivol_prng_rar300.rar", None),
        ("encrypted_multivol_rar300.rar", Some("password")),
    ];
    for (file, password) in cases {
        let mut archive = RarArchive::open(format!("{MVOL}{file}")).expect("open");
        if let Some(p) = password {
            archive.set_password(p);
        }
        let snaps = snapshots(&archive);
        assert!(!snaps.is_empty(), "{file}");
        for (name, size, crc) in snaps {
            let data = archive.read(&name).expect("decode split member");
            assert_eq!(data.len() as u64, size, "{file}/{name} size");
            assert_eq!(rar5_crc32(&data), crc.unwrap(), "{file}/{name} CRC");
        }
    }
}

// ── RAR 1.5 (unp_ver 15, from the rars fixture corpus) ───────────────────

#[test]
#[test]
/// Below `UnpVer` 20 the per-file FHD_SOLID bit is never written (and is
/// ignored when present): a solid run is anchored by the archive-level
/// MHD_SOLID flag plus member position. This crafted fixture is a solid
/// RAR 1.5 pair whose second member had FHD_SOLID cleared and its header
/// CRC recomputed; the member is 46 packed bytes standing for 2700
/// unpacked, so it can only decode by carrying the shared window across.
#[test]
fn rar4_solid15_chain_ignores_cleared_fhd_solid() {
    let path = format!("{RAR154}solid_flag_cleared_rar15.rar");
    let mut archive = RarArchive::open(&path).expect("open");
    let names: Vec<String> = archive.namelist().into_iter().map(str::to_string).collect();
    assert_eq!(names.len(), 2);
    let a = archive.read(&names[0]).expect("first solid member");
    let b = archive.read(&names[1]).expect("second solid member");
    assert_eq!(a.len(), 2700);
    assert_eq!(b.len(), 2700);
    assert_eq!(a, b, "both solid members share the same window content");
}

fn rar4_rar154_fixtures_decode() {
    // RAR 1.5.4-era archives: normal compression (incl. a 17-file doc set),
    // DOS vs long file names, and a solid-flagged archive whose single
    // member is stored.
    for file in [
        "readme_154_normal.rar",
        "readme_154_store_solid.rar",
        "doc_154_best.rar",
        "audio_dos_names_unpack15.rar",
        "audio_win_names_unpack15.rar",
    ] {
        let mut archive = RarArchive::open(format!("{RAR154}{file}")).expect("open");
        let snaps = snapshots(&archive);
        assert!(!snaps.is_empty(), "{file}");
        for (name, size, crc) in snaps {
            let data = archive.read(&name).expect("decode RAR1.5 member");
            assert_eq!(data.len() as u64, size, "{file}/{name} size");
            assert_eq!(rar5_crc32(&data), crc.unwrap(), "{file}/{name} CRC");
        }
    }
}

#[test]
fn rar4_rar154_encrypted_members_decode_with_password() {
    // RAR 1.5 member data encrypted with the legacy RAR15 stream cipher;
    // the decrypted README matches the unencrypted fixture byte for byte.
    let mut archive = RarArchive::open(format!("{RAR154}readme_154_password.rar")).expect("open");
    match archive.read("README.md") {
        Err(RarError::Encrypted(_)) => {}
        other => panic!("expected Encrypted without password, got {other:?}"),
    }
    archive.set_password("password");
    let data = archive.read("README.md").expect("decrypt RAR1.5 member");
    assert_eq!(data.len(), 4_198);
    assert_eq!(rar5_crc32(&data), 0x509e_5e3c);

    let mut plain = RarArchive::open(format!("{RAR154}readme_154_normal.rar")).expect("open");
    assert_eq!(plain.read("README.md").expect("plain member"), data);
}

#[test]
fn rar4_rar154_wrong_password_fails() {
    let mut archive = RarArchive::open(format!("{RAR154}readme_154_password.rar")).expect("open");
    archive.set_password("wrong-password");
    assert!(archive.read("README.md").is_err());
}

// ── RAR 2.x (unp_ver 20/26, from the rars fixture corpus) ─────────────────

#[test]
fn rar4_rar2x_fixtures_decode() {
    // RAR 2.0-era archives: plain LZ, audio-table blocks (a WAV), multiple
    // blocks per member, kept tables, and an out-of-window zero-fill member.
    for file in [
        "comment_nopsw.rar",
        "rar20.rar",
        "unpack20_audio_text.rar",
        "unpack20_keep_tables.rar",
        "unpack20_multiblock.rar",
    ] {
        let mut archive = RarArchive::open(format!("{RAR2}{file}")).expect("open");
        let snaps = snapshots(&archive);
        assert!(!snaps.is_empty(), "{file}");
        for (name, size, crc) in snaps {
            let data = archive.read(&name).expect("decode RAR2 member");
            assert_eq!(data.len() as u64, size, "{file}/{name} size");
            assert_eq!(rar5_crc32(&data), crc.unwrap(), "{file}/{name} CRC");
        }
    }
}

#[test]
fn rar4_rar202_encrypted_members_decode_with_password() {
    // RAR 2.0 member data encrypted with the legacy RAR20 block cipher;
    // contents pinned from the rars fixture corpus.
    let mut archive = RarArchive::open(format!("{RAR2}comment_psw.rar")).expect("open");
    match archive.read("FILE1.TXT") {
        Err(RarError::Encrypted(_)) => {}
        other => panic!("expected Encrypted without password, got {other:?}"),
    }
    archive.set_password("password");
    assert_eq!(
        archive.read("FILE1.TXT").expect("decrypt FILE1"),
        b"file1\r\n"
    );
    assert_eq!(
        archive.read("FILE2.TXT").expect("decrypt FILE2"),
        b"file2\r\n"
    );
}

#[test]
fn rar4_rar202_wrong_password_fails() {
    let mut archive = RarArchive::open(format!("{RAR2}comment_psw.rar")).expect("open");
    archive.set_password("wrong-password");
    assert!(archive.read("FILE1.TXT").is_err());
}

// ── PPMd (RAR 3.0 m5 members from the rars fixture corpus) ────────────────

#[test]
fn rar4_ppmd_members_decode() {
    // A 127 KiB lorem member compressed with PPMd by genuine RAR 3.0.
    let mut archive = RarArchive::open(format!("{RAR300}ppmd_lorem_rar300.rar")).expect("open");
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
        let mut archive = RarArchive::open(format!("{RAR300}{file}")).expect("open");
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
    let mut archive = RarArchive::open(format!("{RAR300}ppmd_solid_rar300.rar")).expect("open");
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

/// RAR 2.0 solid pair (WinRAR 2.5-era): MHD_SOLID main header, second
/// member flagged FHD_SOLID. The shared 64 KiB window must carry across
/// (payload CRCs 0x97668cf2 / 0x28833332 from the reference).
#[test]
fn rar4_solid20_chain_shared_window() {
    let path = format!("{RAR2}SOLID.RAR");
    let mut archive = RarArchive::open(&path).expect("open");
    let names: Vec<String> = archive.namelist().into_iter().map(str::to_string).collect();
    assert_eq!(
        names,
        vec!["SOLID1.TXT".to_string(), "SOLID2.TXT".to_string()]
    );
    let a = archive.read("SOLID1.TXT").expect("first");
    let b = archive.read("SOLID2.TXT").expect("second");
    assert_eq!(rar5_crc32(&a), 0x9766_8cf2, "SOLID1 payload CRC");
    assert_eq!(rar5_crc32(&b), 0x2883_3332, "SOLID2 payload CRC");
}
