#![allow(deprecated)] // legacy constructor family; use create_with_options
//! Self roundtrips: create/read/extract across solid, encrypted, multi-volume, batch, streaming and progress paths.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar::RarArchive;
use std::sync::{Arc, Mutex};

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
        let mut rar = rar::RarArchive::create_with_options(
            &path,
            rar::CreateOptions {
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
    let mut rar = rar::RarArchive::open(&path).unwrap();
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
fn blake2_roundtrip_and_tamper_detection() {
    let dir = make_temp_dir();
    let path = dir.path().join("b2.rar");
    let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar = rar::RarArchive::create_with_options(
            &path,
            rar::CreateOptions {
                blake2: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("data.bin", &payload, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = rar::RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("data.bin").unwrap(), payload);

    // Corrupt one payload byte: BLAKE2sp (and CRC) must reject the read.
    let mut bytes = std::fs::read(&path).unwrap();
    let data_off = first_file_data_offset(&bytes);
    bytes[data_off + 10] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();
    let mut rar = rar::RarArchive::open(&path).unwrap();
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
        let mut rar = rar::RarArchive::create_with_password(&path, "secret").unwrap();
        rar.add_bytes("data.bin", &payload, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = rar::RarArchive::open_with_password(&path, "secret").unwrap();
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
    let mut rar = rar::RarArchive::open_with_password(&path, "secret").unwrap();
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
            let mut rar = rar::RarArchive::create(&path).unwrap();
            rar.add_bytes(bad, b"nope", 0).unwrap();
            rar.close().unwrap();
        }
        let mut rar = rar::RarArchive::open(&path).unwrap();
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
        let mut rar = rar::RarArchive::create(&path).unwrap();
        rar.add_bytes("nested/file.txt", b"hello", 0).unwrap();
        rar.close().unwrap();
    }
    let out = dir.path().join("out");
    let mut rar = rar::RarArchive::open(&path).unwrap();
    rar.extract_all_with_options(&out, rar::ExtractOptions::default())
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
        let mut rar = rar::RarArchive::create(&path).unwrap();
        rar.add_bytes("dir/sub/file.txt", b"flat", 0).unwrap();
        rar.add_bytes("top.txt", b"top", 0).unwrap();
        rar.close().unwrap();
    }
    // Flat extraction writes every member under its basename, no tree.
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = rar::RarArchive::open(&path).unwrap();
        rar.extract_all_with_options(
            &out,
            rar::ExtractOptions {
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
        let mut rar = rar::RarArchive::create(&evil).unwrap();
        rar.add_bytes("good.txt", b"ok", 0).unwrap();
        rar.add_bytes("..", b"escape", 0).unwrap();
        rar.close().unwrap();
    }
    let out2 = dir.path().join("out2");
    std::fs::create_dir_all(&out2).unwrap();
    {
        let mut rar = rar::RarArchive::open(&evil).unwrap();
        let err = rar
            .extract_all_with_options(
                &out2,
                rar::ExtractOptions {
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
        let mut rar = rar::RarArchive::create(&path).unwrap();
        rar.add_bytes("f.bin", &payload, 0).unwrap();
        rar.close().unwrap();
    }
    let mut rar = rar::RarArchive::open(&path).unwrap();
    let err = rar
        .read_with_options(
            "f.bin",
            rar::ExtractOptions {
                max_unpacked_bytes: Some(1000),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("limit"), "{err}");

    let out = dir.path().join("out");
    let mut rar = rar::RarArchive::open(&path).unwrap();
    let err = rar
        .extract_all_with_options(
            &out,
            rar::ExtractOptions {
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
        let mut rar = rar::RarArchive::create_with_options(
            &arc,
            rar::CreateOptions {
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
    let volumes = rar::discover_volumes(&arc);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    for vol in &volumes[..volumes.len() - 1] {
        assert_eq!(
            std::fs::metadata(vol).unwrap().len(),
            128 * 1024,
            "non-final volume must be byte-exact"
        );
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let mut rar = rar::RarArchive::open(&volumes[0]).unwrap();
    rar.extract_all(&out).unwrap();
    assert_eq!(std::fs::read(out.join("a.bin")).unwrap(), data_a);
    assert_eq!(std::fs::read(out.join("b.bin")).unwrap(), data_b);
}

#[test]
fn combined_solid_quickopen_blake2_recovery_password_roundtrip() {
    let dir = make_temp_dir();
    let path = dir.path().join("combo.rar");
    let a = b"combined solid content ".repeat(5000);
    let b = b"different solid content ".repeat(4000);
    {
        let mut rar = rar::RarArchive::create_with_options(
            &path,
            rar::CreateOptions {
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
    let mut rar = rar::RarArchive::open_with_password(&path, "pw").unwrap();
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
        let mut rar = rar::RarArchive::create(&path).unwrap();
        rar.add(&src, 5).unwrap(); // incompressible -> streaming STORE
        rar.close().unwrap();
    }
    let mut rar = rar::RarArchive::open(&path).unwrap();
    let data = rar.read("big.bin").unwrap();
    assert_eq!(data.len(), 32 * 1024 * 1024);
    let src_data = std::fs::read(&src).unwrap();
    assert_eq!(data, src_data);

    // Streamed extraction must match too.
    let out = dir.path().join("out");
    let mut rar = rar::RarArchive::open(&path).unwrap();
    let extracted = rar.extract("big.bin", &out).unwrap();
    assert_eq!(std::fs::read(extracted).unwrap(), src_data);
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
        rar.add_batch(&[rar::BatchEntry::Bytes {
            name: "data.bin",
            data: &payload,
            level: 5,
        }])
        .expect("add");
        rar.close().expect("close");
    }

    let events: Vec<(u64, u64)> = events.lock().expect("lock").iter().copied().collect();

    assert!(!events.is_empty(), "no progress events emitted");
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
    assert_eq!(
        deltas + events[0].0,
        payload.len() as u64,
        "deltas must sum exactly once"
    );
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
    let expected_total: u64 = 512 * 1024 + bytes.len() as u64 + 68 * 1024 * 1024;

    let events: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let mut rar = RarArchive::create(&path).expect("create");
        let sink = events.clone();
        let cb: Box<dyn FnMut(u64, u64) + Send> = Box::new(move |done, total| {
            sink.lock().expect("lock").push((done, total));
        });
        rar.set_progress_callback(Some(cb));
        rar.add_batch(&[
            rar::BatchEntry::File {
                path: &small,
                name: Some("small.bin"),
                level: 5,
            },
            rar::BatchEntry::Bytes {
                name: "bytes.bin",
                data: &bytes,
                level: 5,
            },
            rar::BatchEntry::File {
                path: &big,
                name: Some("big.bin"),
                level: 5,
            },
        ])
        .expect("add batch");
        rar.close().expect("close");
    }

    let events: Vec<(u64, u64)> = events.lock().expect("lock").iter().copied().collect();

    assert!(!events.is_empty(), "no progress events emitted");
    for w in events.windows(2) {
        assert!(w[0].0 <= w[1].0, "progress went backwards");
        assert_eq!(w[0].1, w[1].1, "total changed mid-stream");
    }
    for (done, total) in &events {
        assert!(*done <= *total, "done {done} exceeded total {total}");
    }
    let (last_done, last_total) = *events.last().expect("events");
    assert_eq!(last_done, last_total);
    assert_eq!(
        last_total, expected_total,
        "total must cover the whole batch"
    );
    let deltas: u64 = events.windows(2).map(|w| w[1].0 - w[0].0).sum();
    assert_eq!(
        deltas + events[0].0,
        expected_total,
        "deltas must sum exactly once across the whole batch"
    );
}

#[test]
fn lz_tail_match_fixture_roundtrips_without_panic() {
    // Regression for the 3-byte cache prefilter out-of-bounds read added in
    // 341bd79: this 362-byte fixture ends with two bytes that match an
    // earlier position at a cached distance, which used to index past the
    // end of the buffer at `pos = size - 2` and abort the process.
    let data = include_bytes!("fixtures/rar50/tail-match-362.bin");
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

    let entries: Vec<rar::BatchEntry<'_>> = vec![
        rar::BatchEntry::Directory {
            path: &src_dir,
            name: Some("folder"),
        },
        rar::BatchEntry::File {
            path: &small,
            name: None,
            level: 3,
        },
        rar::BatchEntry::File {
            path: &big,
            name: Some("renamed.bin"),
            level: 3,
        },
        rar::BatchEntry::File {
            path: &small,
            name: Some("copy.bin"),
            level: 1,
        },
    ];

    let seq_path = dir.path().join("seq.rar");
    {
        let mut ar = rar::RarArchive::create(&seq_path).unwrap();
        ar.add_directory_only(&src_dir, "folder").unwrap();
        ar.add(&small, 3).unwrap();
        ar.add_as(&big, "renamed.bin", 3).unwrap();
        ar.add_as(&small, "copy.bin", 1).unwrap();
        ar.close().unwrap();
    }
    let batch_path = dir.path().join("batch.rar");
    {
        let mut ar = rar::RarArchive::create(&batch_path).unwrap();
        ar.add_batch(&entries).unwrap();
        ar.close().unwrap();
    }

    assert_eq!(
        std::fs::read(&seq_path).unwrap(),
        std::fs::read(&batch_path).unwrap(),
        "batch archive differs from sequential archive"
    );

    let mut ar = rar::RarArchive::open(&batch_path).unwrap();
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
    let entries: Vec<rar::BatchEntry<'_>> = vec![
        rar::BatchEntry::Bytes {
            name: "a.bin",
            data: &a,
            level: 3,
        },
        rar::BatchEntry::Bytes {
            name: "b.bin",
            data: &b,
            level: 5,
        },
    ];
    {
        let mut ar = rar::RarArchive::create_with_password(&path, "pw").unwrap();
        ar.add_batch(&entries).unwrap();
        ar.close().unwrap();
    }
    let mut ar = rar::RarArchive::open_with_password(&path, "pw").unwrap();
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
    let entries: Vec<rar::BatchEntry<'_>> = vec![
        rar::BatchEntry::Bytes {
            name: "before.bin",
            data: &small,
            level: 3,
        },
        rar::BatchEntry::File {
            path: &big,
            name: None,
            level: 3,
        },
        rar::BatchEntry::Bytes {
            name: "after.bin",
            data: &small,
            level: 3,
        },
    ];
    {
        let mut ar = rar::RarArchive::create(&path).unwrap();
        ar.add_batch(&entries).unwrap();
        ar.close().unwrap();
    }
    let mut ar = rar::RarArchive::open(&path).unwrap();
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
        ar.add_batch(&[rar::BatchEntry::File {
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
    let entries: Vec<rar::BatchEntry<'_>> = vec![
        rar::BatchEntry::File {
            path: &a_path,
            name: None,
            level: 3,
        },
        rar::BatchEntry::File {
            path: &b_path,
            name: None,
            level: 3,
        },
    ];

    let seq_path = dir.path().join("seq-solid.rar");
    {
        let mut ar = rar::RarArchive::create_with_options(
            &seq_path,
            rar::CreateOptions {
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
        let mut ar = rar::RarArchive::create_with_options(
            &batch_path,
            rar::CreateOptions {
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
