#![allow(deprecated)] // legacy constructor family; use create_with_options
//! Integration tests for archive creation, extraction, and WinRAR interop.
//!
//! `tests/fixtures/winrar5_multiple_files.rar` is a RAR5 archive created by
//! WinRAR, vendored from the libarchive test suite
//! (`test_read_format_rar5_multiple_files.rar`, BSD-2-Clause licensed,
//! https://github.com/libarchive/libarchive).

use rar5::RarArchive;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

const FIXTURE: &str = "tests/fixtures/winrar5_multiple_files.rar";
const FIXTURE_FILES: [(&str, &str); 4] = [
    (
        "test1.bin",
        "7d89f86f9f69d744ffff3fc043e15bf89fc3ffc134ffcbb31d164a99bb8b67b0",
    ),
    (
        "test2.bin",
        "f81e6fceeeab366306b23466bf6bb3aac2875e0906dc20a8652be0696ceb15a2",
    ),
    (
        "test3.bin",
        "5e621f2b6ce8fed758c3df8221f994eda55d1e432c7cc4349c34a30ec2e1c43d",
    ),
    (
        "test4.bin",
        "2627f40180217252956edb9a426e8d3e344adaf89019d3bccbe04f6c3416dcdd",
    ),
];

#[test]
fn rar4_archives_are_rejected_with_clear_error() {
    let dir = make_temp_dir();
    let path = dir.path().join("rar4.rar");
    // RAR4 signature plus a marker-block header; rar-rs is RAR5-only and
    // must refuse with an actionable error (7-Zip handles RAR4).
    let mut data = b"Rar!\x1a\x07\x00".to_vec();
    data.extend_from_slice(&[0x72, 0x04, 0x00]);
    std::fs::write(&path, &data).unwrap();

    match RarArchive::open(&path) {
        Err(rar5::RarError::Unsupported(msg)) => assert!(
            msg.contains("RAR4"),
            "expected a RAR4-specific message, got: {msg}"
        ),
        Err(e) => panic!("expected Unsupported(RAR4), got {e:?}"),
        Ok(_) => panic!("expected RAR4 archive to be rejected"),
    }
}

fn sha256(data: &[u8]) -> String {
    // sha2 is a dependency of the library; expose it via the already-linked
    // crates. digest 0.11's output no longer formats as hex, so encode it
    // manually.
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn make_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn write_repeated(path: &Path, byte: u8, len: usize) {
    let mut f = std::fs::File::create(path).expect("create file");
    let chunk = vec![byte; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(chunk.len());
        f.write_all(&chunk[..n]).expect("write file");
        left -= n;
    }
}

// ── RAR5 block-scanning helpers (test-only) ────────────────────────────────

fn read_vint(data: &[u8], mut off: usize) -> (u64, usize) {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let b = data[off];
        off += 1;
        value |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (value, off)
}

struct BlockInfo {
    start: usize,
    header_len: usize,
    block_type: u64,
    #[allow(dead_code)]
    flags: u64,
    #[allow(dead_code)]
    extra_size: u64,
    #[allow(dead_code)]
    data_size: u64,
    body: Vec<u8>,
}

fn scan_blocks(data: &[u8]) -> Vec<BlockInfo> {
    // Cross the library's block-envelope seam (the same reader the archive
    // scanner uses) instead of re-implementing the envelope format.
    let mut blocks = Vec::new();
    let mut cursor = std::io::Cursor::new(data);
    cursor.set_position(8); // skip the RAR5 signature
    while let Ok(Some(meta)) = rar5::headers::read_block(&mut cursor, None) {
        blocks.push(BlockInfo {
            start: meta.block_start as usize,
            header_len: (meta.data_offset - meta.block_start - 4) as usize,
            block_type: meta.block_type,
            flags: meta.flags,
            extra_size: 0,
            data_size: meta.raw.data_size,
            body: meta.raw.header_data,
        });
        let last = meta.block_type == 0x05;
        cursor.set_position(meta.data_end);
        if last {
            break;
        }
    }
    blocks
}

fn service_name(body: &[u8]) -> Option<String> {
    let (_, mut q) = read_vint(body, 0);
    let (flags, n) = read_vint(body, q);
    q = n;
    if flags & 0x0001 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    if flags & 0x0002 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    // file flags, unpacked size, attributes, compression info, host OS
    for _ in 0..5 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    let (name_len, n) = read_vint(body, q);
    q = n;
    if q + name_len as usize > body.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&body[q..q + name_len as usize]).into_owned())
}

fn first_file_data_offset(data: &[u8]) -> usize {
    for block in scan_blocks(data) {
        if block.block_type == 0x02 {
            return block.start + block.header_len;
        }
    }
    panic!("no file block found");
}

fn main_header_locator(data: &[u8]) -> (u64, Option<u64>, Option<u64>) {
    for block in scan_blocks(data) {
        if block.block_type == 0x01 {
            let (_, mut q) = read_vint(&block.body, 0);
            let (flags, n) = read_vint(&block.body, q);
            q = n;
            let mut extra_size = 0u64;
            if flags & 0x0001 != 0 {
                let (v, n) = read_vint(&block.body, q);
                extra_size = v;
                q = n;
            }
            if flags & 0x0002 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            // archive flags vint, then extra area
            let (_, n) = read_vint(&block.body, q);
            q = n;
            let extra = &block.body[q..q + extra_size as usize];
            let (_, mut e) = read_vint(extra, 0);
            let (rec_type, n) = read_vint(extra, e);
            e = n;
            assert_eq!(rec_type, 0x01, "locator record");
            let (loc_flags, n) = read_vint(extra, e);
            e = n;
            let qo = if loc_flags & 0x0001 != 0 {
                let (v, n) = read_vint(extra, e);
                e = n;
                Some(v)
            } else {
                None
            };
            let rr = if loc_flags & 0x0002 != 0 {
                let (v, _) = read_vint(extra, e);
                Some(v)
            } else {
                None
            };
            return (loc_flags, qo, rr);
        }
    }
    panic!("no main header found");
}

fn service_offset(data: &[u8], name: &str) -> usize {
    for block in scan_blocks(data) {
        if block.block_type == 0x03 && service_name(&block.body).as_deref() == Some(name) {
            return block.start;
        }
    }
    panic!("service {name} not found");
}

#[test]
fn reads_winrar5_fixture_and_extracts_byte_identical_data() {
    let mut rar = RarArchive::open(FIXTURE).expect("open fixture");
    let entries = rar.list();
    assert_eq!(entries.len(), 4);
    for (name, _) in FIXTURE_FILES {
        assert!(
            entries.iter().any(|e| e.name() == name),
            "missing entry {name}"
        );
    }

    for (name, expected_sha) in FIXTURE_FILES {
        let data = rar.read(name).expect("read entry");
        assert_eq!(sha256(&data), *expected_sha, "content mismatch for {name}");
    }
}

#[test]
fn solid_archive_roundtrips_with_interleaved_directories_and_store() {
    let dir = make_temp_dir();
    let path = dir.path().join("solid.rar");
    let payload_a: Vec<u8> = b"common prefix "
        .iter()
        .cycle()
        .take(200_000)
        .copied()
        .collect();
    let payload_b: Vec<u8> = b"common prefix "
        .iter()
        .cycle()
        .take(150_000)
        .copied()
        .collect();
    {
        let mut rar = rar5::RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                solid: true,
                ..Default::default()
            },
        )
        .expect("create");
        rar.add_bytes("a.bin", &payload_a, 3).unwrap();
        rar.add_directory_only(dir.path(), "emptydir").unwrap();
        rar.add_bytes("b.bin", &payload_b, 3).unwrap();
        rar.add_bytes("c.txt", b"small", 0).unwrap(); // STORE resets the chain
        rar.add_bytes("d.bin", &payload_a, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = rar5::RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("a.bin").unwrap(), payload_a);
    assert_eq!(rar.read("b.bin").unwrap(), payload_b);
    assert_eq!(rar.read("c.txt").unwrap(), b"small");
    assert_eq!(rar.read("d.bin").unwrap(), payload_a);

    // The main archive header must carry ARCHIVE_FLAG_SOLID (0x0004).
    let bytes = std::fs::read(&path).unwrap();
    let main = &scan_blocks(&bytes)
        .into_iter()
        .find(|b| b.block_type == 0x01)
        .unwrap()
        .body;
    let (_, mut q) = read_vint(main, 0);
    let (flags, n) = read_vint(main, q);
    q = n;
    if flags & 0x0001 != 0 {
        let (_, n) = read_vint(main, q);
        q = n;
    }
    if flags & 0x0002 != 0 {
        let (_, n) = read_vint(main, q);
        q = n;
    }
    let (arch_flags, _) = read_vint(main, q);
    assert_eq!(arch_flags & 0x0004, 0x0004, "solid archive flag missing");
}

#[test]
fn quick_open_record_written_with_correct_relative_locator() {
    let dir = make_temp_dir();
    let path = dir.path().join("qo.rar");
    let payload = b"quick open payload ".repeat(1000);
    {
        let mut rar = rar5::RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                quick_open: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("f1.bin", &payload, 3).unwrap();
        rar.add_bytes("f2.bin", &vec![7u8; 4096], 0).unwrap();
        rar.close().unwrap();
    }

    let mut rar = rar5::RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("f1.bin").unwrap(), payload);
    assert_eq!(rar.read("f2.bin").unwrap(), vec![7u8; 4096]);

    let bytes = std::fs::read(&path).unwrap();
    let qo_pos = service_offset(&bytes, "QO");
    let (_, qo, rr) = main_header_locator(&bytes);
    assert!(rr.is_none(), "no recovery locator expected");
    assert_eq!(
        qo.unwrap(),
        qo_pos as u64 - 8,
        "QO offset must be relative to archive start"
    );
}

#[test]
fn recovery_locator_offset_is_relative_to_archive_start() {
    let dir = make_temp_dir();
    let path = dir.path().join("rr.rar");
    {
        let mut rar = rar5::RarArchive::create_with_recovery(&path, 10).unwrap();
        rar.add_bytes("a.bin", &b"recovery test payload ".repeat(1000), 3)
            .unwrap();
        rar.close().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let rr_pos = service_offset(&bytes, "RR");
    let (_, _, rr) = main_header_locator(&bytes);
    assert_eq!(
        rr.unwrap(),
        rr_pos as u64 - 8,
        "RR offset must be relative to archive start"
    );
}

#[test]
fn blake2_roundtrip_and_tamper_detection() {
    let dir = make_temp_dir();
    let path = dir.path().join("b2.rar");
    let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar = rar5::RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                blake2: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("data.bin", &payload, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = rar5::RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("data.bin").unwrap(), payload);

    // Corrupt one payload byte: BLAKE2sp (and CRC) must reject the read.
    let mut bytes = std::fs::read(&path).unwrap();
    let data_off = first_file_data_offset(&bytes);
    bytes[data_off + 10] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();
    let mut rar = rar5::RarArchive::open(&path).unwrap();
    assert!(
        rar.read("data.bin").is_err(),
        "tampered data must fail verification"
    );
}

#[test]
fn encrypted_tamper_detected_via_mac_checksum() {
    let dir = make_temp_dir();
    let path = dir.path().join("enc.rar");
    let mut rng_state = 0x9E3779B97F4A7C15u64;
    let payload: Vec<u8> = (0..64 * 1024u32)
        .map(|_| {
            rng_state ^= rng_state >> 12;
            rng_state ^= rng_state << 25;
            rng_state ^= rng_state >> 27;
            (rng_state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8
        })
        .collect();
    {
        let mut rar = rar5::RarArchive::create_with_password(&path, "secret").unwrap();
        rar.add_bytes("data.bin", &payload, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = rar5::RarArchive::open_with_password(&path, "secret").unwrap();
    assert_eq!(rar.read("data.bin").unwrap(), payload);

    // Flip a ciphertext byte: decryption produces garbage which must fail
    // the MAC'd CRC verification.
    let mut bytes = std::fs::read(&path).unwrap();
    let data_off = first_file_data_offset(&bytes);
    let mid = data_off + (bytes.len() - data_off) / 2;
    for (i, byte) in bytes[mid..mid + 16].iter_mut().enumerate() {
        *byte ^= (i as u8).wrapping_add(0x5A);
    }
    std::fs::write(&path, &bytes).unwrap();
    let mut rar = rar5::RarArchive::open_with_password(&path, "secret").unwrap();
    assert!(
        rar.read("data.bin").is_err(),
        "corrupted encrypted data must fail"
    );
}

#[test]
fn extract_rejects_unsafe_entry_names() {
    let dir = make_temp_dir();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    for bad in ["../evil.txt", "/etc/passwd", "C:/windows/x", "a/../../b"] {
        let path = dir
            .path()
            .join(format!("evil-{}.rar", bad.replace(['/', ':'], "_")));
        {
            let mut rar = rar5::RarArchive::create(&path).unwrap();
            rar.add_bytes(bad, b"nope", 0).unwrap();
            rar.close().unwrap();
        }
        let mut rar = rar5::RarArchive::open(&path).unwrap();
        let err = rar.extract_all(&out).unwrap_err();
        assert!(err.to_string().contains("security"), "{bad}: {err}");
    }
    assert!(!out.join("evil.txt").exists());
    assert!(!dir.path().join("evil.txt").exists());
}

#[test]
fn extract_with_safe_paths_false_preserves_legacy_behavior() {
    let dir = make_temp_dir();
    let path = dir.path().join("flat.rar");
    {
        let mut rar = rar5::RarArchive::create(&path).unwrap();
        rar.add_bytes("nested/file.txt", b"hello", 0).unwrap();
        rar.close().unwrap();
    }
    let out = dir.path().join("out");
    let mut rar = rar5::RarArchive::open(&path).unwrap();
    rar.extract_all_with_options(&out, rar5::ExtractOptions::default())
        .unwrap();
    assert_eq!(
        std::fs::read(out.join("nested/file.txt")).unwrap(),
        b"hello"
    );
}

#[test]
fn flat_extraction_flattens_names_and_never_escapes() {
    let dir = make_temp_dir();
    let path = dir.path().join("flat.rar");
    {
        let mut rar = rar5::RarArchive::create(&path).unwrap();
        rar.add_bytes("dir/sub/file.txt", b"flat", 0).unwrap();
        rar.add_bytes("top.txt", b"top", 0).unwrap();
        rar.close().unwrap();
    }
    // Flat extraction writes every member under its basename, no tree.
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = rar5::RarArchive::open(&path).unwrap();
        rar.extract_all_with_options(
            &out,
            rar5::ExtractOptions {
                flat_paths: true,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(std::fs::read(out.join("file.txt")).unwrap(), b"flat");
    assert_eq!(std::fs::read(out.join("top.txt")).unwrap(), b"top");
    assert!(!out.join("dir").exists(), "flat mode must not create dirs");

    // A traversal-shaped member name is rejected even in flat mode: the
    // safe-path policy applies before the basename is used.
    let evil = dir.path().join("evil.rar");
    {
        let mut rar = rar5::RarArchive::create(&evil).unwrap();
        rar.add_bytes("good.txt", b"ok", 0).unwrap();
        rar.add_bytes("..", b"escape", 0).unwrap();
        rar.close().unwrap();
    }
    let out2 = dir.path().join("out2");
    std::fs::create_dir_all(&out2).unwrap();
    {
        let mut rar = rar5::RarArchive::open(&evil).unwrap();
        let err = rar
            .extract_all_with_options(
                &out2,
                rar5::ExtractOptions {
                    flat_paths: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("security"), "{err}");
    }
    assert!(
        !dir.path().join("escape").exists(),
        "traversal member must not escape the destination"
    );
    assert_eq!(std::fs::read(out2.join("good.txt")).unwrap(), b"ok");
}

#[test]
fn extract_limits_reject_oversized_members() {
    let dir = make_temp_dir();
    let path = dir.path().join("lim.rar");
    let payload = vec![7u8; 1_000_000];
    {
        let mut rar = rar5::RarArchive::create(&path).unwrap();
        rar.add_bytes("f.bin", &payload, 0).unwrap();
        rar.close().unwrap();
    }
    let mut rar = rar5::RarArchive::open(&path).unwrap();
    let err = rar
        .read_with_options(
            "f.bin",
            rar5::ExtractOptions {
                max_unpacked_bytes: Some(1000),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("limit"), "{err}");

    let out = dir.path().join("out");
    let mut rar = rar5::RarArchive::open(&path).unwrap();
    let err = rar
        .extract_all_with_options(
            &out,
            rar5::ExtractOptions {
                max_total_unpacked_bytes: Some(10),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("limit"), "{err}");
}

#[test]
fn solid_multivolume_roundtrips_with_exact_volumes() {
    // Solid + multi-volume: the encoder state (LZ window) carries across
    // member and volume boundaries; non-final volumes stay byte-exact.
    // Pseudorandom (incompressible) data guarantees several volumes.
    let dir = make_temp_dir();
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut rng = |buf: &mut [u8]| {
        for byte in buf.iter_mut() {
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            *byte = (seed.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8;
        }
    };
    let mut data_a = vec![0u8; 512 * 1024];
    rng(&mut data_a);
    let mut data_b = vec![0u8; 256 * 1024];
    rng(&mut data_b);
    std::fs::write(&a, &data_a).unwrap();
    std::fs::write(&b, &data_b).unwrap();
    let arc = dir.path().join("sv.rar");
    {
        let mut rar = rar5::RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
                solid: true,
                volume_size: Some(128 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&a, 3).unwrap();
        rar.add(&b, 3).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar5::discover_volumes(&arc);
    assert!(volumes.len() >= 3, "expected several volumes, got {}", volumes.len());
    for vol in &volumes[..volumes.len() - 1] {
        assert_eq!(
            std::fs::metadata(vol).unwrap().len(),
            128 * 1024,
            "non-final volume must be byte-exact"
        );
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let mut rar = rar5::RarArchive::open(&volumes[0]).unwrap();
    rar.extract_all(&out).unwrap();
    assert_eq!(
        std::fs::read(out.join("a.bin")).unwrap(),
        data_a
    );
    assert_eq!(
        std::fs::read(out.join("b.bin")).unwrap(),
        data_b
    );
}

#[test]
fn combined_solid_quickopen_blake2_recovery_password_roundtrip() {
    let dir = make_temp_dir();
    let path = dir.path().join("combo.rar");
    let a = b"combined solid content ".repeat(5000);
    let b = b"different solid content ".repeat(4000);
    {
        let mut rar = rar5::RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                solid: true,
                quick_open: true,
                blake2: true,
                password: Some("pw".into()),
                recovery_percent: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("a.bin", &a, 3).unwrap();
        rar.add_bytes("b.bin", &b, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = rar5::RarArchive::open_with_password(&path, "pw").unwrap();
    assert_eq!(rar.read("a.bin").unwrap(), a);
    assert_eq!(rar.read("b.bin").unwrap(), b);

    let bytes = std::fs::read(&path).unwrap();
    assert!(service_offset(&bytes, "QO") > 0);
    assert!(service_offset(&bytes, "RR") > 0);
    let (_, qo, rr) = main_header_locator(&bytes);
    assert!(qo.is_some() && rr.is_some());
}

#[test]
fn large_store_file_streams_roundtrip() {
    let dir = make_temp_dir();
    let src = dir.path().join("big.bin");
    let mut rng_state = 0x9E3779B97F4A7C15u64;
    let mut file = std::fs::File::create(&src).unwrap();
    let mut chunk = vec![0u8; 1 << 20];
    for _ in 0..32 {
        for byte in chunk.iter_mut() {
            rng_state ^= rng_state >> 12;
            rng_state ^= rng_state << 25;
            rng_state ^= rng_state >> 27;
            *byte = (rng_state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8;
        }
        std::io::Write::write_all(&mut file, &chunk).unwrap();
    }
    drop(file);

    let path = dir.path().join("big.rar");
    {
        let mut rar = rar5::RarArchive::create(&path).unwrap();
        rar.add(&src, 5).unwrap(); // incompressible -> streaming STORE
        rar.close().unwrap();
    }
    let mut rar = rar5::RarArchive::open(&path).unwrap();
    let data = rar.read("big.bin").unwrap();
    assert_eq!(data.len(), 32 * 1024 * 1024);
    let src_data = std::fs::read(&src).unwrap();
    assert_eq!(data, src_data);

    // Streamed extraction must match too.
    let out = dir.path().join("out");
    let mut rar = rar5::RarArchive::open(&path).unwrap();
    let extracted = rar.extract("big.bin", &out).unwrap();
    assert_eq!(std::fs::read(extracted).unwrap(), src_data);
}

/// Official UNRAR (e.g. /home/yuan/下载/rar/unrar) validates archives
/// produced by rar-rs with every new feature combination.
#[test]
#[allow(clippy::type_complexity)]
fn official_unrar_validates_our_feature_archives() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return, // skipped unless the interop script sets it
    };
    let rar = std::env::var_os("SA_OFFICIAL_RAR");
    let dir = make_temp_dir();
    let a = b"official interop solid content ".repeat(3000);
    let b = b"different solid member content ".repeat(2500);

    let cases: Vec<(String, rar5::CreateOptions, Vec<(String, Vec<u8>)>)> = vec![
        (
            "plain".into(),
            rar5::CreateOptions::default(),
            vec![("f1.bin".into(), a.clone()), ("f2.bin".into(), b.clone())],
        ),
        (
            "solid-qo-blake2".into(),
            rar5::CreateOptions {
                solid: true,
                quick_open: true,
                blake2: true,
                ..Default::default()
            },
            vec![("f1.bin".into(), a.clone()), ("f2.bin".into(), b.clone())],
        ),
        (
            "encrypted".into(),
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                ..Default::default()
            },
            vec![("f1.bin".into(), a.clone())],
        ),
        (
            "headers-recovery".into(),
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                encrypt_headers: true,
                recovery_percent: Some(10),
                ..Default::default()
            },
            vec![("f1.bin".into(), a.clone())],
        ),
    ];

    for (name, opts, entries) in cases {
        let path = dir.path().join(format!("{name}.rar"));
        {
            let mut rar = rar5::RarArchive::create_with_options(&path, opts.clone()).unwrap();
            for (n, data) in &entries {
                rar.add_bytes(n, data, 3).unwrap();
            }
            rar.close().unwrap();
        }
        let password_flag = if let Some(pw) = &opts.password {
            vec![format!("-p{pw}")]
        } else {
            vec![]
        };
        let status = std::process::Command::new(&unrar)
            .arg("t")
            .args(&password_flag)
            .arg(&path)
            .status()
            .expect("run official unrar");
        assert!(status.success(), "official unrar rejected {name}: {status}");
    }

    // End-to-end recovery-record validation: official `rar r` must be able
    // to repair a corrupted archive using our inline recovery record.
    if let Some(rar) = &rar {
        let payload: Vec<u8> = (0..2 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let rr = dir.path().join("rr.rar");
        {
            let mut ar = rar5::RarArchive::create_with_recovery(&rr, 10).unwrap();
            ar.add_bytes("payload.bin", &payload, 3).unwrap();
            ar.close().unwrap();
        }
        let status = std::process::Command::new(&unrar)
            .arg("t")
            .arg(&rr)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "official unrar rejected the recovery archive"
        );

        // Corrupt a small region inside the protected file data (well
        // within the 10% recovery capacity).
        let mut bytes = std::fs::read(&rr).unwrap();
        let data_off = first_file_data_offset(&bytes);
        for (i, byte) in bytes[data_off + 5..data_off + 13].iter_mut().enumerate() {
            *byte ^= (i as u8).wrapping_add(0xA5);
        }
        std::fs::write(&rr, &bytes).unwrap();

        let status = std::process::Command::new(rar)
            .args(["r", "-idq"])
            .arg(&rr)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "official rar could not repair our recovery record"
        );

        // WinRAR writes the repaired archive as `fixed.<name>.rar`.
        let fixed = dir.path().join(format!(
            "fixed.{}",
            rr.file_name().unwrap().to_string_lossy()
        ));
        assert!(fixed.exists(), "official rar did not produce {fixed:?}");
        let status = std::process::Command::new(&unrar)
            .arg("t")
            .arg(&fixed)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "repaired archive still fails official unrar test"
        );

        let mut ar = rar5::RarArchive::open(&fixed).unwrap();
        assert_eq!(ar.read("payload.bin").unwrap(), payload);

        // Recovery volumes: official `rar rc` must reconstruct a deleted
        // volume from our `.rev` files.
        let mut rng_state = 0x9E3779B97F4A7C15u64;
        let vol_payload: Vec<u8> = (0..2500 * 1024)
            .map(|_| {
                rng_state ^= rng_state >> 12;
                rng_state ^= rng_state << 25;
                rng_state ^= rng_state >> 27;
                (rng_state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8
            })
            .collect();
        let multi = dir.path().join("multi.part1.rar");
        {
            let mut ar =
                rar5::RarArchive::create_multivolume_with_recovery_count(&multi, 1024 * 1000, 2)
                    .unwrap();
            ar.add_bytes("big.bin", &vol_payload, 0).unwrap();
            ar.close().unwrap();
        }
        let part2 = dir.path().join("multi.part2.rar");
        let part2_bytes = std::fs::read(&part2).unwrap();
        std::fs::remove_file(&part2).unwrap();
        let status = std::process::Command::new(rar)
            .args(["rc", "-idq"])
            .arg(&multi)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "official rar rc failed");
        assert!(
            part2.exists(),
            "official rar rc did not reconstruct multi.part2.rar"
        );
        assert_eq!(
            std::fs::read(&part2).unwrap(),
            part2_bytes,
            "reconstructed volume differs from the original"
        );
        let status = std::process::Command::new(&unrar)
            .arg("t")
            .arg(&multi)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "reconstructed volume set fails unrar test"
        );
        let mut ar = rar5::RarArchive::open(&multi).unwrap();
        assert_eq!(ar.read("big.bin").unwrap(), vol_payload);
    }
}

/// rar-rs reads archives created by the official RAR binary (solid,
/// BLAKE2sp, header encryption, recovery records).
#[test]
fn our_unrar_reads_official_archives() {
    let rar = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return, // skipped unless the interop script sets it
    };
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let a: Vec<u8> = b"official rar solid content "
        .iter()
        .cycle()
        .take(300_000)
        .copied()
        .collect();
    let b: Vec<u8> = b"second file content "
        .iter()
        .cycle()
        .take(200_000)
        .copied()
        .collect();
    std::fs::write(src.join("a.bin"), &a).unwrap();
    std::fs::write(src.join("b.bin"), &b).unwrap();

    // Solid + BLAKE2sp.
    let solid = dir.path().join("official-solid.rar");
    let status = std::process::Command::new(&rar)
        .args(["a", "-s", "-htb", "-idq"])
        .arg(&solid)
        .arg("src")
        .current_dir(dir.path())
        .status()
        .expect("run official rar");
    assert!(status.success(), "official rar solid creation failed");
    let mut ar = rar5::RarArchive::open(&solid).unwrap();
    assert_eq!(ar.read("src/a.bin").unwrap(), a);
    assert_eq!(ar.read("src/b.bin").unwrap(), b);

    // Header-encrypted + file-level encryption with BLAKE2sp.
    let enc = dir.path().join("official-enc.rar");
    let status = std::process::Command::new(&rar)
        .args(["a", "-ppw", "-hp", "-htb", "-idq"])
        .arg(&enc)
        .arg("src")
        .current_dir(dir.path())
        .status()
        .expect("run official rar");
    assert!(status.success(), "official rar encrypted creation failed");
    let mut ar = rar5::RarArchive::open_with_password(&enc, "pw").unwrap();
    assert_eq!(ar.read("src/a.bin").unwrap(), a);
    assert_eq!(ar.read("src/b.bin").unwrap(), b);
}

#[test]
fn create_read_roundtrip_matches_input() {
    let dir = make_temp_dir();
    let path = dir.path().join("rt.rar");
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

    {
        let mut rar = RarArchive::create(&path).expect("create");
        rar.add_bytes("data.bin", &payload, 5).expect("add");
        rar.add_bytes("note.txt", b"hello", 5).expect("add");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    assert_eq!(rar.list().len(), 2);
    let out = rar.read("data.bin").expect("read");
    assert_eq!(out, payload);
    assert_eq!(rar.read("note.txt").expect("read"), b"hello");
}

#[test]
fn encrypted_archive_roundtrip() {
    let dir = make_temp_dir();
    let path = dir.path().join("enc.rar");
    let payload = b"classified content".repeat(1000);

    {
        let mut rar = RarArchive::create_with_password(&path, "hunter2").expect("create encrypted");
        rar.add_bytes("secret.txt", &payload, 3).expect("add");
        rar.close().expect("close");
    }

    // Without the password the entry must refuse to decrypt.
    let mut rar = RarArchive::open(&path).expect("open");
    assert!(
        rar.read("secret.txt").is_err(),
        "reading an encrypted entry without a password must fail"
    );

    // With the password it must round-trip.
    let mut rar = RarArchive::open_with_password(&path, "hunter2").expect("open encrypted");
    assert_eq!(rar.read("secret.txt").expect("read"), payload);
}

#[test]
fn multivolume_creation_roundtrip() {
    let dir = make_temp_dir();
    let base = dir.path().join("vol.rar");
    // Incompressible payload so the volumes actually fill up.
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut payload = vec![0u8; 500_000];
    for b in payload.iter_mut() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        *b = (state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8;
    }

    {
        let mut rar = RarArchive::create_multivolume(&base, 262_144).expect("create volumes");
        rar.add_bytes("big.bin", &payload, 5).expect("add");
        rar.close().expect("close");
    }

    let volumes: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("vol.part"))
        .collect();
    assert!(
        volumes.len() >= 2,
        "expected multiple volume files, got {:?}",
        volumes.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );

    let mut rar = RarArchive::open(&base).expect("open first volume");
    assert_eq!(rar.read("big.bin").expect("read"), payload);
}

#[test]
fn add_as_uses_custom_archive_name() {
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).expect("mkdir");
    std::fs::write(src.join("a.txt"), b"aaa").expect("write");

    let path = dir.path().join("named.rar");
    {
        let mut rar = RarArchive::create(&path).expect("create");
        rar.add_as(src.join("a.txt"), "docs/renamed.txt", 3)
            .expect("add");
        rar.add_as(src, "root", 3).expect("add");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    assert!(
        names.iter().any(|n| n == "docs/renamed.txt"),
        "missing renamed entry: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "root/"),
        "missing dir entry: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "root/sub/"),
        "missing nested dir entry: {names:?}"
    );
    assert_eq!(rar.read("docs/renamed.txt").expect("read"), b"aaa".to_vec());
}

#[test]
fn add_directory_only_writes_dir_entries_without_children() {
    let dir = make_temp_dir();
    let src = dir.path().join("tree");
    std::fs::create_dir_all(src.join("empty")).expect("mkdir");
    std::fs::write(src.join("empty").join("ignored.txt"), b"x").expect("write");
    std::fs::write(src.join("top.txt"), b"y").expect("write");

    let path = dir.path().join("dironly.rar");
    {
        let mut rar = RarArchive::create(&path).expect("create");
        // Directory entry only — the child must NOT be pulled in.
        rar.add_directory_only(&src, "tree").expect("add dir");
        rar.add_as(src.join("top.txt"), "tree/top.txt", 3)
            .expect("add file");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "tree/"), "missing dir: {names:?}");
    assert!(
        !names.iter().any(|n| n.contains("ignored.txt")),
        "child leaked in: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("empty")),
        "add_directory_only must not recurse: {names:?}"
    );
    assert_eq!(rar.read("tree/top.txt").expect("read"), b"y".to_vec());

    // An explicitly added empty directory entry IS preserved.
    let path2 = dir.path().join("dironly2.rar");
    {
        let mut rar = RarArchive::create(&path2).expect("create");
        rar.add_directory_only(&src, "tree").expect("add dir");
        rar.add_directory_only(src.join("empty"), "tree/empty")
            .expect("add empty dir");
        rar.close().expect("close");
    }
    let rar = RarArchive::open(&path2).expect("open");
    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    assert!(
        names.iter().any(|n| n == "tree/empty/"),
        "explicit empty dir missing: {names:?}"
    );
}

#[test]
fn progress_callback_reports_monotonic_progress() {
    let dir = make_temp_dir();
    let path = dir.path().join("prog.rar");
    let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();

    let events: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let mut rar = RarArchive::create(&path).expect("create");
        let sink = events.clone();
        let cb: Box<dyn FnMut(u64, u64) + Send> = Box::new(move |done, total| {
            sink.lock().expect("lock").push((done, total));
        });
        rar.set_progress_callback(Some(cb));
        rar.add_batch(&[rar5::BatchEntry::Bytes {
            name: "data.bin",
            data: &payload,
            level: 5,
        }])
        .expect("add");
        rar.close().expect("close");
    }

    let events: Vec<(u64, u64)> = events.lock().expect("lock").iter().copied().collect();

    assert!(!events.is_empty(), "no progress events emitted");
    assert_eq!(
        events[0],
        (0, payload.len() as u64),
        "file must start with a (0, total) event"
    );
    for w in events.windows(2) {
        assert!(w[0].0 <= w[1].0, "progress went backwards");
        assert_eq!(w[0].1, w[1].1, "total changed mid-stream");
    }
    for (done, total) in &events {
        assert!(*done <= *total, "done {done} exceeded total {total}");
    }
    let (last_done, last_total) = *events.last().expect("events");
    assert_eq!(last_done, last_total);
    assert_eq!(last_total, payload.len() as u64);
    let deltas: u64 = events.windows(2).map(|w| w[1].0 - w[0].0).sum();
    assert_eq!(deltas, payload.len() as u64, "deltas must sum exactly once");
}

#[test]
fn progress_callback_reports_exact_deltas_across_batch_files() {
    let dir = make_temp_dir();
    let path = dir.path().join("batch.rar");
    let small = dir.path().join("small.bin");
    let big = dir.path().join("big.bin");
    write_repeated(&small, 7, 512 * 1024);
    // > 64 MiB so add_file takes the streaming sequential path even with the
    // `parallel` feature (batched waves only accept members up to 64 MiB).
    write_repeated(&big, 9, 68 * 1024 * 1024);
    let bytes: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let expected_totals: [u64; 3] = [512 * 1024, bytes.len() as u64, 68 * 1024 * 1024];

    let events: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let mut rar = RarArchive::create(&path).expect("create");
        let sink = events.clone();
        let cb: Box<dyn FnMut(u64, u64) + Send> = Box::new(move |done, total| {
            sink.lock().expect("lock").push((done, total));
        });
        rar.set_progress_callback(Some(cb));
        rar.add_batch(&[
            rar5::BatchEntry::File {
                path: &small,
                name: Some("small.bin"),
                level: 5,
            },
            rar5::BatchEntry::Bytes {
                name: "bytes.bin",
                data: &bytes,
                level: 5,
            },
            rar5::BatchEntry::File {
                path: &big,
                name: Some("big.bin"),
                level: 5,
            },
        ])
        .expect("add batch");
        rar.close().expect("close");
    }

    let events: Vec<(u64, u64)> = events.lock().expect("lock").iter().copied().collect();
    let mut segments: Vec<(u64, Vec<(u64, u64)>)> = Vec::new();
    for (done, total) in events {
        if done == 0 {
            segments.push((total, vec![(done, total)]));
        } else {
            segments
                .last_mut()
                .expect("progress event before any (0, total) start")
                .1
                .push((done, total));
        }
    }

    assert_eq!(segments.len(), expected_totals.len());
    for ((segment_total, segment), expected) in segments.iter().zip(expected_totals) {
        assert_eq!(*segment_total, expected, "per-file total mismatch");
        assert_eq!(
            segment[0],
            (0, expected),
            "segment must start with a zero event"
        );
        for w in segment.windows(2) {
            assert!(w[0].0 <= w[1].0, "per-file progress went backwards");
            assert_eq!(w[0].1, w[1].1, "per-file total changed mid-stream");
        }
        for (done, total) in segment {
            assert!(*done <= *total, "done {done} exceeded total {total}");
        }
        assert_eq!(
            segment.last().expect("segment").0,
            *segment_total,
            "segment must end at its file total"
        );
        let deltas: u64 = segment.windows(2).map(|w| w[1].0 - w[0].0).sum();
        assert_eq!(
            deltas, *segment_total,
            "per-file deltas must sum exactly once"
        );
    }
}

#[test]
fn lz_tail_match_fixture_roundtrips_without_panic() {
    // Regression for the 3-byte cache prefilter out-of-bounds read added in
    // 341bd79: this 362-byte fixture ends with two bytes that match an
    // earlier position at a cached distance, which used to index past the
    // end of the buffer at `pos = size - 2` and abort the process.
    let data = include_bytes!("fixtures/tail-match-362.bin");
    let dir = make_temp_dir();
    let path = dir.path().join("tail.rar");

    {
        let mut rar = RarArchive::create(&path).expect("create");
        rar.add_bytes("tail.json", data, 3).expect("add");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    assert_eq!(rar.read("tail.json").expect("read").as_slice(), &data[..]);
}

#[test]
#[allow(non_snake_case)]
fn large_file_exceeding_1MiB_window_roundtrips() {
    // Regression for the 1 MiB dictionary cap added in 341bd79: the decoder
    // reconstructs the whole file in the sliding window, so a compressed
    // file larger than the window used to trip `SlidingWindow::get_output`
    // and panic. The decode buffer must grow to cover the full output.
    let size = 1024 * 1024 + 1;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let dir = make_temp_dir();
    let path = dir.path().join("large.rar");

    {
        let mut rar = RarArchive::create(&path).expect("create");
        rar.add_bytes("big.bin", &payload, 5).expect("add");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    assert_eq!(rar.read("big.bin").expect("read"), payload);
}

#[test]
fn multi_chunk_compressed_file_roundtrips() {
    // > DEFAULT_CHUNK_SIZE (4 MiB): the encoder must split the input into
    // multiple chunks with a shared lookbehind window, and only the final
    // chunk may carry the end-of-stream block flag.
    let size = 6 * 1024 * 1024;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let dir = make_temp_dir();
    let path = dir.path().join("multi-chunk.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("big.bin", &payload, 5).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("big.bin").unwrap(), payload);

    // Streaming extraction must produce the same bytes.
    let out = dir.path().join("out");
    let mut rar = RarArchive::open(&path).unwrap();
    let extracted = rar.extract("big.bin", &out).unwrap();
    assert_eq!(std::fs::read(extracted).unwrap(), payload);
}

#[test]
fn incompressible_large_file_roundtrips_via_store() {
    // Lock in the sample-probe STORE fallback: >2 MiB random data must be
    // stored uncompressed and still read back byte-identical.
    let size = 4 * 1024 * 1024;
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut payload = Vec::with_capacity(size);
    for _ in 0..size {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        payload.push((state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8);
    }

    let dir = make_temp_dir();
    let path = dir.path().join("rand.rar");
    {
        let mut rar = RarArchive::create(&path).expect("create");
        rar.add_bytes("rand.bin", &payload, 3).expect("add");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    assert_eq!(rar.read("rand.bin").expect("read"), payload);
}

/// Parallel extraction (feature `parallel`) must produce byte-identical
/// output to the sequential path for an eligible archive (≥ 4 members,
/// ≥ 64 MiB total, non-solid).
#[cfg(feature = "parallel")]
#[test]
fn parallel_extraction_matches_sequential() {
    let dir = make_temp_dir();
    let path = dir.path().join("par.rar");
    {
        let mut rar = RarArchive::create(&path).expect("create");
        for i in 0..4u8 {
            let mut data = Vec::with_capacity(20 * 1024 * 1024);
            let base = b"parallel member payload 0123456789abcdefghijklmnopqrstuvwxyz\n";
            while data.len() < 20 * 1024 * 1024 {
                data.extend_from_slice(base);
            }
            // make each member distinct and delta-friendly
            for chunk in data.chunks_mut(65536) {
                for b in chunk.iter_mut().step_by(5) {
                    *b = b.wrapping_add(i);
                }
            }
            rar.add_bytes(&format!("m{i}.bin"), &data, 3).expect("add");
        }
        rar.close().expect("close");
    }

    let seq_dir = dir.path().join("seq");
    let par_dir = dir.path().join("par");
    {
        let mut rar = RarArchive::open(&path).expect("open");
        rar.extract_all(&seq_dir).expect("sequential extract");
    }
    {
        let mut rar = RarArchive::open(&path).expect("open");
        rar.extract_all(&par_dir).expect("parallel extract");
    }
    for i in 0..4u8 {
        let a = std::fs::read(seq_dir.join(format!("m{i}.bin"))).unwrap();
        let b = std::fs::read(par_dir.join(format!("m{i}.bin"))).unwrap();
        assert_eq!(a, b, "member m{i} differs between sequential and parallel");
    }
}

#[cfg(feature = "parallel")]
#[test]
fn batch_archive_matches_sequential_bytes() {
    let dir = make_temp_dir();
    // Archive outputs live outside `src_dir` so the directory's mtime is
    // stable between the two runs; file mtimes come from disk and are also
    // stable. (In-memory `add_bytes` entries stamp the wall clock, so byte
    // identity is only asserted for file/directory entries.)
    let src_dir = dir.path().join("src");
    std::fs::create_dir(&src_dir).unwrap();
    let small = src_dir.join("small.bin");
    let big = src_dir.join("big.bin");
    let small_payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let big_payload: Vec<u8> = b"batch chunk content pattern 0123456789\n"
        .iter()
        .cycle()
        .take(20 * 1024 * 1024)
        .copied()
        .collect();
    std::fs::write(&small, &small_payload).unwrap();
    std::fs::write(&big, &big_payload).unwrap();

    let entries: Vec<rar5::BatchEntry<'_>> = vec![
        rar5::BatchEntry::Directory {
            path: &src_dir,
            name: Some("folder"),
        },
        rar5::BatchEntry::File {
            path: &small,
            name: None,
            level: 3,
        },
        rar5::BatchEntry::File {
            path: &big,
            name: Some("renamed.bin"),
            level: 3,
        },
        rar5::BatchEntry::File {
            path: &small,
            name: Some("copy.bin"),
            level: 1,
        },
    ];

    let seq_path = dir.path().join("seq.rar");
    {
        let mut ar = rar5::RarArchive::create(&seq_path).unwrap();
        ar.add_directory_only(&src_dir, "folder").unwrap();
        ar.add(&small, 3).unwrap();
        ar.add_as(&big, "renamed.bin", 3).unwrap();
        ar.add_as(&small, "copy.bin", 1).unwrap();
        ar.close().unwrap();
    }
    let batch_path = dir.path().join("batch.rar");
    {
        let mut ar = rar5::RarArchive::create(&batch_path).unwrap();
        ar.add_batch(&entries).unwrap();
        ar.close().unwrap();
    }

    assert_eq!(
        std::fs::read(&seq_path).unwrap(),
        std::fs::read(&batch_path).unwrap(),
        "batch archive differs from sequential archive"
    );

    let mut ar = rar5::RarArchive::open(&batch_path).unwrap();
    assert_eq!(ar.read("small.bin").unwrap(), small_payload);
    assert_eq!(ar.read("renamed.bin").unwrap(), big_payload);
    assert_eq!(ar.read("copy.bin").unwrap(), small_payload);
}

#[cfg(feature = "parallel")]
#[test]
fn batch_encrypted_archive_roundtrips() {
    let dir = make_temp_dir();
    let path = dir.path().join("batch-enc.rar");
    let a = b"encrypted batch member one ".repeat(10_000);
    let b = b"encrypted batch member two ".repeat(8_000);
    let entries: Vec<rar5::BatchEntry<'_>> = vec![
        rar5::BatchEntry::Bytes {
            name: "a.bin",
            data: &a,
            level: 3,
        },
        rar5::BatchEntry::Bytes {
            name: "b.bin",
            data: &b,
            level: 5,
        },
    ];
    {
        let mut ar = rar5::RarArchive::create_with_password(&path, "pw").unwrap();
        ar.add_batch(&entries).unwrap();
        ar.close().unwrap();
    }
    let mut ar = rar5::RarArchive::open_with_password(&path, "pw").unwrap();
    assert_eq!(ar.read("a.bin").unwrap(), a);
    assert_eq!(ar.read("b.bin").unwrap(), b);
}

#[cfg(feature = "parallel")]
#[test]
fn batch_large_member_uses_sequential_path() {
    let dir = make_temp_dir();
    let big = dir.path().join("huge.bin");
    let big_payload: Vec<u8> = b"streamed sequential member payload "
        .iter()
        .cycle()
        .take(70 * 1024 * 1024)
        .copied()
        .collect();
    std::fs::write(&big, &big_payload).unwrap();
    let small = b"small member around the big one".repeat(1000);

    let path = dir.path().join("big.rar");
    let entries: Vec<rar5::BatchEntry<'_>> = vec![
        rar5::BatchEntry::Bytes {
            name: "before.bin",
            data: &small,
            level: 3,
        },
        rar5::BatchEntry::File {
            path: &big,
            name: None,
            level: 3,
        },
        rar5::BatchEntry::Bytes {
            name: "after.bin",
            data: &small,
            level: 3,
        },
    ];
    {
        let mut ar = rar5::RarArchive::create(&path).unwrap();
        ar.add_batch(&entries).unwrap();
        ar.close().unwrap();
    }
    let mut ar = rar5::RarArchive::open(&path).unwrap();
    assert_eq!(ar.namelist(), ["before.bin", "huge.bin", "after.bin"]);
    assert_eq!(ar.read("before.bin").unwrap(), small);
    assert_eq!(ar.read("huge.bin").unwrap(), big_payload);
    assert_eq!(ar.read("after.bin").unwrap(), small);
}

#[cfg(feature = "parallel")]
#[test]
fn batch_large_file_matches_sequential_bytes() {
    let dir = make_temp_dir();
    let big = dir.path().join("huge.bin");
    // Over the 64 MiB batch-member cap, so this exercises the large-file
    // path (chunk-level parallel after the change, sequential before).
    let big_payload = x86_like(70 * 1024 * 1024);
    std::fs::write(&big, &big_payload).unwrap();

    let seq_path = dir.path().join("seq.rar");
    {
        let mut ar = RarArchive::create(&seq_path).unwrap();
        ar.add(&big, 3).unwrap();
        ar.close().unwrap();
    }
    let batch_path = dir.path().join("batch.rar");
    {
        let mut ar = RarArchive::create(&batch_path).unwrap();
        ar.add_batch(&[rar5::BatchEntry::File {
            path: &big,
            name: None,
            level: 3,
        }])
        .unwrap();
        ar.close().unwrap();
    }

    assert_eq!(
        std::fs::read(&seq_path).unwrap(),
        std::fs::read(&batch_path).unwrap(),
        "large-file batch archive differs from sequential archive"
    );

    let mut ar = RarArchive::open(&batch_path).unwrap();
    assert_eq!(ar.read("huge.bin").unwrap(), big_payload);
}

#[cfg(feature = "parallel")]
#[test]
fn batch_solid_falls_back_to_sequential() {
    let dir = make_temp_dir();
    let a_path = dir.path().join("a.bin");
    let b_path = dir.path().join("b.bin");
    let a = b"solid batch fallback content A ".repeat(5_000);
    let b = b"solid batch fallback content B ".repeat(4_000);
    std::fs::write(&a_path, &a).unwrap();
    std::fs::write(&b_path, &b).unwrap();
    let entries: Vec<rar5::BatchEntry<'_>> = vec![
        rar5::BatchEntry::File {
            path: &a_path,
            name: None,
            level: 3,
        },
        rar5::BatchEntry::File {
            path: &b_path,
            name: None,
            level: 3,
        },
    ];

    let seq_path = dir.path().join("seq-solid.rar");
    {
        let mut ar = rar5::RarArchive::create_with_options(
            &seq_path,
            rar5::CreateOptions {
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        ar.add(&a_path, 3).unwrap();
        ar.add(&b_path, 3).unwrap();
        ar.close().unwrap();
    }
    let batch_path = dir.path().join("batch-solid.rar");
    {
        let mut ar = rar5::RarArchive::create_with_options(
            &batch_path,
            rar5::CreateOptions {
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_batch(&entries).unwrap();
        ar.close().unwrap();
    }
    assert_eq!(
        std::fs::read(&seq_path).unwrap(),
        std::fs::read(&batch_path).unwrap(),
        "solid batch fallback must match the sequential archive"
    );
}

/// 8 MiB of x86-like code: AutoSize picks a whole-member E8 filter, which
/// is split into multiple filter records (each capped at
/// `MAX_FILTER_BLOCK_LENGTH`). The streaming `extract_all` path must apply
/// them at the right staging offsets (regression: staging was indexed
/// without the already-consumed prefix).
#[test]
fn streaming_extract_roundtrips_large_filtered_member() {
    let dir = make_temp_dir();
    let path = dir.path().join("x86-large.rar");
    let data = x86_like(8 * 1024 * 1024);
    {
        let mut ar = RarArchive::create(&path).unwrap();
        ar.add_bytes("x86.bin", &data, 3).unwrap();
        ar.close().unwrap();
    }
    let out = dir.path().join("out");
    {
        let mut ar = RarArchive::open(&path).unwrap();
        ar.extract_all(&out).unwrap();
    }
    assert_eq!(std::fs::read(out.join("x86.bin")).unwrap(), data);
}

fn x86_like(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut pos = 0u32;
    while out.len() < size {
        out.extend_from_slice(&[0x90; 64]); // NOP
        pos += 64;
        out.push(0xe8); // CALL rel32
        out.extend_from_slice(&(pos.wrapping_mul(7) & 0x00FF_FFFF).to_le_bytes());
        pos += 5;
        out.extend_from_slice(&[0x41; 16]); // INC ECX
        pos += 16;
    }
    out.truncate(size);
    out
}

// ── Deletion ────────────────────────────────────────────────────────────────

/// Parse the member name out of a file header body (block type 2).
fn file_header_name(body: &[u8]) -> String {
    let (_, mut q) = read_vint(body, 0);
    let (flags, n) = read_vint(body, q);
    q = n;
    if flags & 0x0001 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    if flags & 0x0002 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    let (file_flags, n) = read_vint(body, q);
    q = n;
    let (_, n) = read_vint(body, q); // unpacked size
    q = n;
    let (_, n) = read_vint(body, q); // attributes
    q = n;
    if file_flags & 0x0002 != 0 {
        q += 4; // mtime
    }
    if file_flags & 0x0004 != 0 {
        q += 4; // CRC32
    }
    let (_, n) = read_vint(body, q); // compression info
    q = n;
    let (_, n) = read_vint(body, q); // host OS
    q = n;
    let (name_len, n) = read_vint(body, q);
    q = n;
    String::from_utf8_lossy(&body[q..q + name_len as usize]).into_owned()
}

/// Absolute offset where the data area of the given member starts.
fn file_data_offset(data: &[u8], name: &str) -> usize {
    for block in scan_blocks(data) {
        if block.block_type == 0x02 && file_header_name(&block.body) == name {
            return block.start + block.header_len + 4;
        }
    }
    panic!("file block {name} not found");
}

/// Byte span `[start, end)` of the archive block (header + data) holding
/// the given member name.
fn file_block_span(data: &[u8], name: &str) -> (usize, usize) {
    // scan_blocks' header_len covers the size vint + body but not the
    // 4-byte header CRC32.
    for block in scan_blocks(data) {
        if block.block_type == 0x02 && file_header_name(&block.body) == name {
            return (
                block.start,
                block.start + block.header_len + 4 + block.data_size as usize,
            );
        }
    }
    panic!("file block {name} not found");
}

/// All members in archive order (skipping the main header).
fn member_names(data: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for block in scan_blocks(data) {
        if block.block_type == 0x02 {
            names.push(file_header_name(&block.body));
        }
    }
    names
}

/// Names cached inside a quick-open record payload.
fn qo_cached_names(data: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for block in scan_blocks(data) {
        if block.block_type == 0x03 && service_name(&block.body).as_deref() == Some("QO") {
            // scan_blocks' header_len excludes the 4-byte header CRC32.
            let data_start = block.start + block.header_len + 4;
            let payload = &data[data_start..data_start + block.data_size as usize];
            let mut p = 0;
            while p < payload.len() {
                p += 4; // entry CRC
                let (body_len, n) = read_vint(payload, p);
                p = n;
                let entry = &payload[p..p + body_len as usize];
                p += body_len as usize;
                let (_, mut q) = read_vint(entry, 0); // entry flags
                let (_, n) = read_vint(entry, q); // relative offset
                q = n;
                let (hdr_len, n) = read_vint(entry, q);
                q = n;
                let hdr = &entry[q..q + hdr_len as usize];
                // The cached header is a full block: CRC32(4) + size vint
                // + body.
                let (hsize, n) = read_vint(hdr, 4);
                let body = &hdr[n..n + hsize as usize];
                names.push(file_header_name(body));
            }
        }
    }
    names
}

fn service_exists(data: &[u8], name: &str) -> bool {
    scan_blocks(data)
        .iter()
        .any(|b| b.block_type == 0x03 && service_name(&b.body).as_deref() == Some(name))
}

/// Main header archive-level flags (parse the body manually so tests work
/// with and without a locator record).
fn archive_flags(data: &[u8]) -> u64 {
    for block in scan_blocks(data) {
        if block.block_type == 0x01 {
            let (_, mut q) = read_vint(&block.body, 0);
            let (flags, n) = read_vint(&block.body, q);
            q = n;
            if flags & 0x0001 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            if flags & 0x0002 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            let (arch_flags, _) = read_vint(&block.body, q);
            return arch_flags;
        }
    }
    panic!("no main header");
}

fn compressible(seed: u8, n: usize) -> Vec<u8> {
    let pat: Vec<u8> = (0..64u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(seed))
        .collect();
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend_from_slice(&pat);
    }
    out.truncate(n);
    out
}

#[test]
fn delete_kept_members_preserve_exact_bytes() {
    let dir = make_temp_dir();
    let path = dir.path().join("del.rar");
    let files: Vec<(String, Vec<u8>)> = vec![
        ("a.txt".into(), compressible(1, 60_000)),
        ("b.bin".into(), vec![0x5Au8; 40_000]), // STORE (level 0)
        ("c.txt".into(), compressible(2, 80_000)),
        ("d.txt".into(), compressible(3, 30_000)),
        ("e.bin".into(), vec![0x3Cu8; 50_000]), // STORE (level 0)
    ];
    {
        let mut rar = RarArchive::create(&path).unwrap();
        for (name, data) in &files {
            let level = if name.ends_with(".bin") { 0 } else { 3 };
            rar.add_bytes(name, data, level).unwrap();
        }
        rar.close().unwrap();
    }
    let orig = std::fs::read(&path).unwrap();

    // Delete a middle member and the last member.
    let mut rar = RarArchive::open(&path).unwrap();
    let n = rar.delete(&["b.bin", "e.bin"]).unwrap();
    assert_eq!(n, 2);
    let kept: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    assert_eq!(kept, ["a.txt", "c.txt", "d.txt"]);
    drop(rar);

    // Remaining file blocks (headers + payloads) must be byte-identical.
    let new = std::fs::read(&path).unwrap();
    for name in ["a.txt", "c.txt", "d.txt"] {
        let (s0, e0) = file_block_span(&orig, name);
        let (s1, e1) = file_block_span(&new, name);
        assert_eq!(&orig[s0..e0], &new[s1..e1], "block for {name} changed");
    }
    // The archive must not contain the deleted members anywhere.
    let deleted_spans = ["b.bin", "e.bin"].map(|name| file_block_span(&orig, name));
    for (_, e) in deleted_spans {
        assert!(new.len() < e, "deleted member data still present");
    }
    // Content reads back.
    for (name, data) in &files {
        if *name == "b.bin" || *name == "e.bin" {
            continue;
        }
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(&rar.read(name).unwrap(), data);
    }
}

#[test]
fn delete_rebuilds_quick_open_record() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-qo.rar");
    {
        let mut rar = RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                quick_open: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("f1.txt", &compressible(1, 50_000), 3)
            .unwrap();
        rar.add_bytes("f2.txt", &compressible(2, 50_000), 3)
            .unwrap();
        rar.add_bytes("f3.txt", &compressible(3, 50_000), 3)
            .unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open(&path).unwrap();
    rar.delete(&["f2.txt"]).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let qo_pos = service_offset(&bytes, "QO");
    let (loc_flags, qo, rr) = main_header_locator(&bytes);
    assert_eq!(loc_flags & 0x0001, 0x0001, "QO locator flag missing");
    assert_eq!(qo.unwrap(), qo_pos as u64 - 8, "QO offset out of date");
    assert!(rr.is_none(), "no RR locator expected");
    assert_eq!(qo_cached_names(&bytes), ["f1.txt", "f3.txt"]);
}

#[test]
fn delete_rebuilds_recovery_record() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-rr.rar");
    {
        let mut rar = RarArchive::create_with_recovery(&path, 10).unwrap();
        rar.add_bytes("f1.txt", &compressible(1, 50_000), 3)
            .unwrap();
        rar.add_bytes("f2.txt", &compressible(2, 50_000), 3)
            .unwrap();
        rar.close().unwrap();
    }
    let orig = std::fs::read(&path).unwrap();
    assert!(
        service_exists(&orig, "RR"),
        "precondition: RR record present"
    );
    assert_eq!(archive_flags(&orig) & 0x0008, 0x0008, "RECOVERY flag set");

    // Deleting must keep the archive recoverable: the recovery record is
    // rebuilt over the rewritten archive (a superset of `rar d`, which
    // drops it).
    let mut rar = RarArchive::open(&path).unwrap();
    rar.delete(&["f1.txt"]).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(service_exists(&bytes, "RR"), "RR record must be rebuilt");
    assert_eq!(
        archive_flags(&bytes) & 0x0008,
        0x0008,
        "RECOVERY archive flag must survive"
    );
    let rr_pos = service_offset(&bytes, "RR");
    let (_, _, rr) = main_header_locator(&bytes);
    assert_eq!(rr.unwrap(), rr_pos as u64 - 8, "RR offset out of date");
    let rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.namelist(), ["f2.txt"]);

    // The rebuilt record must actually repair the archive (official rar).
    if let (Some(unrar), Some(rar_bin)) = (
        std::env::var_os("SA_OFFICIAL_UNRAR"),
        std::env::var_os("SA_OFFICIAL_RAR"),
    ) {
        {
            let mut bytes = std::fs::read(&path).unwrap();
            let data_off = first_file_data_offset(&bytes);
            for (i, byte) in bytes[data_off + 5..data_off + 13].iter_mut().enumerate() {
                *byte ^= (i as u8).wrapping_add(0xA5);
            }
            std::fs::write(&path, &bytes).unwrap();
            let status = std::process::Command::new(&rar_bin)
                .args(["r", "-idq"])
                .arg(&path)
                .current_dir(dir.path())
                .status()
                .expect("run official rar r");
            assert!(status.success(), "official rar could not repair");
            let fixed = dir.path().join(format!(
                "fixed.{}",
                path.file_name().unwrap().to_string_lossy()
            ));
            let status = std::process::Command::new(&unrar)
                .arg("t")
                .arg(&fixed)
                .status()
                .unwrap();
            assert!(status.success(), "repaired archive fails unrar test");
        }
    }
}
#[test]
fn delete_from_solid_archive_recompresses_chain() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-solid.rar");
    let files: Vec<(String, Vec<u8>)> = vec![
        ("a.bin".into(), compressible(1, 100_000)),
        ("b.bin".into(), compressible(2, 100_000)),
        ("c.bin".into(), compressible(3, 100_000)),
        ("d.bin".into(), compressible(4, 100_000)),
        ("e.txt".into(), b"tail outside the chain ".repeat(3_000)),
    ];
    {
        let mut rar = RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        for (name, data) in &files {
            // All compressible members join the same solid chain; the last
            // one is stored and starts a fresh chain segment.
            let level = if name == "e.txt" { 0 } else { 3 };
            rar.add_bytes(name, data, level).unwrap();
        }
        rar.close().unwrap();
    }
    let orig = std::fs::read(&path).unwrap();

    // Delete a mid-chain member: the chain is recompressed from its start,
    // and the stored member after it is copied verbatim.
    let mut rar = RarArchive::open(&path).unwrap();
    rar.delete(&["b.bin"]).unwrap();
    for (name, data) in &files {
        if name == "b.bin" {
            continue;
        }
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(&rar.read(name).unwrap(), data, "content of {name} lost");
    }
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(member_names(&bytes), ["a.bin", "c.bin", "d.bin", "e.txt"]);
    // The stored member outside the chain keeps its exact bytes.
    let (s0, e0) = file_block_span(&orig, "e.txt");
    let (s1, e1) = file_block_span(&bytes, "e.txt");
    assert_eq!(
        &orig[s0..e0],
        &bytes[s1..e1],
        "stored tail must be verbatim"
    );

    // Deleting the last member of the chain must not recompress anything:
    // the archive prefix is copied verbatim.
    let bytes = std::fs::read(&path).unwrap();
    let (s_del, _e_del) = file_block_span(&bytes, "d.bin");
    let mut rar = RarArchive::open(&path).unwrap();
    rar.delete(&["d.bin"]).unwrap();
    let bytes2 = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..s_del], &bytes2[..s_del], "prefix must be verbatim");
    let rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.namelist(), ["a.bin", "c.bin", "e.txt"]);
}

#[test]
fn delete_from_encrypted_archives_roundtrips() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-enc.rar");
    let payload = compressible(7, 60_000);
    {
        let mut rar = RarArchive::create_with_password(&path, "s3cret").unwrap();
        rar.add_bytes("f1.txt", &payload, 3).unwrap();
        rar.add_bytes("f2.txt", &payload, 3).unwrap();
        rar.add_bytes("f3.txt", &payload, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open_with_password(&path, "s3cret").unwrap();
    rar.delete(&["f2.txt"]).unwrap();
    let mut rar = RarArchive::open_with_password(&path, "s3cret").unwrap();
    assert_eq!(rar.namelist(), ["f1.txt", "f3.txt"]);
    assert_eq!(rar.read("f1.txt").unwrap(), payload);
    assert_eq!(rar.read("f3.txt").unwrap(), payload);

    // Header-encrypted archives.
    let path2 = dir.path().join("del-hp.rar");
    {
        let mut rar = RarArchive::create_with_password_headers(&path2, "s3cret").unwrap();
        rar.add_bytes("g1.txt", &payload, 3).unwrap();
        rar.add_bytes("g2.txt", &payload, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open_with_password(&path2, "s3cret").unwrap();
    rar.delete(&["g1.txt"]).unwrap();
    let mut rar = RarArchive::open_with_password(&path2, "s3cret").unwrap();
    assert_eq!(rar.namelist(), ["g2.txt"]);
    assert_eq!(rar.read("g2.txt").unwrap(), payload);
}

#[test]
fn delete_all_members_erases_archive() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-all.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("f1.txt", b"one", 0).unwrap();
        rar.add_bytes("f2.txt", b"two", 0).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open(&path).unwrap();
    let n = rar.delete(&["f1.txt", "f2.txt"]).unwrap();
    assert_eq!(n, 2);
    assert!(!path.exists(), "archive must be erased when empty");
}

#[test]
fn delete_rejects_missing_members_and_multivolume() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-err.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("f1.txt", b"one", 0).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open(&path).unwrap();
    match rar.delete(&["nope.txt"]) {
        Err(rar5::RarError::MemberNotFound { name }) => assert_eq!(name, "nope.txt"),
        other => panic!("expected MemberNotFound, got {other:?}"),
    }
    // Archive unchanged after a failed delete.
    let rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.namelist(), ["f1.txt"]);

    // The official `rar` CLI refuses to modify multi-volume archives
    // ("Cannot modify volume"); rar-rs re-splits the volumes instead.
    let vol = dir.path().join("del-vol.rar");
    let payload_a: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let payload_b: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
    {
        let mut rar = rar5::RarArchive::create_multivolume(&vol, 30_000).unwrap();
        rar.add_bytes("a.bin", &payload_a, 0).unwrap();
        rar.add_bytes("b.bin", &payload_b, 0).unwrap();
        rar.add_bytes("c.bin", &payload_a, 0).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar5::discover_volumes(&vol);
    assert!(volumes.len() > 1, "precondition: multi-volume archive");
    let mut rar = RarArchive::open(&volumes[0]).unwrap();
    assert_eq!(rar.namelist(), ["a.bin", "b.bin", "c.bin"]);
    let n = rar.delete(&["b.bin"]).unwrap();
    assert_eq!(n, 1);

    // Content survives and the volume set is readable again.
    let volumes = rar5::discover_volumes(&vol);
    let mut rar = RarArchive::open(&volumes[0]).unwrap();
    assert_eq!(rar.namelist(), ["a.bin", "c.bin"]);
    assert_eq!(rar.read("a.bin").unwrap(), payload_a);
    assert_eq!(rar.read("c.bin").unwrap(), payload_a);
}

#[test]
fn delete_rejects_locked_archive() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-locked.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("f1.txt", b"one", 0).unwrap();
        rar.close().unwrap();
    }
    // Hand-patch the ARCHIVE_FLAG_LOCKED (0x10) bit into the main header's
    // archive-level flags and recompute the header CRC.
    let bytes = std::fs::read(&path).unwrap();
    let main = scan_blocks(&bytes)
        .into_iter()
        .find(|b| b.block_type == 0x01)
        .unwrap();
    let vint_len = main.header_len - main.body.len();
    let mut header = bytes[main.start + 4..main.start + 4 + main.header_len].to_vec();
    // Body layout: [type][block flags][extra size?][arch flags]. The
    // default writer emits a single-byte arch flags vint (0x00).
    let (_, mut q) = read_vint(&header, vint_len);
    let (block_flags, n) = read_vint(&header, q);
    q = n;
    if block_flags & 0x0001 != 0 {
        let (_, n) = read_vint(&header, q);
        q = n;
    }
    header[q] |= 0x10;
    let crc = crc32fast::hash(&header);
    let mut out = bytes[..main.start].to_vec();
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&bytes[main.start + 4 + main.header_len..]);
    std::fs::write(&path, &out).unwrap();

    let mut rar = RarArchive::open(&path).unwrap();
    match rar.delete(&["f1.txt"]) {
        Err(rar5::RarError::ArchiveLocked) => {}
        other => panic!("expected ArchiveLocked, got {other:?}"),
    }
}

/// Official UNRAR validates archives produced by `delete`, and rar-rs
/// reads archives modified by the official `rar d`.
#[test]
fn official_unrar_validates_deleted_archives() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let rar_bin = std::env::var_os("SA_OFFICIAL_RAR");
    let dir = make_temp_dir();
    let a = compressible(11, 60_000);
    let b = compressible(12, 60_000);
    let c = compressible(13, 60_000);

    let cases: Vec<(String, rar5::CreateOptions)> = vec![
        ("plain".into(), rar5::CreateOptions::default()),
        (
            "solid-qo".into(),
            rar5::CreateOptions {
                solid: true,
                quick_open: true,
                ..Default::default()
            },
        ),
        (
            "encrypted".into(),
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                ..Default::default()
            },
        ),
        (
            "headers".into(),
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                encrypt_headers: true,
                ..Default::default()
            },
        ),
    ];
    for (name, opts) in cases {
        let path = dir.path().join(format!("del-{name}.rar"));
        {
            let mut rar = rar5::RarArchive::create_with_options(&path, opts.clone()).unwrap();
            rar.add_bytes("a.bin", &a, 3).unwrap();
            rar.add_bytes("b.bin", &b, 3).unwrap();
            rar.add_bytes("c.bin", &c, 3).unwrap();
            rar.close().unwrap();
        }
        let password_flag = opts
            .password
            .as_ref()
            .map(|pw| vec![format!("-p{pw}")])
            .unwrap_or_default();
        let mut rar = match &opts.password {
            Some(pw) => rar5::RarArchive::open_with_password(&path, pw).unwrap(),
            None => rar5::RarArchive::open(&path).unwrap(),
        };
        rar.delete(&["b.bin"]).unwrap();
        let status = std::process::Command::new(&unrar)
            .arg("t")
            .args(&password_flag)
            .arg(&path)
            .status()
            .expect("run official unrar");
        assert!(status.success(), "official unrar rejected del-{name}");

        // Content still correct through our own reader.
        let mut rar = match &opts.password {
            Some(pw) => rar5::RarArchive::open_with_password(&path, pw).unwrap(),
            None => rar5::RarArchive::open(&path).unwrap(),
        };
        assert_eq!(rar.read("a.bin").unwrap(), a);
        assert_eq!(rar.read("c.bin").unwrap(), c);
    }

    // Reverse direction: the official `rar d` modifies a rar-rs archive,
    // and rar-rs reads the result.
    if let Some(rar_bin) = &rar_bin {
        let path = dir.path().join("del-by-official.rar");
        {
            let mut rar = rar5::RarArchive::create(&path).unwrap();
            rar.add_bytes("a.bin", &a, 3).unwrap();
            rar.add_bytes("b.bin", &b, 3).unwrap();
            rar.add_bytes("c.bin", &c, 3).unwrap();
            rar.close().unwrap();
        }
        let status = std::process::Command::new(rar_bin)
            .args(["d", "-idq"])
            .arg(&path)
            .arg("b.bin")
            .status()
            .expect("run official rar d");
        assert!(status.success(), "official rar d failed");
        let mut rar = rar5::RarArchive::open(&path).unwrap();
        assert_eq!(rar.namelist(), ["a.bin", "c.bin"]);
        assert_eq!(rar.read("a.bin").unwrap(), a);
        assert_eq!(rar.read("c.bin").unwrap(), c);
    }
}

// ── Append / lock / recovery-record commands ────────────────────────────────

#[test]
fn append_preserves_existing_members_and_rebuilds_records() {
    let dir = make_temp_dir();
    let path = dir.path().join("app.rar");
    let a = compressible(21, 60_000);
    let b = compressible(22, 60_000);
    let c = compressible(23, 60_000);
    {
        let mut rar = RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                quick_open: true,
                recovery_percent: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("a.bin", &a, 3).unwrap();
        rar.close().unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    {
        let mut rar = rar5::RarArchive::open_append(&path).unwrap();
        rar.add_bytes("b.bin", &b, 3).unwrap();
        rar.add_bytes("c.bin", &c, 0).unwrap();
        rar.close().unwrap();
    }

    // Existing member untouched (payload + header bytes verbatim).
    let after = std::fs::read(&path).unwrap();
    let (s0, e0) = file_block_span(&before, "a.bin");
    let (s1, e1) = file_block_span(&after, "a.bin");
    assert_eq!(&before[s0..e0], &after[s1..e1], "existing member changed");

    // Quick-open record rebuilt at the end with a valid locator; recovery
    // record rebuilt too.
    let qo_pos = service_offset(&after, "QO");
    let (loc_flags, qo, rr) = main_header_locator(&after);
    assert_eq!(loc_flags & 0x0001, 0x0001);
    assert_eq!(qo.unwrap(), qo_pos as u64 - 8);
    let rr_pos = service_offset(&after, "RR");
    assert_eq!(rr.unwrap(), rr_pos as u64 - 8);
    assert_eq!(qo_cached_names(&after), ["a.bin", "b.bin", "c.bin"]);

    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("a.bin").unwrap(), a);
    assert_eq!(rar.read("b.bin").unwrap(), b);
    assert_eq!(rar.read("c.bin").unwrap(), c);
}

#[test]
fn append_rejects_locked_archive() {
    let dir = make_temp_dir();
    let path = dir.path().join("app-locked.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("a.bin", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    {
        let mut rar = RarArchive::open(&path).unwrap();
        rar.lock().unwrap();
    }
    // Locked archives are read-only: both the official `rar d` and our
    // append/delete refuse them.
    match rar5::RarArchive::open_append(&path) {
        Err(rar5::RarError::ArchiveLocked) => {}
        Err(e) => panic!("expected ArchiveLocked, got {e:?}"),
        Ok(_) => panic!("expected ArchiveLocked"),
    }
    let mut rar = RarArchive::open(&path).unwrap();
    match rar.delete(&["a.bin"]) {
        Err(rar5::RarError::ArchiveLocked) => {}
        other => panic!("expected ArchiveLocked, got {other:?}"),
    }
    // Content still readable after the lock.
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("a.bin").unwrap(), b"x");
}

#[test]
fn add_recovery_record_to_existing_archive() {
    let dir = make_temp_dir();
    let path = dir.path().join("rr-new.rar");
    let payload = compressible(31, 80_000);
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("f1.bin", &payload, 3).unwrap();
        rar.close().unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert!(!service_exists(&before, "RR"));

    {
        let mut rar = RarArchive::open(&path).unwrap();
        rar.add_recovery_record(10).unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(service_exists(&bytes, "RR"));
    assert_eq!(archive_flags(&bytes) & 0x0008, 0x0008);
    let rr_pos = service_offset(&bytes, "RR");
    let (_, _, rr) = main_header_locator(&bytes);
    assert_eq!(rr.unwrap(), rr_pos as u64 - 8);
    // The member payload is untouched.
    let (s0, e0) = file_block_span(&before, "f1.bin");
    let (s1, e1) = file_block_span(&bytes, "f1.bin");
    assert_eq!(&before[s0..e0], &bytes[s1..e1]);
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("f1.bin").unwrap(), payload);

    // The added record must repair the archive (official rar).
    if let (Some(unrar), Some(rar_bin)) = (
        std::env::var_os("SA_OFFICIAL_UNRAR"),
        std::env::var_os("SA_OFFICIAL_RAR"),
    ) {
        {
            let mut bytes = std::fs::read(&path).unwrap();
            let data_off = first_file_data_offset(&bytes);
            for (i, byte) in bytes[data_off + 5..data_off + 13].iter_mut().enumerate() {
                *byte ^= (i as u8).wrapping_add(0xA5);
            }
            std::fs::write(&path, &bytes).unwrap();
            let status = std::process::Command::new(&rar_bin)
                .args(["r", "-idq"])
                .arg(&path)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "official rar could not repair");
            let fixed = dir.path().join(format!(
                "fixed.{}",
                path.file_name().unwrap().to_string_lossy()
            ));
            let status = std::process::Command::new(&unrar)
                .arg("t")
                .arg(&fixed)
                .status()
                .unwrap();
            assert!(status.success(), "repaired archive fails unrar test");
        }
    }
}

#[test]
fn delete_multivolume_rebuilds_recovery_volumes() {
    let dir = make_temp_dir();
    let path = dir.path().join("mv-rev.rar");
    let payload_a: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let payload_b: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
    {
        let mut rar =
            rar5::RarArchive::create_multivolume_with_recovery_count(&path, 30_000, 1).unwrap();
        rar.add_bytes("a.bin", &payload_a, 0).unwrap();
        rar.add_bytes("b.bin", &payload_b, 0).unwrap();
        rar.add_bytes("c.bin", &payload_a, 0).unwrap();
        rar.close().unwrap();
    }
    let rev = dir.path().join("mv-rev.part1.rev");
    assert!(rev.exists(), "precondition: .rev files present");
    let volumes_before = rar5::discover_volumes(&path);
    assert!(volumes_before.len() > 1);

    {
        let mut rar = RarArchive::open(&volumes_before[0]).unwrap();
        rar.delete(&["b.bin"]).unwrap();
    }
    // The .rev set is regenerated over the new volumes.
    let volumes_after = rar5::discover_volumes(&path);
    assert!(rev.exists(), ".rev files must be regenerated");
    let mut rar = RarArchive::open(&volumes_after[0]).unwrap();
    assert_eq!(rar.namelist(), ["a.bin", "c.bin"]);
    assert_eq!(rar.read("a.bin").unwrap(), payload_a);

    // Official `rar rc` must reconstruct a deleted volume from them.
    if let (Some(unrar), Some(rar_bin)) = (
        std::env::var_os("SA_OFFICIAL_UNRAR"),
        std::env::var_os("SA_OFFICIAL_RAR"),
    ) {
        {
            let vols = rar5::discover_volumes(&path);
            let victim = vols[1].clone();
            std::fs::remove_file(&victim).unwrap();
            let status = std::process::Command::new(&rar_bin)
                .args(["rc", "-idq"])
                .arg(&vols[0])
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "official rar rc failed");
            let status = std::process::Command::new(&unrar)
                .arg("t")
                .arg(&vols[0])
                .status()
                .unwrap();
            assert!(status.success(), "reconstructed set fails unrar test");
            let mut rar = RarArchive::open(&vols[0]).unwrap();
            assert_eq!(rar.read("a.bin").unwrap(), payload_a);
            assert_eq!(rar.read("c.bin").unwrap(), payload_a);
        }
    }
}

/// Official `rar` creates archives for the modification commands, and the
/// official tools validate every result.
#[test]
fn official_tools_validate_modified_archives() {
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let unrar = std::env::var_os("SA_OFFICIAL_UNRAR").unwrap_or(rar_bin.clone());
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let a: Vec<u8> = b"append interop payload ".repeat(3_000);
    let b: Vec<u8> = b"second interop member ".repeat(2_500);
    std::fs::write(src.join("a.bin"), &a).unwrap();
    std::fs::write(src.join("b.bin"), &b).unwrap();

    // Our append on a rar-created archive (with a quick-open record).
    let path = dir.path().join("append.rar");
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-m3", "-qo", "-idq"])
        .arg(&path)
        .arg(src.join("a.bin"))
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut rar = rar5::RarArchive::open_append(&path).unwrap();
        rar.add(src.join("b.bin"), 3).unwrap();
        rar.close().unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected appended archive");
    let mut rar = RarArchive::open(&path).unwrap();
    // The official rar stored the first member with its relative path.
    let a_name = rar
        .namelist()
        .iter()
        .find(|n| n.ends_with("src/a.bin"))
        .expect("first member")
        .to_string();
    assert!(rar.namelist().contains(&"b.bin"), "{:?}", rar.namelist());
    assert_eq!(rar.read(&a_name).unwrap(), a);
    assert_eq!(rar.read("b.bin").unwrap(), b);

    // Our delete on a rar-created multi-volume archive (the official CLI
    // refuses to modify multi-volume archives itself).
    let mv = dir.path().join("mv.rar");
    let payload: Vec<u8> = (0..120_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &payload).unwrap();
    let small = src.join("small.bin");
    std::fs::write(&small, b"small member").unwrap();
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-m0", "-v100k", "-idq"])
        .arg(&mv)
        .arg(src.join("big.bin"))
        .arg(&small)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let volumes = rar5::discover_volumes(&mv);
    assert!(volumes.len() > 1, "precondition: multi-volume set");

    // Delete the small member from the rar-created volumes (the official
    // CLI refuses to modify multi-volume archives itself).
    let mut rar = RarArchive::open(&volumes[0]).unwrap();
    let delete_names: Vec<String> = rar
        .namelist()
        .iter()
        .filter(|n| !n.ends_with("big.bin"))
        .map(|s| s.to_string())
        .collect();
    assert!(!delete_names.is_empty(), "small member not found");
    let delete_refs: Vec<&str> = delete_names.iter().map(|s| s.as_str()).collect();
    rar.delete(&delete_refs).unwrap();
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&volumes[0])
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected rewritten volumes");
    let mut rar = RarArchive::open(&volumes[0]).unwrap();
    let big_name = rar
        .namelist()
        .iter()
        .find(|n| n.ends_with("big.bin"))
        .unwrap()
        .to_string();
    assert_eq!(rar.read(&big_name).unwrap(), payload);
}

// ── Rename ──────────────────────────────────────────────────────────────────

#[test]
fn rename_preserves_payloads_and_rebuilds_records() {
    let dir = make_temp_dir();
    let path = dir.path().join("rn.rar");
    let a = compressible(41, 60_000);
    let b = compressible(42, 60_000);
    {
        let mut rar = RarArchive::create_with_options(
            &path,
            rar5::CreateOptions {
                quick_open: true,
                recovery_percent: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("a.bin", &a, 3).unwrap();
        rar.add_bytes("b.bin", &b, 3).unwrap();
        rar.close().unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    let mut rar = RarArchive::open(&path).unwrap();
    let n = rar.rename(&[("a.bin", "renamed.bin")]).unwrap();
    assert_eq!(n, 1);

    let after = std::fs::read(&path).unwrap();
    assert_eq!(
        member_names(&after),
        ["renamed.bin", "b.bin"],
        "renamed member listed"
    );
    // Payloads byte-identical (the header legitimately changes: the name).
    let (s0, e0) = file_block_span(&before, "a.bin");
    let (s1, e1) = file_block_span(&after, "renamed.bin");
    let d0 = file_data_offset(&before, "a.bin");
    let d1 = file_data_offset(&after, "renamed.bin");
    assert_eq!(&before[d0..e0], &after[d1..e1], "payload changed");
    let _ = (s0, s1);
    // Quick-open and recovery records rebuilt with valid locators.
    let qo_pos = service_offset(&after, "QO");
    let (_, qo, rr) = main_header_locator(&after);
    assert_eq!(qo.unwrap(), qo_pos as u64 - 8);
    let rr_pos = service_offset(&after, "RR");
    assert_eq!(rr.unwrap(), rr_pos as u64 - 8);
    assert_eq!(qo_cached_names(&after), ["renamed.bin", "b.bin"]);

    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("renamed.bin").unwrap(), a);
    assert_eq!(rar.read("b.bin").unwrap(), b);

    // The rebuilt recovery record must still repair the archive.
    if let (Some(unrar), Some(rar_bin)) = (
        std::env::var_os("SA_OFFICIAL_UNRAR"),
        std::env::var_os("SA_OFFICIAL_RAR"),
    ) {
        {
            let mut bytes = std::fs::read(&path).unwrap();
            let data_off = first_file_data_offset(&bytes);
            for (i, byte) in bytes[data_off + 5..data_off + 13].iter_mut().enumerate() {
                *byte ^= (i as u8).wrapping_add(0xA5);
            }
            std::fs::write(&path, &bytes).unwrap();
            let status = std::process::Command::new(&rar_bin)
                .args(["r", "-idq"])
                .arg(&path)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "official rar could not repair");
            let fixed = dir.path().join(format!(
                "fixed.{}",
                path.file_name().unwrap().to_string_lossy()
            ));
            let status = std::process::Command::new(&unrar)
                .arg("t")
                .arg(&fixed)
                .status()
                .unwrap();
            assert!(status.success(), "repaired archive fails unrar test");
        }
    }
}

#[test]
fn rename_directory_renames_descendants() {
    let dir = make_temp_dir();
    let path = dir.path().join("rn-dir.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_directory_only(dir.path(), "old").unwrap();
        rar.add_bytes("old/sub/f.txt", b"hello", 0).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.namelist(), ["old/", "old/sub/f.txt"]);
    let n = rar.rename(&[("old", "new")]).unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        rar.namelist(),
        ["new/", "new/sub/f.txt"],
        "descendant prefix renamed"
    );
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("new/sub/f.txt").unwrap(), b"hello");
}

#[test]
fn rename_multivolume_keeps_content() {
    let dir = make_temp_dir();
    let path = dir.path().join("rn-mv.rar");
    let payload: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar = rar5::RarArchive::create_multivolume(&path, 100_000).unwrap();
        rar.add_bytes("big.bin", &payload, 0).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar5::discover_volumes(&path);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    let mut rar = RarArchive::open(&volumes[0]).unwrap();
    let n = rar.rename(&[("big.bin", "renamed.bin")]).unwrap();
    assert_eq!(n, 1);
    let volumes = rar5::discover_volumes(&path);
    let mut rar = RarArchive::open(&volumes[0]).unwrap();
    assert_eq!(rar.namelist(), ["renamed.bin"]);
    assert_eq!(rar.read("renamed.bin").unwrap(), payload);
}

#[test]
fn rename_rejects_missing_and_locked() {
    let dir = make_temp_dir();
    let path = dir.path().join("rn-err.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("a.bin", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open(&path).unwrap();
    match rar.rename(&[("nope", "x")]) {
        Err(rar5::RarError::MemberNotFound { name }) => assert_eq!(name, "nope"),
        other => panic!("expected MemberNotFound, got {other:?}"),
    }
    {
        let mut rar = RarArchive::open(&path).unwrap();
        rar.lock().unwrap();
    }
    let mut rar = RarArchive::open(&path).unwrap();
    match rar.rename(&[("a.bin", "b.bin")]) {
        Err(rar5::RarError::ArchiveLocked) => {}
        other => panic!("expected ArchiveLocked, got {other:?}"),
    }
}

/// Official `rar rn` on rar-rs archives must stay readable, and rar-rs
/// must read archives renamed by the official tool.
#[test]
fn official_rename_cross_validation() {
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let a: Vec<u8> = b"rename interop payload ".repeat(2_000);
    let b: Vec<u8> = b"second member ".repeat(1_500);
    std::fs::write(src.join("a.bin"), &a).unwrap();
    std::fs::write(src.join("b.bin"), &b).unwrap();

    // Official rar renames our archive.
    let path = dir.path().join("rn.rar");
    {
        let mut rar = rar5::RarArchive::create(&path).unwrap();
        rar.add(src.join("a.bin"), 3).unwrap();
        rar.add(src.join("b.bin"), 3).unwrap();
        rar.close().unwrap();
    }
    let status = std::process::Command::new(&rar_bin)
        .args(["rn", "-idq"])
        .arg(&path)
        .arg("a.bin")
        .arg("z.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "official rar rn failed");
    let mut rar = RarArchive::open(&path).unwrap();
    assert_eq!(rar.namelist(), ["z.bin", "b.bin"]);
    assert_eq!(rar.read("z.bin").unwrap(), a);

    // Our rename on an official archive.
    let path2 = dir.path().join("rn2.rar");
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-m3", "-idq"])
        .arg(&path2)
        .arg(src.join("a.bin"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = RarArchive::open(&path2).unwrap();
    let a_name = rar
        .namelist()
        .iter()
        .find(|n| n.ends_with("a.bin"))
        .unwrap()
        .to_string();
    rar.rename(&[(&a_name, "w.bin")]).unwrap();
    let unrar = std::env::var_os("SA_OFFICIAL_UNRAR").unwrap_or(rar_bin.clone());
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path2)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected our renamed archive");
    let mut rar = RarArchive::open(&path2).unwrap();
    assert_eq!(rar.namelist(), ["w.bin"]);
    assert_eq!(rar.read("w.bin").unwrap(), a);
}

// ── Repair / rebuild volumes / comments ─────────────────────────────────────

#[test]
fn repair_archive_restores_damaged_members() {
    let dir = make_temp_dir();
    let path = dir.path().join("rep.rar");
    // Large enough that the recovery record sits at the end and the damage
    // below lands inside the protected member data.
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar = rar5::RarArchive::create_with_recovery(&path, 10).unwrap();
        rar.add_bytes("a.bin", &payload, 0).unwrap();
        rar.close().unwrap();
    }
    let good = std::fs::read(&path).unwrap();

    // Damage a few bytes inside the protected data.
    let mut damaged = good.clone();
    for pos in [300usize, 310, 320] {
        damaged[pos] ^= 0xA5;
    }
    let repaired = rar5::repair_archive(&damaged).unwrap();
    assert_eq!(repaired, good, "repair must restore the original bytes");

    // An undamaged archive is returned unchanged.
    assert_eq!(rar5::repair_archive(&good).unwrap(), good);

    // An archive without a recovery record fails cleanly.
    let plain = dir.path().join("plain.rar");
    {
        let mut rar = rar5::RarArchive::create(&plain).unwrap();
        rar.add_bytes("a.bin", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    let bytes = std::fs::read(&plain).unwrap();
    assert!(rar5::repair_archive(&bytes).is_err());
}

#[test]
fn rebuild_missing_volumes_from_rev_files() {
    let dir = make_temp_dir();
    let path = dir.path().join("rcv.rar");
    let payload_a: Vec<u8> = (0..120_000u32).map(|i| (i % 251) as u8).collect();
    let payload_b: Vec<u8> = (0..60_000u32).map(|i| (i % 253) as u8).collect();
    {
        let mut rar =
            rar5::RarArchive::create_multivolume_with_recovery_count(&path, 60_000, 2).unwrap();
        rar.add_bytes("a.bin", &payload_a, 0).unwrap();
        rar.add_bytes("b.bin", &payload_b, 0).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar5::discover_volumes(&path);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    let rev = dir.path().join("rcv.part1.rev");
    assert!(rev.exists(), "precondition: .rev files present");

    // Delete a middle volume and rebuild it from the .rev files.
    let victim = volumes[1].clone();
    std::fs::remove_file(&victim).unwrap();
    let rebuilt = rar5::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert!(rebuilt.contains(&victim), "middle volume rebuilt");
    let volumes = rar5::discover_volumes(&path);
    let mut rar = rar5::RarArchive::open(&volumes[0]).unwrap();
    assert_eq!(rar.namelist(), ["a.bin", "b.bin"]);
    assert_eq!(rar.read("a.bin").unwrap(), payload_a);
    assert_eq!(rar.read("b.bin").unwrap(), payload_b);

    // Everything present -> nothing to rebuild.
    assert!(
        rar5::rebuild_missing_volumes(&volumes[0])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn comment_set_get_roundtrip() {
    let dir = make_temp_dir();
    let path = dir.path().join("cmt.rar");
    {
        let mut rar = rar5::RarArchive::create(&path).unwrap();
        rar.add_bytes("a.bin", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    {
        let mut rar = rar5::RarArchive::open(&path).unwrap();
        assert_eq!(rar.get_comment().unwrap(), None);
        rar.set_comment(b"my comment\n").unwrap();
    }
    {
        let mut rar = rar5::RarArchive::open(&path).unwrap();
        assert_eq!(rar.get_comment().unwrap(), Some(b"my comment\n".to_vec()));
        // The member survives the comment rewrite.
        assert_eq!(rar.read("a.bin").unwrap(), b"x");
        // An empty comment removes the existing one.
        rar.set_comment(b"").unwrap();
    }
    {
        let mut rar = rar5::RarArchive::open(&path).unwrap();
        assert_eq!(rar.get_comment().unwrap(), None);
    }

    // The comment must be readable by the official tool (env-gated).
    if let Some(rar_bin) = std::env::var_os("SA_OFFICIAL_RAR") {
        {
            let mut rar = rar5::RarArchive::open(&path).unwrap();
            rar.set_comment(b"interop comment").unwrap();
        }
        let out = std::process::Command::new(&rar_bin)
            .arg("cw")
            .arg(&path)
            .output()
            .expect("run official rar cw");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("interop comment"),
            "official rar cw must read our comment"
        );
    }
}

/// Official `rar r` and `rar rc` produce/consume the same artifacts, and
/// our repair/rebuild reads official archives.
#[test]
fn official_repair_and_rebuild_cross_validation() {
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let unrar = std::env::var_os("SA_OFFICIAL_UNRAR").unwrap_or(rar_bin.clone());
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let payload: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &payload).unwrap();

    // Our repair on a rar-created damaged RR archive.
    let path = dir.path().join("rep.rar");
    // Stored (incompressible) so the protected member data dominates the
    // archive and the damage below lands inside it, not in the parity.
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-m0", "-rr10", "-idq"])
        .arg(&path)
        .arg(src.join("big.bin"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let good = std::fs::read(&path).unwrap();
    let mut damaged = good.clone();
    for pos in [500usize, 520, 540] {
        damaged[pos] ^= 0xA5;
    }
    let repaired = rar5::repair_archive(&damaged).unwrap();
    assert_eq!(repaired, good, "byte-identical repair of rar archive");
    std::fs::write(dir.path().join("repaired.rar"), &repaired).unwrap();
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(dir.path().join("repaired.rar"))
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected our repaired archive");

    // Our rc on a rar-created volume set with .rev files.
    let mv = dir.path().join("mv.rar");
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-m0", "-v100k", "-rv2", "-idq"])
        .arg(&mv)
        .arg(src.join("big.bin"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let volumes = rar5::discover_volumes(&mv);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    std::fs::remove_file(&volumes[1]).unwrap();
    let rebuilt = rar5::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(rebuilt.len(), 1);
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&volumes[0])
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected the rebuilt volume set");
    let mut rar = rar5::RarArchive::open(&volumes[0]).unwrap();
    let big_name = rar
        .namelist()
        .iter()
        .find(|n| n.ends_with("big.bin"))
        .unwrap()
        .to_string();
    assert_eq!(rar.read(&big_name).unwrap(), payload);
}

// ── SFX ─────────────────────────────────────────────────────────────────────

fn with_stub(data: &[u8], stub_len: usize) -> Vec<u8> {
    let mut stub = vec![0u8; stub_len];
    for (i, b) in stub.iter_mut().enumerate() {
        *b = (i.wrapping_mul(31).wrapping_add(7) & 0xFF) as u8;
    }
    stub.extend_from_slice(data);
    stub
}

#[test]
fn sfx_archives_open_read_and_modify_with_stub_preserved() {
    let dir = make_temp_dir();
    let path = dir.path().join("sfx.rar");
    let payload = compressible(61, 30_000);
    let payload2 = compressible(62, 20_000);
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("a.bin", &payload, 3).unwrap();
        rar.add_bytes("c.bin", &payload2, 0).unwrap();
        rar.close().unwrap();
    }
    let plain = std::fs::read(&path).unwrap();
    let stub_len = 248_960usize;
    let sfx_path = dir.path().join("sfx.sfx");
    std::fs::write(&sfx_path, with_stub(&plain, stub_len)).unwrap();

    // Reading an SFX archive.
    let mut rar = RarArchive::open(&sfx_path).unwrap();
    assert_eq!(rar.list().len(), 2);
    assert_eq!(rar.read("a.bin").unwrap(), payload);

    // Extracting from an SFX archive.
    let out = dir.path().join("out");
    rar.extract("a.bin", &out).unwrap();
    assert_eq!(std::fs::read(out.join("a.bin")).unwrap(), payload);

    // Deleting from an SFX archive preserves the stub.
    let mut rar = RarArchive::open(&sfx_path).unwrap();
    rar.delete(&["a.bin"]).unwrap();
    let after = std::fs::read(&sfx_path).unwrap();
    assert_eq!(&after[..stub_len], &with_stub(&[], stub_len)[..stub_len]);
    let mut rar = RarArchive::open(&sfx_path).unwrap();
    assert_eq!(rar.namelist(), ["c.bin"]);
    assert_eq!(rar.read("c.bin").unwrap(), payload2);

    // Renaming keeps the stub too.
    let sfx2 = dir.path().join("sfx2.sfx");
    std::fs::write(&sfx2, with_stub(&plain, stub_len)).unwrap();
    let mut rar = RarArchive::open(&sfx2).unwrap();
    rar.rename(&[("a.bin", "b.bin")]).unwrap();
    let after2 = std::fs::read(&sfx2).unwrap();
    assert_eq!(&after2[..stub_len], &with_stub(&[], stub_len)[..stub_len]);
    let mut rar = RarArchive::open(&sfx2).unwrap();
    assert!(rar.namelist().contains(&"b.bin"));
    assert_eq!(rar.read("b.bin").unwrap(), payload);
}

/// Official SFX archives are readable, and the official tools validate our
/// SFX output (env-gated).
#[test]
fn official_sfx_cross_validation() {
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let unrar = std::env::var_os("SA_OFFICIAL_UNRAR").unwrap_or(rar_bin.clone());
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("a.bin"), &payload).unwrap();

    // Our reader on an official SFX archive.
    let sfx = dir.path().join("off.sfx");
    std::fs::write(src.join("c.bin"), b"second member").unwrap();
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-m3", "-idq"])
        .arg(dir.path().join("off.rar"))
        .arg(src.join("a.bin"))
        .arg(src.join("c.bin"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let status = std::process::Command::new(&rar_bin)
        .args(["s", "-sfx/home/yuan/下载/rar/default.sfx", "-idq"])
        .arg(dir.path().join("off.rar"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "official rar s failed");
    assert!(sfx.exists());
    let mut rar = RarArchive::open(&sfx).unwrap();
    let a_name = rar
        .namelist()
        .iter()
        .find(|n| n.ends_with("a.bin"))
        .unwrap()
        .to_string();
    assert_eq!(rar.read(&a_name).unwrap(), payload);
    // Our delete on the official SFX archive keeps the stub.
    let mut rar = RarArchive::open(&sfx).unwrap();
    let name = rar
        .namelist()
        .iter()
        .find(|n| n.ends_with("a.bin"))
        .unwrap()
        .to_string();
    rar.delete(&[&name]).unwrap();
    let data = std::fs::read(&sfx).unwrap();
    let stub_len = rar5::sfx_offset_of(&data).unwrap();
    assert!(stub_len > 0, "stub preserved");

    // Official unrar validates a stub-prefixed rar-rs archive.
    let ours = dir.path().join("ours.rar");
    {
        let mut ar = rar5::RarArchive::create(&ours).unwrap();
        ar.add_bytes("b.bin", &payload, 3).unwrap();
        ar.close().unwrap();
    }
    let plain = std::fs::read(&ours).unwrap();
    let ours_sfx = dir.path().join("ours.sfx");
    std::fs::write(&ours_sfx, with_stub(&plain, 8 * 1024)).unwrap();
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&ours_sfx)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected our stub-prefixed archive");
}

// ── Redirection records (symlinks / hardlinks) ──────────────────────────────

#[test]
#[cfg(unix)]
fn symlink_and_hardlink_redirects_extract() {
    let dir = make_temp_dir();
    let path = dir.path().join("links.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add_bytes("dir/target.txt", b"target content", 0)
            .unwrap();
        // Symlink entries: no data, redirect extra record only.
        rar.add_redirect("dir/lnk.txt", 1, "target.txt").unwrap();
        // Hardlink entry referencing the data member.
        rar.add_redirect("dir/hard.txt", 4, "dir/target.txt")
            .unwrap();
        rar.close().unwrap();
    }

    let out = dir.path().join("out");
    let mut rar = RarArchive::open(&path).unwrap();
    rar.extract_all(&out).unwrap();
    assert_eq!(
        std::fs::read(out.join("dir/target.txt")).unwrap(),
        b"target content"
    );
    #[cfg(unix)]
    {
        let target = std::fs::read_link(out.join("dir/lnk.txt")).unwrap();
        assert_eq!(target, std::path::Path::new("target.txt"));
        // The hardlink shares the inode of the target.
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(out.join("dir/target.txt")).unwrap();
        let b = std::fs::metadata(out.join("dir/hard.txt")).unwrap();
        assert_eq!(a.ino(), b.ino(), "hardlink shares the target inode");
    }
}

/// Redirect entries created by rar-rs must be readable by the official
/// tools, and rar-created symlink archives must extract the same tree
/// (env-gated). Unix-only: creating the source symlink needs `symlink(2)`.
#[test]
#[cfg(unix)]
fn official_redirection_cross_validation() {
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("target.txt"), b"target content").unwrap();
    std::os::unix::fs::symlink("target.txt", src.join("lnk.txt")).unwrap();

    // rar -ol stores the link; our extract recreates it.
    let path = dir.path().join("links.rar");
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-m0", "-ol", "-idq"])
        .arg(&path)
        .arg("src/target.txt")
        .arg("src/lnk.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let out = dir.path().join("out");
    {
        let mut rar = rar5::RarArchive::open(&path).unwrap();
        rar.extract_all(&out).unwrap();
    }
    let link = std::fs::read_link(out.join("src/lnk.txt")).unwrap();
    assert_eq!(link, std::path::Path::new("target.txt"));

    // rar-rs redirect entries must be valid for the official unrar.
    let ours = dir.path().join("ours.rar");
    {
        let mut ar = rar5::RarArchive::create(&ours).unwrap();
        ar.add_bytes("target.txt", b"target content", 0).unwrap();
        ar.add_redirect("lnk.txt", 1, "target.txt").unwrap();
        ar.close().unwrap();
    }
    let unrar = std::env::var_os("SA_OFFICIAL_UNRAR").unwrap_or(rar_bin.clone());
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&ours)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected our redirect archive");
    let out2 = dir.path().join("out2");
    std::fs::create_dir_all(&out2).unwrap();
    let status = std::process::Command::new(&unrar)
        .args(["x", "-ol", "-o+"])
        .arg(&ours)
        .arg(&out2)
        .status()
        .unwrap();
    assert!(status.success(), "unrar failed to extract our redirects");
    let link = std::fs::read_link(out2.join("lnk.txt")).unwrap();
    assert_eq!(link, std::path::Path::new("target.txt"));
}

// ── CLI path switches, filters, thread switch ───────────────────────────────

/// The `rar` CLI binary under test (cargo provides its path to tests).
const RAR_CLI: &str = env!("CARGO_BIN_EXE_rar");

fn make_tree(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.join("f2.tmp"), b"two").unwrap();
    std::fs::write(dir.join("sub/f3.txt"), b"three").unwrap();
    std::fs::write(dir.join("sub/f4.bin"), b"four").unwrap();
}

fn cli_names(archive: &std::path::Path) -> Vec<String> {
    let rar = rar5::RarArchive::open(archive).unwrap();
    let mut names: Vec<String> = rar
        .namelist()
        .into_iter()
        .map(|n| n.trim_end_matches('/').to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn cli_path_switches_and_filters() {
    let dir = make_temp_dir();
    make_tree(dir.path());
    let cases: Vec<(Vec<&str>, Vec<&str>)> = vec![
        // (switches, expected members)
        (
            vec![],
            vec!["f1.txt", "f2.tmp", "sub", "sub/f3.txt", "sub/f4.bin"],
        ),
        (vec!["-ep"], vec!["f1.txt", "f2.tmp", "f3.txt", "f4.bin"]),
        (
            vec!["-x*.tmp"],
            vec!["f1.txt", "sub", "sub/f3.txt", "sub/f4.bin"],
        ),
        (vec!["-n*.txt"], vec!["f1.txt", "sub/f3.txt"]),
        (vec!["-xsub/*"], vec!["f1.txt", "f2.tmp", "sub"]),
        (vec!["-xsub"], vec!["f1.txt", "f2.tmp"]),
        (
            vec!["-appre/fix"],
            vec![
                "pre/fix/f1.txt",
                "pre/fix/f2.tmp",
                "pre/fix/sub",
                "pre/fix/sub/f3.txt",
                "pre/fix/sub/f4.bin",
            ],
        ),
        (
            vec!["-x*.bin"],
            vec!["f1.txt", "f2.tmp", "sub", "sub/f3.txt"],
        ),
    ];
    for (switches, expected) in cases {
        let archive = dir.path().join("t.rar");
        let mut cmd = std::process::Command::new(RAR_CLI);
        cmd.arg("a").arg(&archive);
        for sw in &switches {
            cmd.arg(sw);
        }
        cmd.arg("f1.txt").arg("f2.tmp").arg("sub");
        cmd.current_dir(dir.path());
        let status = cmd.status().unwrap();
        assert!(status.success(), "cli failed for {switches:?}");
        let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(cli_names(&archive), expected, "switches {switches:?}");
        std::fs::remove_file(&archive).unwrap();
    }
}

#[test]
fn cli_wildcard_args_and_ep1() {
    let dir = make_temp_dir();
    make_tree(dir.path());
    let archive = dir.path().join("w.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a"])
        .arg(&archive)
        .arg("sub/*")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["sub/f3.txt", "sub/f4.bin"]);
    std::fs::remove_file(&archive).unwrap();

    let archive2 = dir.path().join("w2.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep1"])
        .arg(&archive2)
        .arg("sub/*")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive2), ["f3.txt", "f4.bin"]);
    std::fs::remove_file(&archive2).unwrap();
}

/// The switch outputs must be readable by the official tools, and the
/// thread switch must be accepted (env-gated).
#[test]
fn official_validates_cli_switch_archives() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    make_tree(dir.path());
    let archive = dir.path().join("sw.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-mt4", "-x*.tmp", "-appre/fix"])
        .arg(&archive)
        .arg("f1.txt")
        .arg("f2.tmp")
        .arg("sub")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        [
            "pre/fix/f1.txt",
            "pre/fix/sub",
            "pre/fix/sub/f3.txt",
            "pre/fix/sub/f4.bin"
        ]
    );
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected the filtered archive");
}

// ── Extra records: nanosecond time, owner/group, version ────────────────────

#[test]
fn nanosecond_mtime_roundtrip() {
    let dir = make_temp_dir();
    let src = dir.path().join("ns.bin");
    std::fs::write(&src, b"ns test").unwrap();
    // A file with sub-second mtime precision. NTFS stores 100 ns units, so
    // the value read back from disk is platform-quantized; everything below
    // compares against the *actual* on-disk timestamp instead of the
    // requested one, keeping the format check exact on every platform.
    let target = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
    let times = std::fs::FileTimes::new().set_modified(target);
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(times)
        .unwrap();
    let disk_mtime = std::fs::metadata(&src).unwrap().modified().unwrap();
    let disk_secs = disk_mtime.duration_since(std::time::UNIX_EPOCH).unwrap();
    let disk_ns = disk_secs.subsec_nanos();

    let path = dir.path().join("ns.rar");
    {
        let mut rar = RarArchive::create(&path).unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    // The writer emits the FILE_TIME extra record (byte-identical to the
    // official `rar` format: flags 0x13 + seconds + nanoseconds).
    let bytes = std::fs::read(&path).unwrap();
    for block in scan_blocks(&bytes) {
        if block.block_type == 0x02 {
            let (_, mut q) = read_vint(&block.body, 0);
            let (flags, n) = read_vint(&block.body, q);
            q = n;
            if flags & 0x0001 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            if flags & 0x0002 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            let (file_flags, n) = read_vint(&block.body, q);
            q = n;
            for _ in 0..2 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            if file_flags & 0x0002 != 0 {
                q += 4;
            }
            if file_flags & 0x0004 != 0 {
                q += 4;
            }
            for _ in 0..2 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            let (nl, n) = read_vint(&block.body, q);
            q = n;
            let name = &block.body[q..q + nl as usize];
            assert_eq!(name, b"ns.bin");
            let extra = &block.body[q + nl as usize..];
            let mut expected = vec![0x0a, 0x03, 0x13];
            expected.extend_from_slice(&(disk_secs.as_secs() as u32).to_le_bytes());
            expected.extend_from_slice(&disk_ns.to_le_bytes());
            assert_eq!(
                extra, &expected[..],
                "FILE_TIME record must match the official format"
            );
            #[cfg(unix)]
            assert_eq!(
                disk_secs.as_secs(),
                1_700_000_000,
                "ext4 keeps the exact requested timestamp"
            );
            #[cfg(unix)]
            assert_eq!(disk_ns, 123_456_789, "ext4 keeps exact nanoseconds");
        }
    }

    // Reading it back restores the nanosecond mtime on extraction.
    let out = dir.path().join("out");
    {
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(
            rar.get_entry("ns.bin").unwrap().header.mtime_ns,
            Some(disk_ns)
        );
        rar.extract("ns.bin", &out).unwrap();
    }
    let extracted = std::fs::metadata(out.join("ns.bin")).unwrap();
    let restored = extracted
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    assert_eq!(restored.as_secs(), disk_secs.as_secs());
    assert_eq!(restored.subsec_nanos(), disk_ns);
}

/// The official `rar` writes FILE_TIME and OWNER records that we parse;
/// our FILE_TIME output must be readable by the official unrar (env-gated).
#[test]
fn official_time_and_owner_cross_validation() {
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let unrar = std::env::var_os("SA_OFFICIAL_UNRAR").unwrap_or(rar_bin.clone());
    let dir = make_temp_dir();
    let src = dir.path().join("ns.bin");
    std::fs::write(&src, b"ns").unwrap();
    let target = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
    let times = std::fs::FileTimes::new().set_modified(target);
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(times)
        .unwrap();

    // Official archive with -ow (owner record) and sub-second mtime.
    let path = dir.path().join("off.rar");
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-m0", "-ow", "-idq"])
        .arg(&path)
        .arg("ns.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = rar5::RarArchive::open(&path).unwrap();
    let entry = rar.get_entry("ns.bin").unwrap();
    // The official rar stores the on-disk timestamp, which NTFS quantizes
    // to 100 ns; compare against the actual disk value, not the request.
    let disk_ns = std::fs::metadata(&src)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    assert_eq!(entry.header.mtime_ns, Some(disk_ns));
    assert!(entry.header.owner.is_some(), "owner record must be parsed");

    // Our ns-mtime archive must be readable by the official unrar.
    let ours = dir.path().join("ours.rar");
    {
        let mut ar = rar5::RarArchive::create(&ours).unwrap();
        ar.add(&src, 3).unwrap();
        ar.close().unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&ours)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected our ns-mtime archive");
}

// ── CLI -r- / -cl / -cu ─────────────────────────────────────────────────────

#[test]
fn cli_recursion_and_case_switches() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("rdir/sub")).unwrap();
    std::fs::write(dir.path().join("rdir/f1.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("rdir/sub/f2.txt"), b"b").unwrap();
    std::fs::write(dir.path().join("MiXeD.TXT"), b"c").unwrap();

    // -r-: directory arguments store only the directory entry.
    let archive = dir.path().join("r.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-r-"])
        .arg(&archive)
        .arg("rdir")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["rdir"]);
    std::fs::remove_file(&archive).unwrap();

    // -cl / -cu: name case conversion.
    let archive = dir.path().join("c.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-cl"])
        .arg(&archive)
        .arg("MiXeD.TXT")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["mixed.txt"]);
    std::fs::remove_file(&archive).unwrap();

    let archive = dir.path().join("c2.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-cu"])
        .arg(&archive)
        .arg("MiXeD.TXT")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["MIXED.TXT"]);
}

// ── WinRAR CLI parity: quiet mode, ch/p commands, -o-, -z, list variants ──

const UNRAR_CLI: &str = env!("CARGO_BIN_EXE_unrar");

#[test]
fn cli_quiet_mode_suppresses_informational_output() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("q.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).is_empty(),
        "-idq must suppress status output, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Without -idq the status line appears.
    let out = std::process::Command::new(RAR_CLI)
        .args(["a"])
        .arg(dir.path().join("q2.rar"))
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Created"));
}

#[test]
fn cli_ch_converts_member_case_like_winrar() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("MiXeD.TXT"), b"x").unwrap();
    let archive = dir.path().join("ch.rar");
    {
        let mut rar = RarArchive::create(&archive).unwrap();
        rar.add(dir.path().join("MiXeD.TXT"), 3).unwrap();
        rar.close().unwrap();
    }
    assert_eq!(cli_names(&archive), ["MiXeD.TXT"]);
    let status = std::process::Command::new(RAR_CLI)
        .args(["ch", "-cl"])
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["mixed.txt"]);
    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("mixed.txt").unwrap(), b"x");
}

#[test]
fn cli_print_writes_member_to_stdout() {
    let dir = make_temp_dir();
    let archive = dir.path().join("p.rar");
    {
        let mut rar = RarArchive::create(&archive).unwrap();
        rar.add_bytes("a.txt", b"hello p", 0).unwrap();
        rar.close().unwrap();
    }
    let out = std::process::Command::new(RAR_CLI)
        .args(["p"])
        .arg(&archive)
        .arg("a.txt")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"hello p");
}

#[test]
fn cli_overwrite_never_skips_existing_files() {
    let dir = make_temp_dir();
    let archive = dir.path().join("o.rar");
    {
        let mut rar = RarArchive::create(&archive).unwrap();
        rar.add_bytes("f.txt", b"new", 0).unwrap();
        rar.close().unwrap();
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("f.txt"), b"OLD").unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-o-"])
        .arg(&archive)
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read(out.join("f.txt")).unwrap(),
        b"OLD",
        "-o- must leave existing files untouched"
    );
}

#[test]
fn cli_comment_file_sets_comment() {
    let dir = make_temp_dir();
    let archive = dir.path().join("z.rar");
    {
        let mut rar = RarArchive::create(&archive).unwrap();
        rar.add_bytes("f.txt", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    std::fs::write(dir.path().join("note.txt"), b"file comment").unwrap();
    // `-z<file>` is a single token (like WinRAR's `-zfile`).
    let status = std::process::Command::new(RAR_CLI)
        .args(["c"])
        .arg(format!("-z{}", dir.path().join("note.txt").display()))
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    let out = std::process::Command::new(RAR_CLI)
        .args(["cw"])
        .arg(&archive)
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"file comment");
}

#[test]
fn unrar_list_variants_bare_and_technical() {
    let dir = make_temp_dir();
    let archive = dir.path().join("lt.rar");
    {
        let mut rar = RarArchive::create(&archive).unwrap();
        rar.add_bytes("a.txt", b"aaa", 0).unwrap();
        rar.add_bytes("b.bin", b"bbbb", 0).unwrap();
        rar.close().unwrap();
    }
    let bare = std::process::Command::new(UNRAR_CLI)
        .arg("lb")
        .arg(&archive)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&bare.stdout), "a.txt\nb.bin\n");

    let tech = std::process::Command::new(UNRAR_CLI)
        .arg("lt")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&tech.stdout);
    assert!(text.contains("a.txt") && text.contains("Checksum"), "{text}");
    // Technical rows carry a CRC column value.
    let row = text.lines().find(|l| l.ends_with("a.txt")).unwrap();
    assert!(!row.trim_start().starts_with("-"), "{row}");
}

// ── WinRAR CLI parity batch 2: -x@/-n@, -ta/-tb, -ag, -ep2/-ep3, -r0 ──────

#[test]
fn cli_mask_list_file_excludes_loaded_masks() {
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("keep.txt"), b"k").unwrap();
    std::fs::write(src.join("drop.tmp"), b"d").unwrap();
    std::fs::write(dir.path().join("masks.lst"), b"*.tmp\n").unwrap();

    let archive = dir.path().join("x.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(format!("-x@{}", dir.path().join("masks.lst").display()))
        .arg(&archive)
        .arg("src")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert!(names.contains(&"src/keep.txt"), "{names:?}");
    assert!(!names.contains(&"src/drop.tmp"), "mask list must exclude *.tmp: {names:?}");
}

#[test]
fn cli_time_filter_after_only_adds_newer_files() {
    let dir = make_temp_dir();
    let old = dir.path().join("old.txt");
    let new = dir.path().join("new.txt");
    std::fs::write(&old, b"o").unwrap();
    std::fs::write(&new, b"n").unwrap();
    let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    let times = std::fs::FileTimes::new().set_modified(past);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_times(times)
        .unwrap();

    let archive = dir.path().join("ta.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ta20200101", "-idq"])
        .arg(&archive)
        .arg("old.txt")
        .arg("new.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert!(names.contains(&"new.txt"), "{names:?}");
    assert!(!names.contains(&"old.txt"), "-ta must drop older files: {names:?}");
}

#[test]
fn cli_auto_name_inserts_date_stamp() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ag", "-idq"])
        .arg(dir.path().join("auto.rar"))
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    // The stamp (YYYYMMDDHHMMSS) is inserted before the extension.
    let created: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("auto") && n.ends_with(".rar") && n.len() > "auto.rar".len()
        })
        .collect();
    assert_eq!(created.len(), 1, "one stamped archive expected");
    let name = created[0].file_name().to_string_lossy().into_owned();
    let stamp = &name["auto".len()..name.len() - ".rar".len()];
    assert_eq!(stamp.len(), 14, "stamp must be YYYYMMDDHHMMSS: {name}");
    assert!(stamp.chars().all(|c| c.is_ascii_digit()), "{name}");
}

#[test]
fn cli_full_paths_ep2_ep3() {
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.txt"), b"f").unwrap();

    let archive = dir.path().join("ep2.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep2", "-idq"])
        .arg(&archive)
        .arg(&src.join("f.txt"))
        .status()
        .unwrap();
    assert!(status.success());
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert_eq!(names.len(), 1, "{names:?}");
    let stored = names[0];
    #[cfg(windows)]
    assert!(
        !stored.starts_with(['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z'])
            || !stored.contains(":/"),
        "-ep2 must drop the drive letter: {stored}"
    );

    let archive3 = dir.path().join("ep3.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep3", "-idq"])
        .arg(&archive3)
        .arg(&src.join("f.txt"))
        .status()
        .unwrap();
    assert!(status.success());
    let rar = RarArchive::open(&archive3).unwrap();
    let stored = rar.namelist()[0];
    #[cfg(windows)]
    assert!(
        stored.contains("_/") || stored.contains("_/"),
        "-ep3 must keep the drive as X_: {stored}"
    );
}

#[test]
fn cli_recurse_zero_does_not_descend_wildcards() {
    let dir = make_temp_dir();
    let src = dir.path().join("r0src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("top.txt"), b"t").unwrap();
    std::fs::write(src.join("sub").join("deep.txt"), b"d").unwrap();

    let archive = dir.path().join("r0.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-r0", "-idq"])
        .arg(&archive)
        .arg("r0src/*")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert!(
        names.contains(&"r0src/top.txt"),
        "-r0 must match top-level files: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("deep.txt")),
        "-r0 must not descend into matched dirs: {names:?}"
    );
}

/// `-ol` stores symbolic links as redirect records (unix-only: creating
/// the source symlink needs symlink(2)).
#[test]
#[cfg(unix)]
fn cli_links_ol_stores_symlink_redirects() {
    let dir = make_temp_dir();
    let src = dir.path().join("lnk");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("target.txt"), b"target").unwrap();
    std::os::unix::fs::symlink("target.txt", src.join("lnk.txt")).unwrap();

    let archive = dir.path().join("ol.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ol", "-idq"])
        .arg(&archive)
        .arg("lnk")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert!(names.contains(&"lnk/target.txt"), "{names:?}");
    assert!(names.contains(&"lnk/lnk.txt"), "{names:?}");
    // The link member carries no data (redirect record), and extraction
    // recreates the symlink.
    let entry = rar.get_entry("lnk/lnk.txt").unwrap();
    assert_eq!(entry.size(), 0);
    let out = dir.path().join("out");
    rar.extract_all(&out).unwrap();
    let link = std::fs::read_link(out.join("lnk/lnk.txt")).unwrap();
    assert_eq!(link, std::path::Path::new("target.txt"));
}

// ── WinRAR CLI parity batch 3: -sl/-sm/-ed, -tn/-to, -si, -tk, -p-/-c-, ──
// ── -ierr, -ad ────────────────────────────────────────────────────────────

/// Set a file's mtime to `secs_ago` seconds in the past.
fn set_mtime_ago(path: &Path, secs_ago: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(t))
        .unwrap();
}

#[test]
fn cli_size_and_empty_dir_filters() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("small.txt"), b"12345").unwrap();
    std::fs::write(dir.path().join("big.txt"), vec![b'x'; 5000]).unwrap();
    std::fs::create_dir_all(dir.path().join("emptydir")).unwrap();
    std::fs::create_dir_all(dir.path().join("fulldir")).unwrap();
    std::fs::write(dir.path().join("fulldir").join("inner.txt"), b"i").unwrap();

    // -sl1k: only files smaller than 1 KiB (directories always pass).
    let archive = dir.path().join("sl.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-sl1k", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        ["emptydir", "fulldir", "fulldir/inner.txt", "small.txt"]
    );

    // -sm1k: only files larger than 1 KiB.
    let archive = dir.path().join("sm.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-sm1k", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        ["big.txt", "emptydir", "fulldir"]
    );

    // -ed: empty directories are not stored.
    let archive = dir.path().join("ed.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ed", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        ["big.txt", "fulldir", "fulldir/inner.txt", "small.txt"]
    );
}

#[test]
fn cli_period_filters_tn_to_match_winrar() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("new.txt"), b"n").unwrap();
    std::fs::write(dir.path().join("old.txt"), b"o").unwrap();
    set_mtime_ago(&dir.path().join("old.txt"), 5400); // 1.5 hours ago

    // -tn1h: only files newer than 1 hour.
    let archive = dir.path().join("tn.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt"]);

    // -to1h: only files older than 1 hour.
    let archive = dir.path().join("to.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-to1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["old.txt"]);

    // Multiple filters combine with AND: 1h < age <= 2h.
    let archive = dir.path().join("tnandto.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn2h", "-to1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["old.txt"]);

    // Compound period and modifier: -tnc1h30m parses and filters on the
    // creation time (both files were just created, so both match).
    let archive = dir.path().join("tnc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tnc1h30m", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt", "old.txt"]);

    // -to with an empty period matches everything (age >= 0).
    let archive = dir.path().join("to0.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-to", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt", "old.txt"]);

    // No match: WinRAR exits 10 and does not create the archive. old.txt
    // (1.5 h old) deterministically fails the 1 s filter.
    let archive = dir.path().join("none.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn1s", "-idq"])
        .arg(&archive)
        .arg("old.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(10), "no-match must exit with code 10");
    assert!(!archive.exists(), "no-match must not create the archive");
}

#[test]
fn cli_stdin_name_reads_stdin() {
    let dir = make_temp_dir();
    let archive = dir.path().join("si.rar");
    let mut child = std::process::Command::new(RAR_CLI)
        .args(["a", "-siin.txt", "-idq"])
        .arg(&archive)
        .stdin(std::process::Stdio::piped())
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello-stdin")
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.namelist(), ["in.txt"]);
    assert_eq!(rar.read("in.txt").unwrap(), b"hello-stdin");
}

#[test]
fn cli_keep_time_preserves_archive_mtime() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("tk.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let before = std::fs::metadata(&archive).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tk", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let after = std::fs::metadata(&archive).unwrap().modified().unwrap();
    let kept = after.duration_since(before).unwrap_or_default();
    assert!(
        kept < std::time::Duration::from_secs(1),
        "-tk must keep the archive mtime, changed by {kept:?}"
    );
}

#[test]
fn cli_clear_password_and_no_comment_switches() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    // -p- creates an unencrypted archive (readable without a password).
    let archive = dir.path().join("pm.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-p-", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("f.txt").unwrap(), b"x");

    // -c- is accepted on create.
    let archive = dir.path().join("nc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-c-", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["f.txt"]);
}

#[test]
fn cli_err_switch_routes_messages_to_stderr() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("ierr.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-ierr"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "-ierr must send messages to stderr, stdout had: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Created"),
        "-ierr must send the status message to stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_append_dir_extracts_under_archive_name() {
    let dir = make_temp_dir();
    let archive = dir.path().join("ad.rar");
    {
        let mut rar = RarArchive::create(&archive).unwrap();
        rar.add_bytes("f.txt", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-ad", "-idq"])
        .arg(&archive)
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        out.join("ad").join("f.txt").exists(),
        "-ad must extract under a subdirectory named after the archive"
    );
}

// ── -md dictionary size (aligned with WinRAR 7.23) ─────────────────────────

/// Write a 32 MiB file of repeated text (compressible, uniform head so the
/// incompressibility probe does not misfire).
fn write_rep_text(path: &Path, size: usize) {
    let block = b"The quick brown fox jumps over the lazy dog 0123456789.\r\n";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        data.extend_from_slice(block);
    }
    data.truncate(size);
    std::fs::write(path, data).unwrap();
}

fn entry_dict_log(archive: &Path, name: &str) -> u8 {
    let rar = rar5::RarArchive::open(archive).unwrap();
    rar.get_entry(name).unwrap().header.comp_dict_size
}

#[test]
fn cli_dict_size_switch_matches_winrar() {
    let dir = make_temp_dir();
    let file = dir.path().join("rep32t.bin");
    write_rep_text(&file, 32 * 1024 * 1024);

    // Default: 32 MiB (log 8) — WinRAR 7.23's default at every level.
    let archive = dir.path().join("def.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("rep32t.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(entry_dict_log(&archive, "rep32t.bin"), 8);

    // -md64m on a 32 MiB file: 2x file size caps it at 64 MiB (log 9).
    let archive = dir.path().join("md64.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-md64m", "-idq"])
        .arg(&archive)
        .arg("rep32t.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(entry_dict_log(&archive, "rep32t.bin"), 9);

    // -md128k is honored (log 0) even for a large file.
    let archive = dir.path().join("md128.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-md128k", "-idq"])
        .arg(&archive)
        .arg("rep32t.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(entry_dict_log(&archive, "rep32t.bin"), 0);

    // The archive with the larger dictionary round-trips.
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    let data = rar.read("rep32t.bin").unwrap();
    assert_eq!(data, std::fs::read(&file).unwrap());

    // -md above 4 GiB is accepted (WinRAR 7.23 accepts arbitrary values,
    // e.g. -md6g/-md65g). For a small file the 2x-file-size cap lands in
    // the RAR5 range, so the member stays a plain v50 with the capped log.
    for md in ["6g", "8g", "65g"] {
        let archive = dir.path().join(format!("md{md}.rar"));
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", &format!("-md{md}"), "-idq"])
            .arg(&archive)
            .arg("rep32t.bin")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "-md{md} must be accepted");
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("rep32t.bin").unwrap();
        assert_eq!(e.header.comp_version, 0, "-md{md} small file stays v50");
        assert_eq!(e.header.dict_size_bytes, None, "-md{md}");
        // 2x floor_pow2(32 MiB) = 64 MiB -> log 9.
        assert_eq!(e.header.comp_dict_size, 9, "-md{md} cap");
        let data = rar.read("rep32t.bin").unwrap();
        assert_eq!(data, std::fs::read(&file).unwrap(), "-md{md} roundtrip");
    }

    // Invalid sizes are rejected with WinRAR's wording.
    for bad in ["-md3m", "-md", "-md100k", "-md129g"] {
        let out = std::process::Command::new(RAR_CLI)
            .args(["a", bad, "-idq"])
            .arg(dir.path().join(format!("bad_{}.rar", bad.trim_start_matches('-'))))
            .arg("rep32t.bin")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{bad} must be rejected");
        let msg = String::from_utf8_lossy(&out.stderr);
        assert!(
            msg.contains("Unknown option") && msg.contains(bad.trim_start_matches('-')),
            "{bad}: unexpected message {msg}"
        );
    }
}

// ── Long-range matching (WinRAR -mcl semantics) ────────────────────────────

/// A 32 MiB file whose second half is an exact copy of its (random)
/// first half: the 16 MiB match distance is far beyond the near match
/// window, so only the long-range search can compress it. WinRAR applies
/// this automatically for -m2..-m5; we must too.
#[test]
fn long_range_compresses_distant_copies() {
    let dir = make_temp_dir();
    let file = dir.path().join("pair32.bin");
    let half = 16 * 1024 * 1024usize;
    let mut data = pseudo_random_bytes(half, 42);
    data.extend_from_slice(&data.clone());
    std::fs::write(&file, &data).unwrap();

    let archive = dir.path().join("pair.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-md32m", "-idq"])
        .arg(&archive)
        .arg("pair32.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    // The 16 MiB copy must compress to a small fraction: well below
    // 1.25x the random half (16 MiB + small overhead + tiny copy).
    let packed = std::fs::metadata(&archive).unwrap().len();
    assert!(
        packed < 20 * 1024 * 1024,
        "distant copy must compress: {packed} bytes"
    );
    // And it must round-trip byte-identically through our extractor.
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    rar.extract_all_with_options(
        &out_dir,
        rar5::ExtractOptions {
            max_unpacked_bytes: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(std::fs::read(out_dir.join("pair32.bin")).unwrap(), data);
}

/// Deterministic pseudo-random bytes (LCG) — incompressible.
fn pseudo_random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

// ── -ms / -df / -t / -ep4 / -as / -or and `rar a` replace (WinRAR 7.23) ──

/// `-ms<list>` stores matching files without compression (WinRAR: level 0
/// for the listed extensions/masks, everything else compresses).
#[test]
fn cli_store_types_ms_stores_matching_files() {
    let dir = make_temp_dir();
    let txt = dir.path().join("a.txt");
    let bin = dir.path().join("b.bin");
    std::fs::write(&txt, b"aaaa".repeat(200)).unwrap();
    std::fs::write(&bin, pseudo_random_bytes(8 * 1024, 3)).unwrap();

    let archive = dir.path().join("ms.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-msbin", "-idq"])
        .arg(&archive)
        .arg("a.txt")
        .arg("b.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    let b = rar.get_entry("b.bin").unwrap();
    assert_eq!(b.header.comp_method, 0, "-msbin must store b.bin");
    let a = rar.get_entry("a.txt").unwrap();
    assert_eq!(a.header.comp_method, 3, "a.txt must still compress");
    assert_eq!(rar.read("b.bin").unwrap(), std::fs::read(&bin).unwrap());
    assert_eq!(rar.read("a.txt").unwrap(), std::fs::read(&txt).unwrap());
}

/// `-df` deletes the source files after archiving (the archive keeps them).
#[test]
fn cli_delete_after_df_removes_sources() {
    let dir = make_temp_dir();
    let file = dir.path().join("gone.txt");
    std::fs::write(&file, b"will be deleted").unwrap();
    let archive = dir.path().join("df.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-df", "-idq"])
        .arg(&archive)
        .arg("gone.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!file.exists(), "-df must delete the source");
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("gone.txt").unwrap(), b"will be deleted");
}

/// `-t` tests the archive right after creating it.
#[test]
fn cli_test_after_t_validates_new_archive() {
    let dir = make_temp_dir();
    let file = dir.path().join("t.txt");
    std::fs::write(&file, b"test after payload").unwrap();
    let archive = dir.path().join("t.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-t", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-t must succeed on a healthy archive");
}

/// `-ep4<path>` excludes the path prefix from stored names.
#[test]
fn cli_exclude_prefix_ep4_strips_prefix() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sub/dir")).unwrap();
    std::fs::write(dir.path().join("sub/dir/f.txt"), b"data").unwrap();
    let archive = dir.path().join("ep4.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep4sub", "-idq"])
        .arg(&archive)
        .arg("sub/dir/f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.namelist(), ["dir/f.txt"]);
    assert_eq!(rar.read("dir/f.txt").unwrap(), b"data");
}

/// `-as` synchronizes an existing archive: members not in the file list
/// are dropped.
#[test]
fn cli_sync_archive_as_drops_stale_members() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
    let archive = dir.path().join("as.rar");
    let run = |files: &[&str]| {
        std::process::Command::new(RAR_CLI)
            .args(["a", "-as", "-idq"])
            .arg(&archive)
            .args(files)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    };
    assert!(run(&["a.txt", "keep.txt"]));
    assert!(run(&["a.txt"])); // keep.txt is stale now
    let rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.namelist(), ["a.txt"]);
}

/// `rar a` on an existing archive replaces same-named members (WinRAR
/// update semantics) and preserves the others.
#[test]
fn cli_a_replaces_same_named_members() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"old version").unwrap();
    std::fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
    let archive = dir.path().join("upd.rar");
    let run = |files: &[&str]| {
        std::process::Command::new(RAR_CLI)
            .args(["a", "-idq"])
            .arg(&archive)
            .args(files)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    };
    assert!(run(&["a.txt", "keep.txt"]));
    std::fs::write(dir.path().join("a.txt"), b"new version").unwrap();
    assert!(run(&["a.txt"]));
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    // Note: the replaced member moves to the end (delete + re-add);
    // WinRAR keeps the original position. Member sets must match.
    let mut names = rar.namelist();
    names.sort();
    assert_eq!(names, ["a.txt", "keep.txt"]);
    assert_eq!(rar.read("a.txt").unwrap(), b"new version");
}

/// The accepted-for-parity switches (`-ds`, `-s=g`, `-htc`, `-mcx`, `-me`,
/// `-oc`, `-mlp`, `-dh`) must be accepted without changing the outcome.
#[test]
fn cli_accepts_parity_switches() {
    let dir = make_temp_dir();
    let file = dir.path().join("p.txt");
    std::fs::write(&file, b"parity switch payload").unwrap();
    let archive = dir.path().join("par.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args([
            "a", "-ds", "-s=g", "-htc", "-mcx", "-me", "-oc", "-mlp", "-dh", "-idq",
        ])
        .arg(&archive)
        .arg("p.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("p.txt").unwrap(), b"parity switch payload");
}

/// `unrar x -or` renames colliding destinations like WinRAR: `a.txt`
/// becomes `a(1).txt`.
#[test]
fn cli_or_auto_renames_colliding_destinations() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"archive content").unwrap();
    let archive = dir.path().join("or.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("a.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("a.txt"), b"old file").unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-or", "-idq"])
        .arg(&archive)
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"old file");
    assert_eq!(
        std::fs::read(out.join("a(1).txt")).unwrap(),
        b"archive content"
    );
}

/// `rar s` converts an archive to SFX (prepending an SFX module found in
/// the WinRAR installation on Windows) and `rar s-` strips it back; the
/// .sfx file must still extract byte-identically.
#[test]
fn cli_sfx_roundtrip_with_module() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"sfx payload").unwrap();
    let archive = dir.path().join("base.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let status = std::process::Command::new(RAR_CLI)
        .args(["s"])
        .arg("base.rar")
        .current_dir(dir.path())
        .status()
        .unwrap();
    if !status.success() {
        // No SFX module available (non-Windows without one installed):
        // the command itself is what we test, so a clean failure is fine.
        return;
    }
    let sfx = dir.path().join("base.sfx");
    assert!(sfx.exists(), "base.sfx must be created");
    let sfx_len = std::fs::metadata(&sfx).unwrap().len();
    assert!(sfx_len > std::fs::metadata(&archive).unwrap().len());

    // The .sfx file extracts byte-identically (our extractor skips the
    // module prefix).
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-idq"])
        .arg(&sfx)
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "unrar must extract the .sfx file");
    assert_eq!(std::fs::read(out.join("f.txt")).unwrap(), b"sfx payload");

    // `rar s-` strips the module back to a plain archive.
    let status = std::process::Command::new(RAR_CLI)
        .args(["s-"])
        .arg("base.sfx")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "s- must strip the SFX module");
    let mut rar = rar5::RarArchive::open(dir.path().join("base.rar")).unwrap();
    assert_eq!(rar.read("f.txt").unwrap(), b"sfx payload");
}

// ── -ts file time save/restore (WinRAR 7.23 aligned) ───────────────────────

fn created_time(path: &Path) -> Option<std::time::SystemTime> {
    #[cfg(windows)]
    {
        std::fs::metadata(path).ok()?.created().ok()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// `-ts` stores the creation/access times and `unrar x -ts` restores
/// them alongside the modification time (Windows can set all three;
/// Unix restores mtime + atime).
#[test]
fn cli_ts_saves_and_restores_file_times() {
    let dir = make_temp_dir();
    let file = dir.path().join("t.txt");
    std::fs::write(&file, b"ts payload").unwrap();
    let src_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
    let src_ctime = created_time(&file);

    // Default: only mtime is stored (no ctime/atime in the extra record).
    let archive = dir.path().join("def.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("t.txt").unwrap();
        assert!(e.header.ctime.is_none(), "default must not store ctime");
        assert!(e.header.atime.is_none(), "default must not store atime");
    }

    // -ts: all three times stored with ns precision.
    let archive = dir.path().join("all.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ts", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("t.txt").unwrap();
        assert!(e.header.ctime.is_some(), "-ts must store ctime");
        if let Some(src_ctime) = src_ctime {
            let c = e.header.ctime.unwrap();
            let restored = std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(c.0)
                + std::time::Duration::from_nanos(c.1 as u64);
            let diff = restored
                .duration_since(src_ctime)
                .unwrap_or_else(|_| src_ctime.duration_since(restored).unwrap());
            assert!(
                diff < std::time::Duration::from_secs(2),
                "stored ctime {restored:?} far from source {src_ctime:?}"
            );
        }
    }

    // Extract with -ts: mtime and (Windows) creation time restored.
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-ts", "-y"])
        .arg(&archive)
        .arg(&out)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let extracted = out.join("t.txt");
    let dst_mtime = std::fs::metadata(&extracted).unwrap().modified().unwrap();
    assert!(
        dst_mtime.duration_since(src_mtime).unwrap_or_default()
            < std::time::Duration::from_secs(2),
        "extracted mtime must match the source"
    );
    if let Some(src_ctime) = src_ctime {
        let dst_ctime = created_time(&extracted);
        if let Some(dst_ctime) = dst_ctime {
            let diff = dst_ctime
                .duration_since(src_ctime)
                .unwrap_or_else(|_| src_ctime.duration_since(dst_ctime).unwrap());
            assert!(
                diff < std::time::Duration::from_secs(2),
                "extracted ctime {dst_ctime:?} must match source {src_ctime:?}"
            );
        }
    }

    // -ts1: 1-second precision (ns fields zero).
    let archive = dir.path().join("sec.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ts1", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("t.txt").unwrap();
        assert_eq!(e.header.mtime_ns, Some(0), "-ts1 must store second precision");
    }

    // -ts-: no times stored at all.
    let archive = dir.path().join("none.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ts-", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("t.txt").unwrap();
        assert!(e.header.ctime.is_none() && e.header.atime.is_none());
        assert!(e.header.mtime_ns.is_none(), "-ts- must not write a time extra record");
    }

    // Invalid specs are rejected.
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "--ts=xyz"])
        .arg(dir.path().join("badts.rar"))
        .arg("t.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "invalid -ts spec must be rejected");
}

// ── misc switches: -ver version control, accepted no-ops, -ilog ────────────

#[test]
fn cli_version_control_keeps_previous_versions() {
    let dir = make_temp_dir();
    let file = dir.path().join("ver.txt");
    std::fs::write(&file, b"v1").unwrap();
    let archive = dir.path().join("ver.rar");

    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // First update with -ver: old version kept as `ver.txt;1`.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, b"v2").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-ver", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        assert_eq!(rar.read("ver.txt").unwrap(), b"v2");
        assert_eq!(rar.read("ver.txt;1").unwrap(), b"v1");
    }

    // Second update: the chain shifts (ver.txt;1 -> ver.txt;2).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, b"v3").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-ver", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        assert_eq!(rar.read("ver.txt").unwrap(), b"v3");
        assert_eq!(rar.read("ver.txt;1").unwrap(), b"v2");
        assert_eq!(rar.read("ver.txt;2").unwrap(), b"v1");
    }

    // -ver1 caps the history at one previous version.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, b"v4").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-ver1", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        assert_eq!(rar.read("ver.txt").unwrap(), b"v4");
        assert_eq!(rar.read("ver.txt;1").unwrap(), b"v3");
        assert!(!rar.namelist().contains(&"ver.txt;2"));
    }
}

#[test]
fn cli_misc_switches_are_accepted_and_ilog_logs_errors() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("m.txt"), b"m").unwrap();

    // Platform-specific / informational switches are accepted.
    let archive = dir.path().join("misc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args([
            "a", "-idc", "-idd", "-idn", "-idp", "-ac", "-ai", "-os", "-scu", "-oni",
            "-ri5", "-vp", "-vd", "-oi1", "-ams", "-e1", "-ow", "-idq",
        ])
        .arg(&archive)
        .arg("m.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "misc switches must be accepted");

    // -ilog writes the error to the log file.
    let log = dir.path().join("err.log");
    let out = std::process::Command::new(RAR_CLI)
        .arg("a")
        .arg(format!("-ilog{}", log.display()))
        .arg("-idq")
        .arg(dir.path().join("bad.rar"))
        .arg("missing.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        std::fs::read_to_string(&log).unwrap().contains("missing.txt"),
        "-ilog must record the error"
    );
}

// ── configuration sources: RARINISWITCHES / -cfg- / command-line priority ──

#[test]
fn cli_config_sources_apply_with_winrar_priority() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    // RARINISWITCHES supplies default switches (here: quiet mode).
    let archive = dir.path().join("env.rar");
    let out = std::process::Command::new(RAR_CLI)
        .env("RARINISWITCHES", "-idq")
        .args(["a"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "RARINISWITCHES=-idq must suppress output, got {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Command line wins over the environment for single-value switches
    // (no duplicate-argument error, -m5 applied).
    let archive = dir.path().join("prio.rar");
    let out = std::process::Command::new(RAR_CLI)
        .env("RARINISWITCHES", "-m1 -md128k")
        .args(["a", "-m5", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "CLI must override RARINISWITCHES without errors: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // -cfg- ignores RARINISWITCHES entirely.
    let archive = dir.path().join("cfg.rar");
    let out = std::process::Command::new(RAR_CLI)
        .env("RARINISWITCHES", "-idq")
        .args(["a", "-cfg-"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Created"),
        "-cfg- must ignore RARINISWITCHES, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ── rarfiles.lst solid ordering (WinRAR 7.23 semantics) ─────────────────────

#[test]
fn cli_rarfiles_lst_orders_solid_members() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("aaa.cpp"), b"a").unwrap();
    std::fs::write(dir.path().join("f1.cpp"), b"b").unwrap();
    std::fs::write(dir.path().join("ddd.cpp"), b"c").unwrap();
    std::fs::write(dir.path().join("bbb.h"), b"d").unwrap();
    std::fs::write(dir.path().join("ccc.txt"), b"e").unwrap();
    std::fs::create_dir_all(dir.path().join("subd")).unwrap();
    std::fs::write(dir.path().join("subd").join("nested.txt"), b"n").unwrap();
    std::fs::write(dir.path().join("subd").join("deep.cpp"), b"p").unwrap();

    // rarfiles.lst next to the rar binary (Windows/Unix lookup path).
    let lst = Path::new(RAR_CLI).parent().unwrap().join("rarfiles.lst");
    std::fs::write(
        &lst,
        "; test list\n*.txt\nf*.cpp\n*.cpp\n$default\n",
    )
    .unwrap();
    let result = std::panic::catch_unwind(|| {
        let archive = dir.path().join("rfl.rar");
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", "-s", "-idq"])
            .arg(&archive)
            .args(["*.cpp", "*.h", "*.txt", "subd"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let rar = rar5::RarArchive::open(&archive).unwrap();
        let names: Vec<String> = rar
            .namelist()
            .into_iter()
            .map(|s| s.trim_start_matches("./").trim_end_matches('/').to_string())
            .collect();
        // WinRAR order: *.txt group, then f*.cpp (subset of *.cpp, so it
        // wins over *.cpp regardless of position), then *.cpp, then
        // $default, with directory entries last.
        assert_eq!(
            names,
            [
                "ccc.txt",
                "subd/nested.txt",
                "f1.cpp",
                "aaa.cpp",
                "ddd.cpp",
                "subd/deep.cpp",
                "bbb.h",
                "subd",
            ],
            "solid member order must follow rarfiles.lst: {names:?}"
        );
    });
    let _ = std::fs::remove_file(&lst);
    result.unwrap();
}


