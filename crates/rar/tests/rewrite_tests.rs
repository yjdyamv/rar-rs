#![allow(deprecated)] // legacy constructor family; use create_with_options
//! Surgical rewrite commands: delete, append, lock, recovery-record addition, rename, repair and comment edits.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar5::RarArchive;

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

/// WinRAR zero-pads volume part numbers to the digit count of the total
/// volume count (part01..part15). The writer now emits the same padding
/// for sets of 10+ volumes, and discovery, `.rev` naming and rebuild
/// must handle it.
#[test]
fn zero_padded_volume_sets_discover_and_rebuild() {
    let dir = make_temp_dir();
    let path = dir.path().join("pad.rar");
    let payload: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar =
            rar5::RarArchive::create_multivolume_with_recovery_count(&path, 20_000, 3).unwrap();
        rar.add_bytes("a.bin", &payload, 0).unwrap();
        rar.close().unwrap();
    }
    let parent = dir.path();

    // The writer must name the 20-volume set with WinRAR's two-digit
    // padding, and the .rev files must follow the same padding.
    assert!(
        parent.join("pad.part01.rar").exists(),
        "writer must zero-pad volumes of a 10+ volume set"
    );
    assert!(
        !parent.join("pad.part1.rar").exists(),
        "no unpadded volume name may be emitted"
    );
    assert!(
        parent.join("pad.part01.rev").exists(),
        ".rev names must follow the set's zero-padding"
    );
    let volumes = rar5::discover_volumes(&parent.join("pad.part01.rar"));
    assert!(volumes.len() >= 10, "precondition: >= 10 volumes");
    assert_eq!(
        volumes[0].file_name().unwrap().to_string_lossy(),
        "pad.part01.rar",
        "discovery must find the padded first volume"
    );

    // Discovery must find the padded set from the first volume.
    let padded_first = parent.join("pad.part01.rar");
    let discovered = rar5::discover_volumes(&padded_first);
    assert_eq!(
        discovered.len(),
        volumes.len(),
        "padded set must be fully discovered"
    );

    // Delete a padded middle volume and rebuild it from the padded .rev.
    let victim = parent.join("pad.part07.rar");
    std::fs::remove_file(&victim).unwrap();
    let rebuilt = rar5::rebuild_missing_volumes(&padded_first).unwrap();
    assert!(rebuilt.contains(&victim), "padded middle volume rebuilt");
    let mut rar = rar5::RarArchive::open(&padded_first).unwrap();
    assert_eq!(rar.read("a.bin").unwrap(), payload);
}

/// Streaming path repair (`repair_archive_path`) must agree with the
/// in-memory repair byte-for-byte and stream without materializing the
/// whole archive.
#[test]
fn repair_archive_path_streams_and_matches_in_memory() {
    let dir = make_temp_dir();
    let path = dir.path().join("rep-path.rar");
    // Large member so the recovery record sits at the end and damage
    // below lands inside the protected data.
    let payload: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar = rar5::RarArchive::create_with_recovery(&path, 10).unwrap();
        rar.add_bytes("a.bin", &payload, 0).unwrap();
        rar.close().unwrap();
    }
    let good = std::fs::read(&path).unwrap();

    // Damage several bytes inside the protected data.
    let mut damaged = good.clone();
    for pos in [400usize, 410, 420, 900] {
        damaged[pos] ^= 0x5A;
    }
    std::fs::write(&path, &damaged).unwrap();

    let out = dir.path().join("fixed.rar");
    let repaired = rar5::repair_archive_path(&path, &out).unwrap();
    assert!(repaired, "damage must be reported as repaired");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        good,
        "streaming repair must restore the original bytes"
    );

    // Undamaged archive -> no output written (like `rar r`'s "All OK"),
    // reported as not repaired.
    let out2 = dir.path().join("fixed2.rar");
    std::fs::write(&path, &good).unwrap();
    let repaired = rar5::repair_archive_path(&path, &out2).unwrap();
    assert!(!repaired, "intact archive must report no repair");
    assert!(
        !out2.exists(),
        "intact repair must not write an output file"
    );

    // The repaired archive must open and extract byte-identically.
    let mut rar = rar5::RarArchive::open(&out).unwrap();
    assert_eq!(rar.read("a.bin").unwrap(), payload);

    // No recovery record -> clean error, and the output stays absent.
    let plain = dir.path().join("plain.rar");
    {
        let mut rar = rar5::RarArchive::create(&plain).unwrap();
        rar.add_bytes("a.bin", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    let out3 = dir.path().join("fixed3.rar");
    assert!(rar5::repair_archive_path(&plain, &out3).is_err());
    assert!(!out3.exists());
}

/// `repair_archive_path_with` reports non-decreasing progress reaching
/// `(total, total)` and honours the cancellation flag (no partial output
/// left behind).
#[test]
fn repair_archive_path_with_reports_progress_and_cancels() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = make_temp_dir();
    let path = dir.path().join("rep-prog.rar");
    let payload: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar = rar5::RarArchive::create_with_recovery(&path, 10).unwrap();
        rar.add_bytes("a.bin", &payload, 0).unwrap();
        rar.close().unwrap();
    }
    let good = std::fs::read(&path).unwrap();
    let mut damaged = good.clone();
    damaged[400] ^= 0x5A;
    damaged[900] ^= 0x5A;
    std::fs::write(&path, &damaged).unwrap();

    // Progress: monotonic, reaches the file size exactly once.
    let out = dir.path().join("fixed-prog.rar");
    let mut reports: Vec<(u64, u64)> = Vec::new();
    let repaired = rar5::repair_archive_path_with(
        &path,
        &out,
        None,
        Some(&mut |done, total| {
            reports.push((done, total));
        }),
    )
    .unwrap();
    assert!(repaired);
    assert_eq!(std::fs::read(&out).unwrap(), good);
    assert!(!reports.is_empty());
    let total = reports[0].1;
    assert_eq!(total, good.len() as u64 * 2, "scan+copy total");
    let mut last = 0u64;
    let mut reached_end = false;
    eprintln!("reports: {:?}", reports);
    for (done, t) in &reports {
        assert_eq!(*t, total);
        assert!(*done >= last, "progress must be non-decreasing");
        last = *done;
        if *done == total {
            reached_end = true;
        }
    }
    assert!(reached_end, "progress must reach the total");

    // Cancellation: flagged before the call -> Cancelled, no output file.
    let cancel = AtomicBool::new(true);
    let out2 = dir.path().join("fixed-cancel.rar");
    let err = rar5::repair_archive_path_with(&path, &out2, Some(&cancel), None).unwrap_err();
    assert!(matches!(err, rar5::RarError::Cancelled));
    assert!(!out2.exists(), "cancelled repair must not leave output");

    // Cancellation set mid-run: a flag armed after the first progress
    // report aborts the streaming scan/copy and still leaves nothing.
    let cancel = AtomicBool::new(false);
    let out3 = dir.path().join("fixed-cancel2.rar");
    let mut seen_progress = false;
    let err = rar5::repair_archive_path_with(
        &path,
        &out3,
        Some(&cancel),
        Some(&mut |done, _| {
            if done > 0 && !seen_progress {
                seen_progress = true;
                cancel.store(true, Ordering::Relaxed);
            }
        }),
    )
    .unwrap_err();
    assert!(matches!(err, rar5::RarError::Cancelled));
    assert!(!out3.exists());
}

/// `rebuild_missing_volumes_with` reports non-decreasing progress and
/// honours cancellation (missing volumes are not written on abort).
#[test]
fn rebuild_missing_volumes_with_reports_progress_and_cancels() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = make_temp_dir();
    let path = dir.path().join("rcv2.rar");
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar =
            rar5::RarArchive::create_multivolume_with_recovery_count(&path, 60_000, 2).unwrap();
        rar.add_bytes("a.bin", &payload, 0).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar5::discover_volumes(&path);
    let victim = volumes[1].clone();
    std::fs::remove_file(&victim).unwrap();

    let mut reports: Vec<(u64, u64)> = Vec::new();
    let rebuilt = rar5::rebuild_missing_volumes_with(
        &volumes[0],
        None,
        Some(&mut |done, total| reports.push((done, total))),
    )
    .unwrap();
    assert!(rebuilt.contains(&victim));
    assert!(!reports.is_empty());
    let total = reports[0].1;
    assert!(total > 0);
    let mut last = 0u64;
    for (done, t) in &reports {
        assert_eq!(*t, total);
        assert!(*done >= last, "progress must be non-decreasing");
        last = *done;
    }
    assert_eq!(last, total, "progress must reach the total");
    std::fs::remove_file(&victim).unwrap();

    // Cancelled up front -> Cancelled, no volume written.
    let cancel = AtomicBool::new(true);
    let err = rar5::rebuild_missing_volumes_with(&volumes[0], Some(&cancel), None).unwrap_err();
    assert!(matches!(err, rar5::RarError::Cancelled));
    assert!(!victim.exists(), "cancelled rebuild must not write volumes");
}
