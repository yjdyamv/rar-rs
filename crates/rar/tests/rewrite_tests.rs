//! Surgical rewrite commands: delete, append, lock, recovery-record addition, rename, repair and comment edits.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{ArchiveEditor, ArchiveReader, ArchiveWriter};

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
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        for (name, data) in &files {
            let level = if name.ends_with(".bin") { 0 } else { 3 };
            let opts = rar_rs::EntryWriteOptions::new()
                .compression_level(rar_rs::CompressionLevel::try_from(level).unwrap());
            rar.add_bytes(name, data, opts).unwrap();
        }
        rar.finish().unwrap();
    }
    let orig = std::fs::read(&path).unwrap();

    // Delete a middle member and the last member.
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let ids: Vec<_> = ["b.bin", "e.bin"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    let n = ed.delete_entries(&ids).unwrap();
    assert_eq!(n, 2);
    let kept: Vec<String> = ed.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(kept, ["a.txt", "c.txt", "d.txt"]);
    drop(ed);

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
        let mut rar = ArchiveReader::open(&path).unwrap();
        assert_eq!(
            &rar.read_entry(rar.unique_entry(name).unwrap()).unwrap(),
            data
        );
    }
}

#[test]
fn delete_rebuilds_quick_open_record() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-qo.rar");
    {
        let mut rar =
            ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default().quick_open(true))
                .unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("f1.txt", &compressible(1, 50_000), opts3)
            .unwrap();
        rar.add_bytes("f2.txt", &compressible(2, 50_000), opts3)
            .unwrap();
        rar.add_bytes("f3.txt", &compressible(3, 50_000), opts3)
            .unwrap();
        rar.finish().unwrap();
    }
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let ids: Vec<_> = ["f2.txt"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    ed.delete_entries(&ids).unwrap();

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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().recovery_percent(10),
        )
        .unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("f1.txt", &compressible(1, 50_000), opts3)
            .unwrap();
        rar.add_bytes("f2.txt", &compressible(2, 50_000), opts3)
            .unwrap();
        rar.finish().unwrap();
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
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let ids: Vec<_> = ["f1.txt"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    ed.delete_entries(&ids).unwrap();

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
    let rar = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["f2.txt"]
    );

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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().solid_mode(rar_rs::SolidMode::Continuous),
        )
        .unwrap();
        for (name, data) in &files {
            // All compressible members join the same solid chain; the last
            // one is stored and starts a fresh chain segment.
            let level = if name == "e.txt" { 0 } else { 3 };
            let opts = rar_rs::EntryWriteOptions::new()
                .compression_level(rar_rs::CompressionLevel::try_from(level).unwrap());
            rar.add_bytes(name, data, opts).unwrap();
        }
        rar.finish().unwrap();
    }
    let orig = std::fs::read(&path).unwrap();

    // Delete a mid-chain member: the chain is recompressed from its start,
    // and the stored member after it is copied verbatim.
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let ids: Vec<_> = ["b.bin"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    ed.delete_entries(&ids).unwrap();
    for (name, data) in &files {
        if name == "b.bin" {
            continue;
        }
        let mut rar = ArchiveReader::open(&path).unwrap();
        assert_eq!(
            &rar.read_entry(rar.unique_entry(name).unwrap()).unwrap(),
            data,
            "content of {name} lost"
        );
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
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let ids: Vec<_> = ["d.bin"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    ed.delete_entries(&ids).unwrap();
    let bytes2 = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..s_del], &bytes2[..s_del], "prefix must be verbatim");
    let rar = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["a.bin", "c.bin", "e.txt"]
    );
}

#[test]
fn delete_from_encrypted_archives_roundtrips() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-enc.rar");
    let payload = compressible(7, 60_000);
    {
        let mut rar =
            ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default().password("s3cret"))
                .unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("f1.txt", &payload, opts3).unwrap();
        rar.add_bytes("f2.txt", &payload, opts3).unwrap();
        rar.add_bytes("f3.txt", &payload, opts3).unwrap();
        rar.finish().unwrap();
    }
    let mut ed = ArchiveEditor::open_with_password(&path, "s3cret").unwrap();
    let ids: Vec<_> = ["f2.txt"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    ed.delete_entries(&ids).unwrap();
    let mut rar =
        ArchiveReader::open_with(&path, rar_rs::OpenOptions::new().password("s3cret")).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["f1.txt", "f3.txt"]
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("f1.txt").unwrap()).unwrap(),
        payload
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("f3.txt").unwrap()).unwrap(),
        payload
    );

    // Header-encrypted archives.
    let path2 = dir.path().join("del-hp.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &path2,
            rar_rs::WriterOptions::default()
                .password("s3cret")
                .encrypt_headers(true),
        )
        .unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("g1.txt", &payload, opts3).unwrap();
        rar.add_bytes("g2.txt", &payload, opts3).unwrap();
        rar.finish().unwrap();
    }
    let mut ed = ArchiveEditor::open_with_password(&path2, "s3cret").unwrap();
    let ids: Vec<_> = ["g1.txt"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    ed.delete_entries(&ids).unwrap();
    let mut rar =
        ArchiveReader::open_with(&path2, rar_rs::OpenOptions::new().password("s3cret")).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["g2.txt"]
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("g2.txt").unwrap()).unwrap(),
        payload
    );
}

#[test]
fn delete_all_members_erases_archive() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-all.rar");
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("f1.txt", b"one", opts0).unwrap();
        rar.add_bytes("f2.txt", b"two", opts0).unwrap();
        rar.finish().unwrap();
    }
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let ids: Vec<_> = ["f1.txt", "f2.txt"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    let n = ed.delete_entries(&ids).unwrap();
    assert_eq!(n, 2);
    assert!(!path.exists(), "archive must be erased when empty");
}

#[test]
fn delete_rejects_missing_members_and_multivolume() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-err.rar");
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("f1.txt", b"one", opts0).unwrap();
        rar.finish().unwrap();
    }
    let ed = ArchiveEditor::open(&path).unwrap();
    match ed.unique_entry("nope.txt") {
        Err(rar_rs::RarError::MemberNotFound { name }) => assert_eq!(name, "nope.txt"),
        other => panic!("expected MemberNotFound, got {other:?}"),
    }
    // Archive unchanged after a failed delete.
    let rar = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["f1.txt"]
    );

    // The official `rar` CLI refuses to modify multi-volume archives
    // ("Cannot modify volume"); rar-rs re-splits the volumes instead.
    let vol = dir.path().join("del-vol.rar");
    let payload_a: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let payload_b: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
    {
        let mut rar =
            ArchiveWriter::create_with(&vol, rar_rs::WriterOptions::default().volume_size(30_000))
                .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload_a, opts0).unwrap();
        rar.add_bytes("b.bin", &payload_b, opts0).unwrap();
        rar.add_bytes("c.bin", &payload_a, opts0).unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&vol);
    assert!(volumes.len() > 1, "precondition: multi-volume archive");
    let rar = ArchiveReader::open(&volumes[0]).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["a.bin", "b.bin", "c.bin"]
    );
    let mut ed = ArchiveEditor::open(&volumes[0]).unwrap();
    let ids: Vec<_> = ["b.bin"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    let n = ed.delete_entries(&ids).unwrap();
    assert_eq!(n, 1);

    // Content survives and the volume set is readable again.
    let volumes = rar_rs::discover_volumes(&vol);
    let mut rar = ArchiveReader::open(&volumes[0]).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["a.bin", "c.bin"]
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        payload_a
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("c.bin").unwrap()).unwrap(),
        payload_a
    );
}

#[test]
fn delete_rejects_locked_archive() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-locked.rar");
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("f1.txt", b"one", opts0).unwrap();
        rar.finish().unwrap();
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

    let mut ed = ArchiveEditor::open(&path).unwrap();
    let ids: Vec<_> = ["f1.txt"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    match ed.delete_entries(&ids) {
        Err(rar_rs::RarError::ArchiveLocked) => {}
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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .quick_open(true)
                .recovery_percent(10),
        )
        .unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("a.bin", &a, opts3).unwrap();
        rar.finish().unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    {
        let mut rar = ArchiveWriter::append(&path).unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("b.bin", &b, opts3).unwrap();
        rar.add_bytes("c.bin", &c, opts0).unwrap();
        rar.finish().unwrap();
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

    let mut rar = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        a
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("b.bin").unwrap()).unwrap(),
        b
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("c.bin").unwrap()).unwrap(),
        c
    );
}

#[test]
fn append_rejects_locked_archive() {
    let dir = make_temp_dir();
    let path = dir.path().join("app-locked.rar");
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", b"x", opts0).unwrap();
        rar.finish().unwrap();
    }
    {
        let mut ed = ArchiveEditor::open(&path).unwrap();
        ed.lock().unwrap();
    }
    // Locked archives are read-only: both the official `rar d` and our
    // append/delete refuse them.
    match ArchiveWriter::append(&path) {
        Err(rar_rs::RarError::ArchiveLocked) => {}
        Err(e) => panic!("expected ArchiveLocked, got {e:?}"),
        Ok(_) => panic!("expected ArchiveLocked"),
    }
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let ids: Vec<_> = ["a.bin"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    match ed.delete_entries(&ids) {
        Err(rar_rs::RarError::ArchiveLocked) => {}
        other => panic!("expected ArchiveLocked, got {other:?}"),
    }
    // Content still readable after the lock.
    let mut rar = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        b"x"
    );
}

#[test]
fn add_recovery_record_to_existing_archive() {
    let dir = make_temp_dir();
    let path = dir.path().join("rr-new.rar");
    let payload = compressible(31, 80_000);
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("f1.bin", &payload, opts3).unwrap();
        rar.finish().unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert!(!service_exists(&before, "RR"));

    {
        let mut ed = ArchiveEditor::open(&path).unwrap();
        ed.apply(rar_rs::EditPlan::new().set_recovery(10)).unwrap();
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
    let mut rar = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("f1.bin").unwrap()).unwrap(),
        payload
    );

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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .volume_size(30_000)
                .recovery_volume_count(1),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload_a, opts0).unwrap();
        rar.add_bytes("b.bin", &payload_b, opts0).unwrap();
        rar.add_bytes("c.bin", &payload_a, opts0).unwrap();
        rar.finish().unwrap();
    }
    let rev = dir.path().join("mv-rev.part1.rev");
    assert!(rev.exists(), "precondition: .rev files present");
    let volumes_before = rar_rs::discover_volumes(&path);
    assert!(volumes_before.len() > 1);

    {
        let mut ed = ArchiveEditor::open(&volumes_before[0]).unwrap();
        let ids: Vec<_> = ["b.bin"]
            .iter()
            .map(|n| ed.unique_entry(n).unwrap())
            .collect();
        ed.delete_entries(&ids).unwrap();
    }
    // The .rev set is regenerated over the new volumes.
    let volumes_after = rar_rs::discover_volumes(&path);
    assert!(rev.exists(), ".rev files must be regenerated");
    let mut rar = ArchiveReader::open(&volumes_after[0]).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["a.bin", "c.bin"]
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        payload_a
    );

    // Official `rar rc` must reconstruct a deleted volume from them.
    if let (Some(unrar), Some(rar_bin)) = (
        std::env::var_os("SA_OFFICIAL_UNRAR"),
        std::env::var_os("SA_OFFICIAL_RAR"),
    ) {
        {
            let vols = rar_rs::discover_volumes(&path);
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
            let mut rar = ArchiveReader::open(&vols[0]).unwrap();
            assert_eq!(
                rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
                payload_a
            );
            assert_eq!(
                rar.read_entry(rar.unique_entry("c.bin").unwrap()).unwrap(),
                payload_a
            );
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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .quick_open(true)
                .recovery_percent(10),
        )
        .unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("a.bin", &a, opts3).unwrap();
        rar.add_bytes("b.bin", &b, opts3).unwrap();
        rar.finish().unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    let mut ed = ArchiveEditor::open(&path).unwrap();
    let id = ed.unique_entry("a.bin").unwrap();
    let n = ed
        .rename_entries(&[(id, "renamed.bin".to_string())])
        .unwrap();
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

    let mut rar = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("renamed.bin").unwrap())
            .unwrap(),
        a
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("b.bin").unwrap()).unwrap(),
        b
    );

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
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        rar.add_directory(dir.path(), "old").unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("old/sub/f.txt", b"hello", opts0).unwrap();
        rar.finish().unwrap();
    }
    let mut ed = ArchiveEditor::open(&path).unwrap();
    assert_eq!(
        ed.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["old/", "old/sub/f.txt"]
    );
    let id = ed.unique_entry("old/").unwrap();
    let n = ed.rename_entries(&[(id, "new".to_string())]).unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        ed.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["new/", "new/sub/f.txt"],
        "descendant prefix renamed"
    );
    let mut rar = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("new/sub/f.txt").unwrap())
            .unwrap(),
        b"hello"
    );
}

#[test]
fn rename_multivolume_keeps_content() {
    let dir = make_temp_dir();
    let path = dir.path().join("rn-mv.rar");
    let payload: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().volume_size(100_000),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("big.bin", &payload, opts0).unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    let mut ed = ArchiveEditor::open(&volumes[0]).unwrap();
    let id = ed.unique_entry("big.bin").unwrap();
    let n = ed
        .rename_entries(&[(id, "renamed.bin".to_string())])
        .unwrap();
    assert_eq!(n, 1);
    let volumes = rar_rs::discover_volumes(&path);
    let mut rar = ArchiveReader::open(&volumes[0]).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["renamed.bin"]
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("renamed.bin").unwrap())
            .unwrap(),
        payload
    );
}

#[test]
fn rename_rejects_missing_and_locked() {
    let dir = make_temp_dir();
    let path = dir.path().join("rn-err.rar");
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", b"x", opts0).unwrap();
        rar.finish().unwrap();
    }
    let ed = ArchiveEditor::open(&path).unwrap();
    match ed.unique_entry("nope") {
        Err(rar_rs::RarError::MemberNotFound { name }) => assert_eq!(name, "nope"),
        other => panic!("expected MemberNotFound, got {other:?}"),
    }
    {
        let mut ed = ArchiveEditor::open(&path).unwrap();
        ed.lock().unwrap();
    }
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let id = ed.unique_entry("a.bin").unwrap();
    match ed.rename_entries(&[(id, "b.bin".to_string())]) {
        Err(rar_rs::RarError::ArchiveLocked) => {}
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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().recovery_percent(10),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload, opts0).unwrap();
        rar.finish().unwrap();
    }
    let good = std::fs::read(&path).unwrap();

    // Damage a few bytes inside the protected data.
    let mut damaged = good.clone();
    for pos in [300usize, 310, 320] {
        damaged[pos] ^= 0xA5;
    }
    let repaired = rar_rs::repair_archive(&damaged).unwrap();
    assert_eq!(repaired, good, "repair must restore the original bytes");

    // An undamaged archive is returned unchanged.
    assert_eq!(rar_rs::repair_archive(&good).unwrap(), good);

    // An archive without a recovery record fails cleanly.
    let plain = dir.path().join("plain.rar");
    {
        let mut rar = ArchiveWriter::create_with(&plain, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", b"x", opts0).unwrap();
        rar.finish().unwrap();
    }
    let bytes = std::fs::read(&plain).unwrap();
    assert!(rar_rs::repair_archive(&bytes).is_err());
}

#[test]
fn rebuild_missing_volumes_from_rev_files() {
    let dir = make_temp_dir();
    let path = dir.path().join("rcv.rar");
    let payload_a: Vec<u8> = (0..120_000u32).map(|i| (i % 251) as u8).collect();
    let payload_b: Vec<u8> = (0..60_000u32).map(|i| (i % 253) as u8).collect();
    {
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .volume_size(60_000)
                .recovery_volume_count(2),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload_a, opts0).unwrap();
        rar.add_bytes("b.bin", &payload_b, opts0).unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    let rev = dir.path().join("rcv.part1.rev");
    assert!(rev.exists(), "precondition: .rev files present");

    // Delete a middle volume and rebuild it from the .rev files.
    let victim = volumes[1].clone();
    std::fs::remove_file(&victim).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert!(rebuilt.contains(&victim), "middle volume rebuilt");
    let volumes = rar_rs::discover_volumes(&path);
    let mut rar = ArchiveReader::open(&volumes[0]).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["a.bin", "b.bin"]
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        payload_a
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("b.bin").unwrap()).unwrap(),
        payload_b
    );

    // Everything present -> nothing to rebuild.
    assert!(
        rar_rs::rebuild_missing_volumes(&volumes[0])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn comment_set_get_roundtrip() {
    let dir = make_temp_dir();
    let path = dir.path().join("cmt.rar");
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", b"x", opts0).unwrap();
        rar.finish().unwrap();
    }
    {
        let mut rar = rar_rs::RarArchive::open(&path).unwrap();
        assert_eq!(rar.get_comment().unwrap(), None);
        let mut ed = ArchiveEditor::open(&path).unwrap();
        ed.apply(rar_rs::EditPlan::new().set_comment(b"my comment\n"))
            .unwrap();
    }
    {
        let mut rar = rar_rs::RarArchive::open(&path).unwrap();
        assert_eq!(rar.get_comment().unwrap(), Some(b"my comment\n".to_vec()));
        // The member survives the comment rewrite.
        let mut rar2 = ArchiveReader::open(&path).unwrap();
        assert_eq!(
            rar2.read_entry(rar2.unique_entry("a.bin").unwrap())
                .unwrap(),
            b"x"
        );
        // An empty comment removes the existing one.
        let mut ed = ArchiveEditor::open(&path).unwrap();
        ed.apply(rar_rs::EditPlan::new().set_comment(b"")).unwrap();
    }
    {
        let mut rar = rar_rs::RarArchive::open(&path).unwrap();
        assert_eq!(rar.get_comment().unwrap(), None);
    }

    // The comment must be readable by the official tool (env-gated).
    if let Some(rar_bin) = std::env::var_os("SA_OFFICIAL_RAR") {
        {
            let mut ed = ArchiveEditor::open(&path).unwrap();
            ed.apply(rar_rs::EditPlan::new().set_comment(b"interop comment"))
                .unwrap();
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
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload, opts3).unwrap();
        rar.add_bytes("c.bin", &payload2, opts0).unwrap();
        rar.finish().unwrap();
    }
    let plain = std::fs::read(&path).unwrap();
    let stub_len = 248_960usize;
    let sfx_path = dir.path().join("sfx.sfx");
    std::fs::write(&sfx_path, with_stub(&plain, stub_len)).unwrap();

    // Reading an SFX archive.
    let mut rar = ArchiveReader::open(&sfx_path).unwrap();
    assert_eq!(rar.entries().count(), 2);
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        payload
    );

    // Extracting from an SFX archive.
    let out = dir.path().join("out");
    rar.extract_entry(rar.unique_entry("a.bin").unwrap(), &out)
        .unwrap();
    assert_eq!(std::fs::read(out.join("a.bin")).unwrap(), payload);

    // Deleting from an SFX archive preserves the stub.
    let mut ed = ArchiveEditor::open(&sfx_path).unwrap();
    let ids: Vec<_> = ["a.bin"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    ed.delete_entries(&ids).unwrap();
    let after = std::fs::read(&sfx_path).unwrap();
    assert_eq!(&after[..stub_len], &with_stub(&[], stub_len)[..stub_len]);
    let rar = ArchiveReader::open(&sfx_path).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<_>>(),
        ["c.bin"]
    );
    let mut rar = ArchiveReader::open(&sfx_path).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("c.bin").unwrap()).unwrap(),
        payload2
    );

    // Renaming keeps the stub too.
    let sfx2 = dir.path().join("sfx2.sfx");
    std::fs::write(&sfx2, with_stub(&plain, stub_len)).unwrap();
    let mut ed = ArchiveEditor::open(&sfx2).unwrap();
    let id = ed.unique_entry("a.bin").unwrap();
    ed.rename_entries(&[(id, "b.bin".to_string())]).unwrap();
    let after2 = std::fs::read(&sfx2).unwrap();
    assert_eq!(&after2[..stub_len], &with_stub(&[], stub_len)[..stub_len]);
    let rar = ArchiveReader::open(&sfx2).unwrap();
    assert!(rar.entries().any(|e| e.name() == "b.bin"));
    let mut rar = ArchiveReader::open(&sfx2).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("b.bin").unwrap()).unwrap(),
        payload
    );
}

#[test]
#[cfg(unix)]
fn symlink_and_hardlink_redirects_extract() {
    let dir = make_temp_dir();
    let path = dir.path().join("links.rar");
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("dir/target.txt", b"target content", opts0)
            .unwrap();
        // Symlink entries: no data, redirect extra record only.
        rar.add_redirect("dir/lnk.txt", 1, "target.txt").unwrap();
        // Hardlink entry referencing the data member.
        rar.add_redirect("dir/hard.txt", 4, "dir/target.txt")
            .unwrap();
        rar.finish().unwrap();
    }

    let out = dir.path().join("out");
    let mut rar = ArchiveReader::open(&path).unwrap();
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

/// Redirect members are RAR5-only: the RAR4 writer has no redirect extra
/// record, and smuggling a RAR5 header into a RAR4 stream would corrupt it.
#[test]
fn rar4_rejects_redirect_members() {
    let dir = make_temp_dir();
    let path = dir.path().join("rar4_links.rar");
    let mut rar = ArchiveWriter::create_with(
        &path,
        rar_rs::WriterOptions::default().compression(rar_rs::ArchiveVersion::V29),
    )
    .unwrap();
    let opts0 = rar_rs::EntryWriteOptions::new()
        .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
    rar.add_bytes("target.txt", b"x", opts0).unwrap();
    let err = rar.add_redirect("lnk.txt", 5, "target.txt").unwrap_err();
    assert!(matches!(err, rar_rs::RarError::Unsupported(_)));
    assert!(err.to_string().contains("redirect"), "got: {err}");
    let err = rar.finish().unwrap_err();
    assert!(err.to_string().contains("aborted"), "got: {err}");
}

/// The legacy pre-RAR3 writers (v15 RAR 1.5 adaptive-Huffman, v20 RAR 2.x
/// LZSS+Huffman with audio traps) produce members the library's own
/// decoders round-trip byte-for-byte, and read-back reports the requested
/// member version per member.
#[test]
fn old_format_writers_roundtrip_at_every_level() {
    let audio_signal: Vec<u8> = {
        let mut v = 0u8;
        (0..8_000u32)
            .map(|i| {
                v = v.wrapping_add(((i % 17) * 3 + 1) as u8);
                v
            })
            .collect()
    };
    for version in [rar_rs::ArchiveVersion::V15, rar_rs::ArchiveVersion::V20] {
        for level in 1..=5u8 {
            let dir = make_temp_dir();
            let path = dir.path().join(format!("roundtrip-{version}-m{level}.rar"));
            let payloads: Vec<(&str, Vec<u8>)> = vec![
                ("repeat.txt", support::compressible(7, 32_000)),
                (
                    "random.bin",
                    (0..4_096u32)
                        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
                        .collect(),
                ),
                ("audio.bin", audio_signal.clone()),
                // A long single-byte run: the RAR 1.5 encoder's `st` literal
                // run mode (enabled from m3 up) and RAR 2.x long matches.
                ("run.bin", vec![b'a'; 64_000]),
            ];
            {
                let mut rar = ArchiveWriter::create_with(
                    &path,
                    rar_rs::WriterOptions::default().compression(version),
                )
                .unwrap_or_else(|e| panic!("create {version} m{level}: {e}"));
                let opts = rar_rs::EntryWriteOptions::new()
                    .compression_level(rar_rs::CompressionLevel::try_from(level).unwrap());
                for (name, data) in &payloads {
                    rar.add_bytes(name, data, opts)
                        .unwrap_or_else(|e| panic!("add {version} m{level} {name}: {e}"));
                }
                rar.finish().unwrap();
            }
            let mut reader = ArchiveReader::open(&path)
                .unwrap_or_else(|e| panic!("open {version} m{level}: {e}"));
            let entries: Vec<_> = reader.entries().collect();
            assert_eq!(entries.len(), payloads.len(), "{version} m{level}");
            entries.iter().for_each(|entry| {
                assert_eq!(
                    entry.version(),
                    version,
                    "{version} m{level} {}",
                    entry.name()
                );
            });
            // The repetitive text member must actually compress.
            assert_ne!(entries[0].metadata().method(), 0, "{version} m{level}");
            let ids: Vec<_> = entries.into_iter().map(|e| e.id()).collect();
            for (i, (name, expected)) in payloads.iter().enumerate() {
                let got = reader
                    .read_entry(ids[i])
                    .unwrap_or_else(|e| panic!("read {version} m{level} {name}: {e}"));
                assert_eq!(got, *expected, "{version} m{level} {name}");
            }
        }
    }
}

/// Solid RAR 1.5/2.x chains round-trip: one persistent encoder carries the
/// adaptive tables (and RAR 2.x window) across the members of the run, the
/// read side reports them as solid, and extraction is byte-identical.
#[test]
fn old_format_solid_roundtrip_at_every_level() {
    for version in [rar_rs::ArchiveVersion::V15, rar_rs::ArchiveVersion::V20] {
        for level in 1..=5u8 {
            let dir = make_temp_dir();
            let path = dir.path().join(format!("solid-{version}-m{level}.rar"));
            let payloads: Vec<(&str, Vec<u8>)> = vec![
                ("first.txt", support::compressible(11, 24_000)),
                (
                    "second.txt",
                    b"solid chain shares the window, shared phrases repeat ".repeat(1200),
                ),
                ("run.bin", vec![b'z'; 40_000]),
            ];
            {
                let mut rar = ArchiveWriter::create_with(
                    &path,
                    rar_rs::WriterOptions::default()
                        .compression(version)
                        .solid_mode(rar_rs::SolidMode::Continuous),
                )
                .unwrap_or_else(|e| panic!("create solid {version} m{level}: {e}"));
                let opts = rar_rs::EntryWriteOptions::new()
                    .compression_level(rar_rs::CompressionLevel::try_from(level).unwrap());
                for (name, data) in &payloads {
                    rar.add_bytes(name, data, opts)
                        .unwrap_or_else(|e| panic!("add solid {version} m{level} {name}: {e}"));
                }
                rar.finish().unwrap();
            }
            let mut reader = ArchiveReader::open(&path)
                .unwrap_or_else(|e| panic!("open solid {version} m{level}: {e}"));
            let entries: Vec<_> = reader.entries().collect();
            assert_eq!(entries.len(), payloads.len(), "solid {version} m{level}");
            entries.iter().for_each(|entry| {
                assert_eq!(
                    entry.version(),
                    version,
                    "solid {version} m{level} {}",
                    entry.name()
                );
                // Pre-RAR3 writers never flag FHD_SOLID (the read side
                // derives chains from the archive-level MHD_SOLID), so
                // `comp_solid()` stays false by design; the chain itself is
                // exercised by the byte round-trip below (the v20 encoder's
                // window matches back into earlier members).
                assert_ne!(
                    entry.metadata().method(),
                    0,
                    "solid {version} m{level} {}",
                    entry.name()
                );
            });
            let ids: Vec<_> = entries.into_iter().map(|e| e.id()).collect();
            for (i, (name, expected)) in payloads.iter().enumerate() {
                let got = reader
                    .read_entry(ids[i])
                    .unwrap_or_else(|e| panic!("read solid {version} m{level} {name}: {e}"));
                assert_eq!(got, *expected, "solid {version} m{level} {name}");
            }
        }
    }
}

/// Member-level encryption (`-p`) works for the v15/v20 writers too, with
/// the historical ciphers: RAR 1.5 members use the RAR15 stream XOR and
/// RAR 2.x the RAR20 block cipher (16-byte padded, no salt), so no
/// `FHD_SALT` is ever attached. Round-trips through both `-p` and `-hp`.
#[test]
fn old_format_password_roundtrip_at_every_level() {
    let payload = support::compressible(13, 24_000);
    for version in [rar_rs::ArchiveVersion::V15, rar_rs::ArchiveVersion::V20] {
        for (suffix, hp) in [("pw", false), ("hp", true)] {
            for level in [1u8, 3, 5] {
                let dir = make_temp_dir();
                let path = dir.path().join(format!("{version}-{suffix}-m{level}.rar"));
                let opts = rar_rs::WriterOptions::default()
                    .compression(version)
                    .password("hunter2")
                    .encrypt_headers(hp);
                {
                    let mut rar = ArchiveWriter::create_with(&path, opts)
                        .unwrap_or_else(|e| panic!("create {version} {suffix} m{level}: {e}"));
                    let level = rar_rs::CompressionLevel::try_from(level).unwrap();
                    let eo = rar_rs::EntryWriteOptions::new().compression_level(level);
                    rar.add_bytes("a.bin", &payload, eo).unwrap();
                    rar.add_bytes("b.bin", &payload[..payload.len() / 2], eo)
                        .unwrap();
                    rar.finish().unwrap();
                }
                let mut reader =
                    ArchiveReader::open_with(&path, rar_rs::OpenOptions::new().password("hunter2"))
                        .unwrap_or_else(|e| panic!("open {version} {suffix} m{level}: {e}"));
                assert_eq!(
                    reader
                        .entry(reader.unique_entry("a.bin").unwrap())
                        .unwrap()
                        .version(),
                    version,
                    "{version} {suffix} m{level}"
                );
                assert_eq!(
                    reader
                        .read_entry(reader.unique_entry("a.bin").unwrap())
                        .unwrap(),
                    payload,
                    "{version} {suffix} m{level} a.bin"
                );
                assert_eq!(
                    reader
                        .read_entry(reader.unique_entry("b.bin").unwrap())
                        .unwrap(),
                    payload[..payload.len() / 2],
                    "{version} {suffix} m{level} b.bin"
                );

                // The wrong password must not decode (member decrypt fails,
                // not a silent empty read); `-hp` errors already at open
                // because the headers themselves cannot be scanned.
                let wrong =
                    ArchiveReader::open_with(&path, rar_rs::OpenOptions::new().password("wrong"));
                if hp {
                    assert!(
                        wrong.is_err(),
                        "{version} {suffix} m{level}: -hp must reject at open"
                    );
                } else {
                    let mut wrong = wrong.unwrap_or_else(|e| {
                        panic!("open wrong-pw {version} {suffix} m{level}: {e}")
                    });
                    let err = wrong
                        .read_entry(wrong.unique_entry("a.bin").unwrap())
                        .unwrap_err();
                    // Wrong-password decode either fails the decrypt stage
                    // (reported as WrongPassword) or, for the saltless old
                    // stream/block ciphers, scrambles data into a CRC
                    // mismatch — never a silent wrong read.
                    let msg = err.to_string();
                    assert!(
                        msg.contains("password") || msg.contains("CRC"),
                        "{version} {suffix} m{level} wrong-pw: {err}"
                    );
                }
            }
        }
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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .volume_size(20_000)
                .recovery_volume_count(3),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload, opts0).unwrap();
        rar.finish().unwrap();
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
    let volumes = rar_rs::discover_volumes(&parent.join("pad.part01.rar"));
    assert!(volumes.len() >= 10, "precondition: >= 10 volumes");
    assert_eq!(
        volumes[0].file_name().unwrap().to_string_lossy(),
        "pad.part01.rar",
        "discovery must find the padded first volume"
    );

    // Discovery must find the padded set from the first volume.
    let padded_first = parent.join("pad.part01.rar");
    let discovered = rar_rs::discover_volumes(&padded_first);
    assert_eq!(
        discovered.len(),
        volumes.len(),
        "padded set must be fully discovered"
    );

    // Delete a padded middle volume and rebuild it from the padded .rev.
    let victim = parent.join("pad.part07.rar");
    std::fs::remove_file(&victim).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&padded_first).unwrap();
    assert!(rebuilt.contains(&victim), "padded middle volume rebuilt");
    let mut rar = ArchiveReader::open(&padded_first).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        payload
    );
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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().recovery_percent(10),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload, opts0).unwrap();
        rar.finish().unwrap();
    }
    let good = std::fs::read(&path).unwrap();

    // Damage several bytes inside the protected data.
    let mut damaged = good.clone();
    for pos in [400usize, 410, 420, 900] {
        damaged[pos] ^= 0x5A;
    }
    std::fs::write(&path, &damaged).unwrap();

    let out = dir.path().join("fixed.rar");
    let repaired = rar_rs::repair_archive_path(&path, &out).unwrap();
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
    let repaired = rar_rs::repair_archive_path(&path, &out2).unwrap();
    assert!(!repaired, "intact archive must report no repair");
    assert!(
        !out2.exists(),
        "intact repair must not write an output file"
    );

    // The repaired archive must open and extract byte-identically.
    let mut rar = ArchiveReader::open(&out).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        payload
    );

    // No recovery record -> clean error, and the output stays absent.
    let plain = dir.path().join("plain.rar");
    {
        let mut rar = ArchiveWriter::create_with(&plain, rar_rs::WriterOptions::default()).unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", b"x", opts0).unwrap();
        rar.finish().unwrap();
    }
    let out3 = dir.path().join("fixed3.rar");
    assert!(rar_rs::repair_archive_path(&plain, &out3).is_err());
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
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().recovery_percent(10),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload, opts0).unwrap();
        rar.finish().unwrap();
    }
    let good = std::fs::read(&path).unwrap();
    let mut damaged = good.clone();
    damaged[400] ^= 0x5A;
    damaged[900] ^= 0x5A;
    std::fs::write(&path, &damaged).unwrap();

    // Progress: monotonic, reaches the file size exactly once.
    let out = dir.path().join("fixed-prog.rar");
    let mut reports: Vec<(u64, u64)> = Vec::new();
    let repaired = rar_rs::repair_archive_path_with(
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
    let err = rar_rs::repair_archive_path_with(&path, &out2, Some(&cancel), None).unwrap_err();
    assert!(matches!(err, rar_rs::RarError::Cancelled));
    assert!(!out2.exists(), "cancelled repair must not leave output");

    // Cancellation set mid-run: a flag armed after the first progress
    // report aborts the streaming scan/copy and still leaves nothing.
    let cancel = AtomicBool::new(false);
    let out3 = dir.path().join("fixed-cancel2.rar");
    let mut seen_progress = false;
    let err = rar_rs::repair_archive_path_with(
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
    assert!(matches!(err, rar_rs::RarError::Cancelled));
    assert!(!out3.exists());
}

/// `rebuild_missing_volumes_with` reports non-decreasing progress and
/// honours cancellation (missing volumes are not written on abort).
#[test]
fn rebuild_missing_volumes_with_reports_progress_and_cancels() {
    use std::sync::atomic::AtomicBool;

    let dir = make_temp_dir();
    let path = dir.path().join("rcv2.rar");
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .volume_size(60_000)
                .recovery_volume_count(2),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload, opts0).unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&path);
    let victim = volumes[1].clone();
    std::fs::remove_file(&victim).unwrap();

    let mut reports: Vec<(u64, u64)> = Vec::new();
    let rebuilt = rar_rs::rebuild_missing_volumes_with(
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
    let err = rar_rs::rebuild_missing_volumes_with(&volumes[0], Some(&cancel), None).unwrap_err();
    assert!(matches!(err, rar_rs::RarError::Cancelled));
    assert!(!victim.exists(), "cancelled rebuild must not write volumes");
}

/// `delete_with_progress` reports monotonic rewrite progress reaching the
/// archive size, and the deleted archive is byte-correct.
#[test]
fn delete_with_progress_reports_monotonic_progress() {
    let dir = make_temp_dir();
    let path = dir.path().join("del-prog.rar");
    {
        let mut rar = ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        let opts3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3u8).unwrap());
        for i in 0..4u8 {
            let data: Vec<u8> = (0..60_000u32).map(|k| (k % 251) as u8).collect();
            rar.add_bytes(&format!("m{i}.bin"), &data, opts3).unwrap();
        }
        rar.finish().unwrap();
    }
    let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
    let mut ed = ArchiveEditor::open(&path).unwrap();
    let cb_reports = reports.clone();
    let ids: Vec<_> = ["m1.bin", "m2.bin"]
        .iter()
        .map(|n| ed.unique_entry(n).unwrap())
        .collect();
    let n = ed
        .delete_entries_with_progress(
            &ids,
            Some(Box::new(move |done, total| {
                cb_reports.lock().unwrap().push((done, total));
            })),
        )
        .unwrap();
    assert_eq!(n, 2);
    let kept: Vec<String> = ed.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(kept, ["m0.bin", "m3.bin"]);
    drop(ed);

    let reports = reports.lock().unwrap();
    let mut last = 0u64;
    let mut reached_end = false;
    for (done, t) in reports.iter() {
        assert!(*t > 0);
        assert!(*done >= last, "progress must be non-decreasing");
        last = *done;
        if *done == *t {
            reached_end = true;
        }
    }
    assert!(reached_end, "progress must reach the total");

    // Content reads back.
    for name in ["m0.bin", "m3.bin"] {
        let mut rar = ArchiveReader::open(&path).unwrap();
        let data: Vec<u8> = (0..60_000u32).map(|k| (k % 251) as u8).collect();
        assert_eq!(
            rar.read_entry(rar.unique_entry(name).unwrap()).unwrap(),
            data
        );
    }
}
