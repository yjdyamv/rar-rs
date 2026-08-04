//! Integration tests for archive creation, extraction, and WinRAR interop.
//!
//! `tests/fixtures/winrar5_multiple_files.rar` is a RAR5 archive created by
//! WinRAR, vendored from the libarchive test suite
//! (`test_read_format_rar5_multiple_files.rar`, BSD-2-Clause licensed,
//! https://github.com/libarchive/libarchive).

use rar5::RarArchive;

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

fn sha256(data: &[u8]) -> String {
    // sha2 is a dependency of the library; expose it via the already-linked
    // crates. digest 0.11's output no longer formats as hex, so encode it
    // manually.
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(data);
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn make_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
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
    let mut blocks = Vec::new();
    let mut pos = 8; // skip signature
    while pos + 4 < data.len() {
        let hsize_start = pos + 4;
        let (hsize, p) = read_vint(data, hsize_start);
        if hsize == 0 || hsize > data.len() as u64 {
            break;
        }
        let body_end = p + hsize as usize;
        if body_end > data.len() {
            break;
        }
        let body = data[p..body_end].to_vec();
        let (block_type, q) = read_vint(&body, 0);
        let (flags, q) = read_vint(&body, q);
        let mut extra_size = 0u64;
        let mut data_size = 0u64;
        let mut q = q;
        if flags & 0x0001 != 0 {
            let (v, n) = read_vint(&body, q);
            extra_size = v;
            q = n;
        }
        if flags & 0x0002 != 0 {
            let (v, n) = read_vint(&body, q);
            data_size = v;
            q = n;
        }
        let _ = q;
        blocks.push(BlockInfo {
            start: pos,
            header_len: body_end - hsize_start,
            block_type,
            flags,
            extra_size,
            data_size,
            body,
        });
        pos = body_end + data_size as usize;
        if block_type == 0x05 {
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
    let payload_a: Vec<u8> = b"common prefix ".iter().cycle().take(200_000).copied().collect();
    let payload_b: Vec<u8> = b"common prefix ".iter().cycle().take(150_000).copied().collect();
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
    assert_eq!(qo.unwrap(), qo_pos as u64 - 8, "QO offset must be relative to archive start");
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
    assert_eq!(rr.unwrap(), rr_pos as u64 - 8, "RR offset must be relative to archive start");
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
    assert!(rar.read("data.bin").is_err(), "tampered data must fail verification");
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
    assert!(rar.read("data.bin").is_err(), "corrupted encrypted data must fail");
}

#[test]
fn extract_rejects_unsafe_entry_names() {
    let dir = make_temp_dir();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    for bad in ["../evil.txt", "/etc/passwd", "C:/windows/x", "a/../../b"] {
        let path = dir.path().join(format!("evil-{}.rar", bad.replace(['/', ':'], "_")));
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
    assert_eq!(std::fs::read(out.join("nested/file.txt")).unwrap(), b"hello");
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
fn solid_with_multivolume_is_rejected() {
    let err = rar5::RarArchive::create_with_options(
        "solid-vol.rar",
        rar5::CreateOptions {
            solid: true,
            volume_size: Some(1024),
            ..Default::default()
        },
    )
    .err()
    .expect("solid multivolume must be rejected");
    assert!(matches!(err, rar5::RarError::Unsupported(_)), "{err}");
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
        let status = std::process::Command::new(&unrar).arg("t").arg(&rr).status().unwrap();
        assert!(status.success(), "official unrar rejected the recovery archive");

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
        assert!(status.success(), "official rar could not repair our recovery record");

        // WinRAR writes the repaired archive as `fixed.<name>.rar`.
        let fixed = dir
            .path()
            .join(format!("fixed.{}", rr.file_name().unwrap().to_string_lossy()));
        assert!(fixed.exists(), "official rar did not produce {fixed:?}");
        let status = std::process::Command::new(&unrar).arg("t").arg(&fixed).status().unwrap();
        assert!(status.success(), "repaired archive still fails official unrar test");

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
        assert!(status.success(), "reconstructed volume set fails unrar test");
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
    let a: Vec<u8> = b"official rar solid content ".iter().cycle().take(300_000).copied().collect();
    let b: Vec<u8> = b"second file content ".iter().cycle().take(200_000).copied().collect();
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

    let events: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let mut rar = RarArchive::create(&path).expect("create");
        let sink = events.clone();
        let cb: Box<dyn FnMut(u64, u64) + Send> = Box::new(move |done, total| {
            sink.lock().expect("lock").push((done, total));
        });
        rar.set_progress_callback(Some(cb));
        rar.add_bytes("data.bin", &payload, 5).expect("add");
        rar.close().expect("close");
    }

    let events: Vec<(u64, u64)> = events.lock().expect("lock").iter().copied().collect();

    assert!(!events.is_empty(), "no progress events emitted");
    for w in events.windows(2) {
        assert!(w[0].0 <= w[1].0, "progress went backwards");
        assert_eq!(w[0].1, w[1].1, "total changed mid-stream");
    }
    let (last_done, last_total) = *events.last().expect("events");
    assert_eq!(last_done, last_total);
    assert_eq!(last_total, payload.len() as u64);
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
