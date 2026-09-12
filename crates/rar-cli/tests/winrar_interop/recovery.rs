use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, OpenOptions, WriterOptions,
};

use crate::support::{
    file_sha256, rar_bin, rar4_623_bin, run, temp_dir, unrar_bin, unrar_extract, unrar_test,
    write_pattern_file,
};

// ── rv/rc recovery-volume cross-validation (Phase 2.1) ─────────────────────

/// Phase 2.1 cross-validation, direction 1: WinRAR builds the volume set
/// and its `.rev` recovery volumes — both `-rv2` at create time and the
/// standalone `rv` command — we delete a middle volume, and OUR `rc` must
/// rebuild it byte-identically. Both sets use >= 10 volumes, so WinRAR
/// zero-pads the part numbers (part01..partNN); discovery, `.rev`
/// probing and rebuild must handle WinRAR's real naming.
#[test]
fn winrar_rv_then_our_rc_rebuilds_byte_identical() {
    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 1_400_000, 7); // STORE: spans ~14 x 100k volumes

    // (a) Recovery volumes created at archive time with `-rv2`.
    let set_a = dir.path().join("seta.rar");
    let (ok, out) = run(Command::new(&rar)
        .args(["a", "-m0", "-v100k", "-rv2", "-idq"])
        .arg(&set_a)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR -rv2 creation failed:\n{out}");
    let first_a = dir.path().join("seta.part01.rar");
    let volumes_a = rar_rs::discover_volumes(&first_a);
    assert!(
        volumes_a.len() >= 10,
        "precondition: >= 10 volumes so WinRAR zero-pads, got {}",
        volumes_a.len()
    );
    assert!(
        dir.path().join("seta.part01.rev").exists(),
        "WinRAR -rv2 must create padded .rev files"
    );

    // Delete a middle volume; our `rc` rebuilds it byte-identically.
    let victim_a = dir.path().join("seta.part07.rar");
    let victim_bytes_a = std::fs::read(&victim_a).unwrap();
    std::fs::remove_file(&victim_a).unwrap();
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rc", "-idq"])
        .arg(&first_a));
    assert!(ok, "our rar rc failed on WinRAR's padded set:\n{out}");
    assert_eq!(
        std::fs::read(&victim_a).unwrap(),
        victim_bytes_a,
        "our rc must rebuild the WinRAR volume byte-identically"
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first_a, None);
        assert!(ok, "UnRAR rejected the set rebuilt by our rc:\n{out}");
    }

    // (b) Standalone `rv` command on an existing set (default 10%).
    let set_b = dir.path().join("setb.rar");
    let (ok, out) = run(Command::new(&rar)
        .args(["a", "-m0", "-v100k", "-idq"])
        .arg(&set_b)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR creation failed:\n{out}");
    let first_b = dir.path().join("setb.part01.rar");
    let (ok, out) = run(Command::new(&rar).args(["rv", "-idq"]).arg(&first_b));
    assert!(ok, "WinRAR rv failed:\n{out}");
    assert!(
        dir.path().join("setb.part01.rev").exists(),
        "WinRAR rv must create padded .rev files"
    );
    let victim_b = dir.path().join("setb.part05.rar");
    let victim_bytes_b = std::fs::read(&victim_b).unwrap();
    std::fs::remove_file(&victim_b).unwrap();
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rc", "-idq"])
        .arg(&first_b));
    assert!(ok, "our rar rc failed on WinRAR's rv set:\n{out}");
    assert_eq!(
        std::fs::read(&victim_b).unwrap(),
        victim_bytes_b,
        "our rc must rebuild byte-identically"
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first_b, None);
        assert!(ok, "UnRAR rejected the second rebuilt set:\n{out}");
    }

    // Both rebuilt sets must also read back with our own reader.
    for first in [&first_a, &first_b] {
        let mut ar = ArchiveReader::open(first).unwrap();
        let name = ar
            .entries()
            .find(|e| e.name().ends_with("big.bin"))
            .unwrap()
            .name()
            .to_string();
        assert_eq!(
            ar.read_entry(ar.unique_entry(&name).unwrap()).unwrap(),
            std::fs::read(&src).unwrap()
        );
    }
}

/// Phase 2.1 cross-validation, direction 2: we build the volume set and
/// its `.rev` recovery volumes with our own `rv`, then WinRAR's `rc` must
/// reconstruct a deleted volume byte-identically (unpadded set: fewer
/// than 10 volumes).
#[test]
fn our_rv_then_winrar_rc_rebuilds_byte_identical() {
    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 700_000, 11); // STORE: 4 x 200k volumes

    // Our volume set (unpadded part1..part4).
    let set = dir.path().join("ours.rar");
    {
        let mut rar =
            ArchiveWriter::create_with(&set, WriterOptions::default().volume_size(200_000))
                .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&set);
    assert!(
        (3..10).contains(&volumes.len()),
        "precondition: a small unpadded set, got {}",
        volumes.len()
    );

    // Our `rv` command adds the .rev files (exact count 2).
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rv", "-idq"])
        .arg(&set)
        .arg("2"));
    assert!(ok, "our rar rv failed:\n{out}");
    assert!(dir.path().join("ours.part1.rev").exists());
    assert!(dir.path().join("ours.part2.rev").exists());

    // Delete a middle volume; WinRAR `rc` rebuilds it byte-identically.
    let victim = volumes[1].clone();
    let victim_bytes = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let (ok, out) = run(Command::new(&rar).args(["rc", "-idq"]).arg(&volumes[0]));
    assert!(ok, "WinRAR rc failed on our set:\n{out}");
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        victim_bytes,
        "WinRAR rc must rebuild our volume byte-identically"
    );

    // Both tools must read the rebuilt set.
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected the rebuilt set:\n{out}");
    }
    let mut ar = ArchiveReader::open(&volumes[0]).unwrap();
    let name = ar
        .entries()
        .find(|e| e.name().ends_with("big.bin"))
        .unwrap()
        .name()
        .to_string();
    assert_eq!(
        ar.read_entry(ar.unique_entry(&name).unwrap()).unwrap(),
        std::fs::read(&src).unwrap()
    );
}

/// Phase 2.1 cross-validation, direction 3: zero-padded volume sets.
/// WinRAR creates a >= 10 volume set (part01..partNN); our `rv` adds
/// `.rev` files named with the set's zero-padding; then both WinRAR's and
/// our `rc` rebuild deleted volumes byte-identically from the same
/// `.rev` files.
#[test]
fn zero_padded_volume_sets_rv_rc_cross_validate() {
    let Some(rar) = rar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 1_500_000, 13); // STORE: ~15 x 100k volumes

    // WinRAR creates the padded set (>= 10 volumes -> part01..partNN).
    let set = dir.path().join("pad.rar");
    let (ok, out) = run(Command::new(&rar)
        .args(["a", "-m0", "-v100k", "-idq"])
        .arg(&set)
        .arg("big.bin")
        .current_dir(dir.path()));
    assert!(ok, "WinRAR creation failed:\n{out}");
    let first = dir.path().join("pad.part01.rar");
    let volumes = rar_rs::discover_volumes(&first);
    assert!(
        volumes.len() >= 10,
        "precondition: >= 10 volumes so WinRAR zero-pads, got {}",
        volumes.len()
    );

    // Our `rv` adds .rev files with the set's zero-padding.
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rv", "-idq"])
        .arg(&first));
    assert!(ok, "our rar rv failed on the padded set:\n{out}");
    assert!(
        dir.path().join("pad.part01.rev").exists() && dir.path().join("pad.part02.rev").exists(),
        "our .rev names must follow the set's zero-padding"
    );
    assert!(
        !dir.path().join("pad.part1.rev").exists(),
        "no unpadded .rev name may be created"
    );

    // WinRAR `rc` rebuilds a deleted volume from our .rev files.
    let victim = dir.path().join("pad.part09.rar");
    let victim_bytes = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let (ok, out) = run(Command::new(&rar).args(["rc", "-idq"]).arg(&first));
    assert!(ok, "WinRAR rc failed on our padded .rev files:\n{out}");
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        victim_bytes,
        "WinRAR rc must rebuild from our padded .rev byte-identically"
    );

    // Our `rc` rebuilds a different volume from the same .rev files.
    let victim2 = dir.path().join("pad.part12.rar");
    let victim2_bytes = std::fs::read(&victim2).unwrap();
    std::fs::remove_file(&victim2).unwrap();
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rc", "-idq"])
        .arg(&first));
    assert!(ok, "our rar rc failed on the padded set:\n{out}");
    assert_eq!(
        std::fs::read(&victim2).unwrap(),
        victim2_bytes,
        "our rc must rebuild from the same .rev files byte-identically"
    );

    // Both tools validate the final set.
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first, None);
        assert!(ok, "UnRAR rejected the rebuilt padded set:\n{out}");
    }
    let mut ar = ArchiveReader::open(&first).unwrap();
    let name = ar
        .entries()
        .find(|e| e.name().ends_with("big.bin"))
        .unwrap()
        .name()
        .to_string();
    assert_eq!(
        ar.read_entry(ar.unique_entry(&name).unwrap()).unwrap(),
        std::fs::read(&src).unwrap()
    );
}

/// Recovery record (`-rr`) + encryption (`-p`): both directions. Single
/// volume only (WinRAR forbids inline recovery records on multi-volume
/// sets, which use `.rev` instead).
#[test]
fn recovery_record_with_encryption_interops() {
    let dir = temp_dir();
    let src = dir.path().join("rr.bin");
    write_pattern_file(&src, 3 * 1024 * 1024, 31);

    // WinRAR -> ours: -rr + -p, then we read with the password.
    if let Some(rar) = rar_bin() {
        let arc = dir.path().join("win_rr.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-rr", "-psecret", "-idq"])
            .arg(&arc)
            .arg("rr.bin")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -rr -p failed:\n{out}");
        let mut ar = ArchiveReader::open_with(&arc, OpenOptions::new().password("secret")).unwrap();
        let name = ar.entries().next().unwrap().name().to_string();
        assert_eq!(
            ar.read_entry(ar.unique_entry(&name).unwrap()).unwrap(),
            std::fs::read(&src).unwrap(),
            "rar-rs read a different file from WinRAR's -rr -p archive"
        );
    }

    // Ours -> WinRAR: recovery_percent + password.
    let arc = dir.path().join("ours_rr.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .password("secret")
                .recovery_percent(10),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&arc, Some("secret"));
        assert!(ok, "UnRAR rejected our -rr -p archive:\n{out}");
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&arc, &dest, Some("secret"));
        assert!(ok, "UnRAR failed to extract our -rr -p archive:\n{out}");
        assert_eq!(
            file_sha256(&dest.join("rr.bin")),
            file_sha256(&src),
            "WinRAR extracted different bytes from our -rr -p archive"
        );
    }
}

/// RAR4 inline recovery records (`-ma4 -rr`) interoperate both ways with
/// WinRAR 6.23 (the last RAR4 writer): WinRAR's `rar r` repairs damage
/// from OUR NEWSUB (0x7a) record byte-identically, and OUR repair path
/// rebuilds a damaged WinRAR-made RAR4 RR archive.
#[test]
fn rar4_recovery_record_interops_with_winrar() {
    let dir = temp_dir();
    // Pseudo-random payload (NOT the periodic pattern: WinRAR 6.23's RAR4
    // repair mis-rebuilds periodic data whether the record is its own or
    // ours, so a byte-identical repair check needs aperiodic bytes).
    let src = dir.path().join("rr4.bin");
    let mut content = Vec::with_capacity(500 * 1024);
    let mut seed = 41u32;
    while content.len() < 500 * 1024 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        content.push((seed >> 24) as u8);
    }
    std::fs::write(&src, &content).unwrap();

    // A stored RAR4 member's payload starts right after the file header,
    // which sits after the 7-byte signature + 13-byte main header. The
    // file header's own head_size field (offset +5 within it) tells where.
    fn payload_offset(raw: &[u8]) -> usize {
        let fh = 7 + 13;
        assert_eq!(&raw[..7], b"Rar!\x1a\x07\x00");
        let hsize = u16::from_le_bytes([raw[fh + 5], raw[fh + 6]]) as usize;
        fh + hsize
    }

    // Ours -> WinRAR: create with -ma4 -rr10%, damage a payload sector, and
    // both OUR repair and WinRAR's `rar r` rebuild it.
    let arc = dir.path().join("our_rr4.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m0", "-rr10%", "-idq"])
        .arg(&arc)
        .arg("rr4.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -rr10% failed:\n{out}");

    // Our own repair round-trip: damage -> fix -> byte-identical.
    let mut damaged = std::fs::read(&arc).unwrap();
    let start = payload_offset(&damaged);
    damaged[start + 8000..start + 8000 + 128].fill(0x5a);
    let damaged_path = dir.path().join("our_rr4_damaged.rar");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let fixed_path = dir.path().join("our_rr4_fixed.rar");
    let repaired =
        rar_rs::repair_legacy_archive_path(&damaged_path, &fixed_path).expect("our repair");
    assert!(repaired, "our repair must find and fix the damage");
    assert_eq!(
        std::fs::read(&fixed_path).unwrap(),
        std::fs::read(&arc).unwrap(),
        "our repair must restore the archive byte-identically"
    );

    // WinRAR repairs the same damage from our recovery record (6.23 only:
    // 7.23 cannot repair RAR4 recovery records).
    if let Some(rar) = rar4_623_bin() {
        let (ok, out) = run(Command::new(&rar)
            .args(["r", "-y", "-idq"])
            .arg("our_rr4_damaged.rar")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR r failed on our RAR4 RR archive:\n{out}");
        let win_fixed = dir.path().join("fixed.our_rr4_damaged.rar");
        assert!(
            win_fixed.exists(),
            "WinRAR must write fixed.our_rr4_damaged.rar"
        );
        let out_dir = dir.path().join("win_rr4_out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(unrar_bin().unwrap())
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&win_fixed)
            .arg(&out_dir));
        assert!(ok, "UnRAR x of the WinRAR-fixed archive failed:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("rr4.bin")),
            file_sha256(&src),
            "WinRAR rebuilt different bytes from our RAR4 RR record"
        );
    }

    // WinRAR -> ours: WinRAR 6.23 creates the record, we repair damage.
    if let Some(rar) = rar4_623_bin() {
        let warc = dir.path().join("win_rr4.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-ma4", "-m0", "-rr10%", "-idq"])
            .arg(&warc)
            .arg("rr4.bin")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -ma4 -rr10% failed:\n{out}");
        let mut wdamaged = std::fs::read(&warc).unwrap();
        let wstart = payload_offset(&wdamaged);
        wdamaged[wstart + 12345..wstart + 12345 + 96].fill(0xa5);
        let wdamaged_path = dir.path().join("win_rr4_damaged.rar");
        std::fs::write(&wdamaged_path, &wdamaged).unwrap();
        let wfixed_path = dir.path().join("win_rr4_fixed.rar");
        let repaired =
            rar_rs::repair_legacy_archive_path(&wdamaged_path, &wfixed_path).expect("our repair");
        assert!(repaired, "our repair must fix the WinRAR RAR4 RR archive");
        let mut ar = ArchiveReader::open(&wfixed_path).unwrap();
        assert_eq!(
            ar.read_entry(ar.unique_entry("rr4.bin").unwrap()).unwrap(),
            std::fs::read(&src).unwrap(),
            "we repaired WinRAR's RAR4 RR archive to different bytes"
        );
    }
}
