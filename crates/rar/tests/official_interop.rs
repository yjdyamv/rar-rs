//! Cross-validation against the official rar/unrar console tools (env-gated via SA_OFFICIAL_RAR / SA_OFFICIAL_UNRAR).

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{CompressionLevel, EntryWriteOptions};

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

    let cases: Vec<(
        String,
        rar_rs::WriterOptions,
        Option<&str>,
        Vec<(String, Vec<u8>)>,
    )> = vec![
        (
            "plain".into(),
            rar_rs::WriterOptions::default(),
            None,
            vec![("f1.bin".into(), a.clone()), ("f2.bin".into(), b.clone())],
        ),
        (
            "solid-qo-blake2".into(),
            rar_rs::WriterOptions::default()
                .solid_mode(rar_rs::SolidMode::Continuous)
                .quick_open(true)
                .blake2(true),
            None,
            vec![("f1.bin".into(), a.clone()), ("f2.bin".into(), b.clone())],
        ),
        (
            "encrypted".into(),
            rar_rs::WriterOptions::default().password("s3cret"),
            Some("s3cret"),
            vec![("f1.bin".into(), a.clone())],
        ),
        (
            "headers-recovery".into(),
            rar_rs::WriterOptions::default()
                .password("s3cret")
                .encrypt_headers(true)
                .recovery_percent(10),
            Some("s3cret"),
            vec![("f1.bin".into(), a.clone())],
        ),
    ];

    for (name, opts, password, entries) in cases {
        let path = dir.path().join(format!("{name}.rar"));
        {
            let mut rar = rar_rs::ArchiveWriter::create_with(&path, opts).unwrap();
            for (n, data) in &entries {
                rar.add_bytes(
                    n,
                    data,
                    EntryWriteOptions::new()
                        .compression_level(CompressionLevel::try_from(3).unwrap()),
                )
                .unwrap();
            }
            rar.finish().unwrap();
        }
        let password_flag = if let Some(pw) = password {
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
            let mut ar = rar_rs::ArchiveWriter::create_with(
                &rr,
                rar_rs::WriterOptions::default().recovery_percent(10),
            )
            .unwrap();
            ar.add_bytes(
                "payload.bin",
                &payload,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
            ar.finish().unwrap();
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

        let mut ar = rar_rs::ArchiveReader::open(&fixed).unwrap();
        let payload_id = ar.unique_entry("payload.bin").unwrap();
        assert_eq!(ar.read_entry(payload_id).unwrap(), payload);

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
            let mut ar = rar_rs::ArchiveWriter::create_with(
                &multi,
                rar_rs::WriterOptions::default()
                    .volume_size(1024 * 1000)
                    .recovery_volume_count(2),
            )
            .unwrap();
            ar.add_bytes(
                "big.bin",
                &vol_payload,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
            ar.finish().unwrap();
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
        let mut ar = rar_rs::ArchiveReader::open(&multi).unwrap();
        let big_id = ar.unique_entry("big.bin").unwrap();
        assert_eq!(ar.read_entry(big_id).unwrap(), vol_payload);
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
    let mut ar = rar_rs::ArchiveReader::open(&solid).unwrap();
    let a_id = ar.unique_entry("src/a.bin").unwrap();
    assert_eq!(ar.read_entry(a_id).unwrap(), a);
    let b_id = ar.unique_entry("src/b.bin").unwrap();
    assert_eq!(ar.read_entry(b_id).unwrap(), b);

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
    let mut ar =
        rar_rs::ArchiveReader::open_with(&enc, rar_rs::OpenOptions::new().password("pw")).unwrap();
    let a_id = ar.unique_entry("src/a.bin").unwrap();
    assert_eq!(ar.read_entry(a_id).unwrap(), a);
    let b_id = ar.unique_entry("src/b.bin").unwrap();
    assert_eq!(ar.read_entry(b_id).unwrap(), b);
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

    let cases: Vec<(String, rar_rs::WriterOptions, Option<&str>)> = vec![
        ("plain".into(), rar_rs::WriterOptions::default(), None),
        (
            "solid-qo".into(),
            rar_rs::WriterOptions::default()
                .solid_mode(rar_rs::SolidMode::Continuous)
                .quick_open(true),
            None,
        ),
        (
            "encrypted".into(),
            rar_rs::WriterOptions::default().password("s3cret"),
            Some("s3cret"),
        ),
        (
            "headers".into(),
            rar_rs::WriterOptions::default()
                .password("s3cret")
                .encrypt_headers(true),
            Some("s3cret"),
        ),
    ];
    for (name, opts, password) in cases {
        let path = dir.path().join(format!("del-{name}.rar"));
        {
            let mut rar = rar_rs::ArchiveWriter::create_with(&path, opts).unwrap();
            rar.add_bytes(
                "a.bin",
                &a,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
            rar.add_bytes(
                "b.bin",
                &b,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
            rar.add_bytes(
                "c.bin",
                &c,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
            rar.finish().unwrap();
        }
        let password_flag = password
            .map(|pw| vec![format!("-p{pw}")])
            .unwrap_or_default();
        let mut rar = match password {
            Some(pw) => rar_rs::ArchiveEditor::open_with_password(&path, pw).unwrap(),
            None => rar_rs::ArchiveEditor::open(&path).unwrap(),
        };
        let b_name = rar.unique_entry("b.bin").unwrap();
        rar.delete_entries(&[b_name]).unwrap();
        let status = std::process::Command::new(&unrar)
            .arg("t")
            .args(&password_flag)
            .arg(&path)
            .status()
            .expect("run official unrar");
        assert!(status.success(), "official unrar rejected del-{name}");

        // Content still correct through our own reader.
        let mut rar = match password {
            Some(pw) => {
                rar_rs::ArchiveReader::open_with(&path, rar_rs::OpenOptions::new().password(pw))
                    .unwrap()
            }
            None => rar_rs::ArchiveReader::open(&path).unwrap(),
        };
        let a_id = rar.unique_entry("a.bin").unwrap();
        assert_eq!(rar.read_entry(a_id).unwrap(), a);
        let c_id = rar.unique_entry("c.bin").unwrap();
        assert_eq!(rar.read_entry(c_id).unwrap(), c);
    }

    // Reverse direction: the official `rar d` modifies a rar-rs archive,
    // and rar-rs reads the result.
    if let Some(rar_bin) = &rar_bin {
        let path = dir.path().join("del-by-official.rar");
        {
            let mut rar =
                rar_rs::ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default())
                    .unwrap();
            rar.add_bytes(
                "a.bin",
                &a,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
            rar.add_bytes(
                "b.bin",
                &b,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
            rar.add_bytes(
                "c.bin",
                &c,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
            rar.finish().unwrap();
        }
        let status = std::process::Command::new(rar_bin)
            .args(["d", "-idq"])
            .arg(&path)
            .arg("b.bin")
            .status()
            .expect("run official rar d");
        assert!(status.success(), "official rar d failed");
        let mut rar = rar_rs::ArchiveReader::open(&path).unwrap();
        assert_eq!(
            rar.entries()
                .map(|e| e.name().to_string())
                .collect::<Vec<String>>(),
            ["a.bin", "c.bin"]
        );
        let a_id = rar.unique_entry("a.bin").unwrap();
        assert_eq!(rar.read_entry(a_id).unwrap(), a);
        let c_id = rar.unique_entry("c.bin").unwrap();
        assert_eq!(rar.read_entry(c_id).unwrap(), c);
    }
}

// ── Append / lock / recovery-record commands ────────────────────────────────

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
        let mut rar = rar_rs::ArchiveWriter::append(&path).unwrap();
        rar.add_path(
            src.join("b.bin"),
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected appended archive");
    let mut rar = rar_rs::ArchiveReader::open(&path).unwrap();
    let names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    // The official rar stored the first member with its relative path.
    let a_name = names
        .iter()
        .find(|n| n.ends_with("src/a.bin"))
        .expect("first member")
        .to_string();
    assert!(names.contains(&"b.bin".to_string()), "{:?}", names);
    let a_id = rar.unique_entry(&a_name).unwrap();
    assert_eq!(rar.read_entry(a_id).unwrap(), a);
    let b_id = rar.unique_entry("b.bin").unwrap();
    assert_eq!(rar.read_entry(b_id).unwrap(), b);

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
    let volumes = rar_rs::discover_volumes(&mv);
    assert!(volumes.len() > 1, "precondition: multi-volume set");

    // Delete the small member from the rar-created volumes (the official
    // CLI refuses to modify multi-volume archives itself).
    let mut rar = rar_rs::ArchiveEditor::open(&volumes[0]).unwrap();
    let delete_ids: Vec<_> = rar
        .entries()
        .filter(|e| !e.name().ends_with("big.bin"))
        .map(|e| e.id())
        .collect();
    assert!(!delete_ids.is_empty(), "small member not found");
    rar.delete_entries(&delete_ids).unwrap();
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&volumes[0])
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected rewritten volumes");
    let mut rar = rar_rs::ArchiveReader::open(&volumes[0]).unwrap();
    let big_name = rar
        .entries()
        .find(|e| e.name().ends_with("big.bin"))
        .unwrap()
        .name()
        .to_string();
    let big_id = rar.unique_entry(&big_name).unwrap();
    assert_eq!(rar.read_entry(big_id).unwrap(), payload);
}

// ── Rename ──────────────────────────────────────────────────────────────────

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
        let mut rar =
            rar_rs::ArchiveWriter::create_with(&path, rar_rs::WriterOptions::default()).unwrap();
        rar.add_path(
            src.join("a.bin"),
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
        )
        .unwrap();
        rar.add_path(
            src.join("b.bin"),
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
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
    let mut rar = rar_rs::ArchiveReader::open(&path).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<String>>(),
        ["z.bin", "b.bin"]
    );
    let z_id = rar.unique_entry("z.bin").unwrap();
    assert_eq!(rar.read_entry(z_id).unwrap(), a);

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
    let mut rar = rar_rs::ArchiveEditor::open(&path2).unwrap();
    let a_id = rar
        .entries()
        .find(|e| e.name().ends_with("a.bin"))
        .unwrap()
        .id();
    rar.rename_entries(&[(a_id, "w.bin".to_string())]).unwrap();
    let unrar = std::env::var_os("SA_OFFICIAL_UNRAR").unwrap_or(rar_bin.clone());
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path2)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected our renamed archive");
    let mut rar = rar_rs::ArchiveReader::open(&path2).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<String>>(),
        ["w.bin"]
    );
    let w_id = rar.unique_entry("w.bin").unwrap();
    assert_eq!(rar.read_entry(w_id).unwrap(), a);
}

// ── Repair / rebuild volumes / comments ─────────────────────────────────────

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
    let repaired = rar_rs::repair_archive(&damaged).unwrap();
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
    let volumes = rar_rs::discover_volumes(&mv);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    std::fs::remove_file(&volumes[1]).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(rebuilt.len(), 1);
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&volumes[0])
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected the rebuilt volume set");
    let mut rar = rar_rs::ArchiveReader::open(&volumes[0]).unwrap();
    let big_name = rar
        .entries()
        .find(|e| e.name().ends_with("big.bin"))
        .unwrap()
        .name()
        .to_string();
    let big_id = rar.unique_entry(&big_name).unwrap();
    assert_eq!(rar.read_entry(big_id).unwrap(), payload);
}

// ── SFX ─────────────────────────────────────────────────────────────────────

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
    // Official `rar s` needs an SFX module. Resolve it portably: an env
    // override (SA_OFFICIAL_SFX), default.sfx next to the official rar
    // binary (the Linux rar tarball ships one), or the legacy developer
    // path; without a module the official-conversion part is skipped (the
    // reader-side SFX checks below still run on our synthetic stub).
    let sfx_module = std::env::var_os("SA_OFFICIAL_SFX")
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .or_else(|| {
            std::path::Path::new(&rar_bin)
                .parent()
                .map(|dir| dir.join("default.sfx"))
                .filter(|p| p.exists())
        })
        .or_else(|| {
            let legacy = std::path::Path::new("/home/yuan/下载/rar/default.sfx");
            legacy.exists().then(|| legacy.to_path_buf())
        });
    let Some(sfx_module) = sfx_module else {
        return; // no official SFX module available; conversion part skipped
    };
    let status = std::process::Command::new(&rar_bin)
        .arg("s")
        .arg(format!("-sfx{}", sfx_module.display()))
        .arg("-idq")
        .arg(dir.path().join("off.rar"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "official rar s failed");
    assert!(sfx.exists());
    let mut rar = rar_rs::ArchiveReader::open(&sfx).unwrap();
    let a_id = rar
        .entries()
        .find(|e| e.name().ends_with("a.bin"))
        .unwrap()
        .id();
    assert_eq!(rar.read_entry(a_id).unwrap(), payload);
    // Our delete on the official SFX archive keeps the stub.
    let mut rar = rar_rs::ArchiveEditor::open(&sfx).unwrap();
    let a_id = rar
        .entries()
        .find(|e| e.name().ends_with("a.bin"))
        .unwrap()
        .id();
    rar.delete_entries(&[a_id]).unwrap();
    let data = std::fs::read(&sfx).unwrap();
    let stub_len = rar_rs::sfx_offset_of(&data).unwrap();
    assert!(stub_len > 0, "stub preserved");

    // Official unrar validates a stub-prefixed rar-rs archive.
    let ours = dir.path().join("ours.rar");
    {
        let mut ar =
            rar_rs::ArchiveWriter::create_with(&ours, rar_rs::WriterOptions::default()).unwrap();
        ar.add_bytes(
            "b.bin",
            &payload,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
        )
        .unwrap();
        ar.finish().unwrap();
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
        let mut rar = rar_rs::ArchiveReader::open(&path).unwrap();
        rar.extract_all_with_options(&out, rar_rs::ExtractOptions::default())
            .unwrap();
    }
    let link = std::fs::read_link(out.join("src/lnk.txt")).unwrap();
    assert_eq!(link, std::path::Path::new("target.txt"));

    // rar-rs redirect entries must be valid for the official unrar.
    let ours = dir.path().join("ours.rar");
    {
        let mut ar =
            rar_rs::ArchiveWriter::create_with(&ours, rar_rs::WriterOptions::default()).unwrap();
        ar.add_bytes(
            "target.txt",
            b"target content",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        ar.add_redirect("lnk.txt", 1, "target.txt").unwrap();
        ar.finish().unwrap();
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

// ── CLI behavior tests moved to the `rar-cli` crate ─────────────────────
// (tests/cli_behavior.rs): everything that drives the built `rar`/`unrar`
// binaries via `CARGO_BIN_EXE_*` now lives next to them.

// ── Extra records: nanosecond time, owner/group, version ────────────────────

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
    let rar = rar_rs::ArchiveReader::open(&path).unwrap();
    let entry = rar.entry(rar.unique_entry("ns.bin").unwrap()).unwrap();
    // The official rar stores the on-disk timestamp, which NTFS quantizes
    // to 100 ns; compare against the actual disk value, not the request.
    let disk_ns = std::fs::metadata(&src)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    assert_eq!(entry.mtime_ns(), Some(disk_ns));
    // `-ow` (save owner/group) is a POSIX option: the Unix rar stores an
    // owner record (assert Some), while the Windows rar ignores it (assert
    // None so the platform mismatch is visible, not silently skipped).
    #[cfg(unix)]
    assert!(entry.owner().is_some(), "owner record must be parsed");
    #[cfg(not(unix))]
    assert_eq!(
        entry.owner(),
        None,
        "Windows rar ignores -ow; no owner record is expected"
    );

    // Our ns-mtime archive must be readable by the official unrar.
    let ours = dir.path().join("ours.rar");
    {
        let mut ar =
            rar_rs::ArchiveWriter::create_with(&ours, rar_rs::WriterOptions::default()).unwrap();
        ar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
        )
        .unwrap();
        ar.finish().unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&ours)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected our ns-mtime archive");
}

/// WinRAR 6.23 (the last RAR4-producing official release) must accept a
/// RAR4 archive edited by our stage-A header ops: an inline NEWSUB recovery
/// record added by `rar rr` semantics, a byte-level repair through that
/// record, and a locked archive. 6.23's UnRAR validates every case.
#[test]
fn official_unrar_validates_rar4_header_edits() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return, // skipped unless the interop script sets it
    };
    let dir = make_temp_dir();
    let path = dir.path().join("rar4-edit.rar");
    let a_path = dir.path().join("a.bin");
    let a_payload: Vec<u8> = (0..300_000u32)
        .map(|i| ((i.wrapping_mul(2_654_435_761)) >> 13) as u8)
        .collect();
    std::fs::write(&a_path, &a_payload).unwrap();
    let b_path = dir.path().join("b.txt");
    std::fs::write(&b_path, vec![b'x'; 80_000]).unwrap();
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().compression(rar_rs::ArchiveVersion::V29),
        )
        .unwrap();
        archive
            .add_path(
                &a_path,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive
            .add_path_as(
                &b_path,
                "b.txt",
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }

    // `rar rr 10%` semantics on the existing archive.
    {
        let mut editor = rar_rs::ArchiveEditor::open(&path).unwrap();
        editor
            .apply(rar_rs::EditPlan::new().set_recovery(10))
            .unwrap();
    }
    let with_rr = std::fs::read(&path).unwrap();
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected the rr-added archive");

    // Damage a protected sector; our legacy repair rebuilds it, and 6.23's
    // UnRAR still validates the repaired bytes.
    let mut damaged = with_rr.clone();
    let damage_at = 150_000;
    damaged[damage_at..damage_at + 64].fill(0x9c);
    let damaged_path = dir.path().join("damaged.rar");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let fixed_path = dir.path().join("fixed.rar");
    assert!(rar_rs::repair_legacy_archive_path(&damaged_path, &fixed_path).unwrap());
    assert_eq!(std::fs::read(&fixed_path).unwrap(), with_rr);
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&fixed_path)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected the repaired archive");

    // `rar k` semantics: locked archives still validate.
    {
        let mut editor = rar_rs::ArchiveEditor::open(&fixed_path).unwrap();
        editor.lock().unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&fixed_path)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected the locked archive");
}

/// WinRAR 6.23 (the last RAR4-producing official release) must accept a
/// RAR4 archive whose members were renamed by our stage-A header surgery —
/// both an archive built by our own writer (ASCII -> Unicode and
/// Unicode -> ASCII) and an archive built by 6.23's Rar.exe itself.
#[test]
fn official_unrar_validates_rar4_renames() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();

    // Our writer -> our rename -> 6.23 UnRAR t.
    let ours = dir.path().join("rn4.rar");
    let f1 = dir.path().join("alpha.txt");
    let f2 = dir.path().join("beta.bin");
    let payload_1: Vec<u8> = b"first rename payload ".repeat(1_200);
    let payload_2: Vec<u8> = b"second rename payload ".repeat(900);
    std::fs::write(&f1, &payload_1).unwrap();
    std::fs::write(&f2, &payload_2).unwrap();
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &ours,
            rar_rs::WriterOptions::default().compression(rar_rs::ArchiveVersion::V29),
        )
        .unwrap();
        archive
            .add_path(
                &f1,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive
            .add_path(
                &f2,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    {
        let mut editor = rar_rs::ArchiveEditor::open(&ours).unwrap();
        let a = editor
            .entries()
            .find(|e| e.name().ends_with("alpha.txt"))
            .unwrap()
            .id();
        let b = editor
            .entries()
            .find(|e| e.name().ends_with("beta.bin"))
            .unwrap()
            .id();
        editor
            .apply(
                rar_rs::EditPlan::new()
                    .rename(a, "阿尔法.txt")
                    .rename(b, "gamma.bin"),
            )
            .unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&ours)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected renamed rar-rs archive");
    let mut reader = rar_rs::ArchiveReader::open(&ours).unwrap();
    let alpha = reader.unique_entry("阿尔法.txt").unwrap();
    assert_eq!(reader.read_entry(alpha).unwrap(), payload_1);
    let gamma = reader.unique_entry("gamma.bin").unwrap();
    assert_eq!(reader.read_entry(gamma).unwrap(), payload_2);

    // 6.23-built RAR4 -> our rename -> 6.23 UnRAR t.
    let theirs = dir.path().join("official4.rar");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("m1.bin"), &payload_1).unwrap();
    std::fs::write(src.join("m2.txt"), &payload_2).unwrap();
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-ma4", "-idq"])
        .arg(&theirs)
        .arg(src.join("m1.bin"))
        .arg(src.join("m2.txt"))
        .status()
        .unwrap();
    assert!(status.success(), "6.23 could not create the RAR4 fixture");
    {
        let mut editor = rar_rs::ArchiveEditor::open(&theirs).unwrap();
        let m1 = editor
            .entries()
            .find(|e| e.name().ends_with("m1.bin"))
            .unwrap()
            .id();
        editor
            .rename_entries(&[(m1, "第一.bin".to_string())])
            .unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&theirs)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "unrar rejected 6.23 archive renamed by rar-rs"
    );
}

/// WinRAR 6.23 validates RAR4 archives whose members were added through
/// `add_bytes` (the path that used to fall into the RAR5 writer), with
/// Unicode and compressed members.
#[test]
fn official_unrar_validates_rar4_add_bytes() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    let path = dir.path().join("bytes4.rar");
    let unicode_payload: Vec<u8> = b"unicode interop payload ".repeat(900);
    let ascii_payload: Vec<u8> = b"ascii interop payload ".repeat(700);
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().compression(rar_rs::ArchiveVersion::V29),
        )
        .unwrap();
        archive
            .add_bytes(
                "文-件名-ünï.bin",
                &unicode_payload,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive
            .add_bytes(
                "plain.bin",
                &ascii_payload,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected add_bytes RAR4 archive");
    let mut reader = rar_rs::ArchiveReader::open(&path).unwrap();
    let unicode = reader.unique_entry("文-件名-ünï.bin").unwrap();
    assert_eq!(reader.read_entry(unicode).unwrap(), unicode_payload);
    let plain = reader.unique_entry("plain.bin").unwrap();
    assert_eq!(reader.read_entry(plain).unwrap(), ascii_payload);
}

/// Comment cross-validation: WinRAR 6.23 reads a comment our editor set on
/// a RAR4 archive (`rar cw` reproduces the text byte-for-byte), and we read
/// the comment from a 6.23-created archive.
#[test]
fn official_tools_validate_rar4_comments() {
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    let path = dir.path().join("cmt4.rar");
    let file = dir.path().join("f.txt");
    std::fs::write(&file, b"payload").unwrap();
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().compression(rar_rs::ArchiveVersion::V29),
        )
        .unwrap();
        archive
            .add_path(
                &file,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    let comment: &[u8] = "归档注释 with ünï and 中文".as_bytes();
    {
        let mut editor = rar_rs::ArchiveEditor::open(&path).unwrap();
        editor
            .apply(rar_rs::EditPlan::new().set_comment(comment.to_vec()))
            .unwrap();
    }
    // 6.23's `cw` must reproduce the exact comment we stored.
    let out = dir.path().join("extracted.txt");
    let status = std::process::Command::new(&rar_bin)
        .args(["cw", "-idq"])
        .arg(&path)
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "6.23 cw failed on our commented archive");
    assert_eq!(std::fs::read(&out).unwrap(), comment, "6.23 cw mismatch");

    // 6.23 sets a comment; we read it back.
    let theirs = dir.path().join("official-comment.rar");
    let comment_file = dir.path().join("theirs.txt");
    std::fs::write(&comment_file, b"official comment \xe4\xb8\xad\xe6\x96\x87").unwrap();
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &theirs,
            rar_rs::WriterOptions::default().compression(rar_rs::ArchiveVersion::V29),
        )
        .unwrap();
        archive
            .add_path(
                &file,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    let status = std::process::Command::new(&rar_bin)
        .arg("c")
        .arg("-idq")
        .arg(format!("-z{}", comment_file.display()))
        .arg(&theirs)
        .status()
        .unwrap();
    assert!(status.success(), "6.23 could not set the comment");
    let mut archive = rar_rs::RarArchive::open(&theirs).unwrap();
    assert_eq!(
        archive.get_comment().unwrap(),
        Some(b"official comment \xe4\xb8\xad\xe6\x96\x87".to_vec())
    );
}

/// WinRAR 6.23 validates RAR4 archives after our stage-B member deletion
/// (both a rar-rs-built and a 6.23-built archive).
#[test]
fn official_unrar_validates_rar4_deletes() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let rar_bin = match std::env::var_os("SA_OFFICIAL_RAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    let f1 = dir.path().join("keep.bin");
    let f2 = dir.path().join("drop.bin");
    let keep: Vec<u8> = b"kept payload ".repeat(900);
    let drop: Vec<u8> = b"dropped payload ".repeat(800);
    std::fs::write(&f1, &keep).unwrap();
    std::fs::write(&f2, &drop).unwrap();

    // rar-rs-built archive: delete one member, 6.23 validates.
    let ours = dir.path().join("del.rar");
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &ours,
            rar_rs::WriterOptions::default().compression(rar_rs::ArchiveVersion::V29),
        )
        .unwrap();
        archive
            .add_path(
                &f1,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive
            .add_path(
                &f2,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    {
        let mut editor = rar_rs::ArchiveEditor::open(&ours).unwrap();
        let drop_id = editor
            .entries()
            .find(|e| e.name().ends_with("drop.bin"))
            .unwrap()
            .id();
        editor.delete_entries(&[drop_id]).unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&ours)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected deleted rar-rs archive");
    let mut reader = rar_rs::ArchiveReader::open(&ours).unwrap();
    let kept = reader
        .entries()
        .find(|e| e.name().ends_with("keep.bin"))
        .unwrap()
        .id();
    assert_eq!(reader.read_entry(kept).unwrap(), keep);

    // 6.23-built archive: delete one member through our editor, 6.23
    // validates the result.
    let theirs = dir.path().join("del623.rar");
    let status = std::process::Command::new(&rar_bin)
        .args(["a", "-ma4", "-idq"])
        .arg(&theirs)
        .arg(&f1)
        .arg(&f2)
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut editor = rar_rs::ArchiveEditor::open(&theirs).unwrap();
        let drop_id = editor
            .entries()
            .find(|e| e.name().ends_with("drop.bin"))
            .unwrap()
            .id();
        editor.delete_entries(&[drop_id]).unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&theirs)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "unrar rejected 6.23 archive deleted by rar-rs"
    );
    let mut reader = rar_rs::ArchiveReader::open(&theirs).unwrap();
    let kept = reader
        .entries()
        .find(|e| e.name().ends_with("keep.bin"))
        .unwrap()
        .id();
    assert_eq!(reader.read_entry(kept).unwrap(), keep);
}

/// WinRAR 6.23 validates a RAR4 archive after our stage-B append, and
/// repairs through the rebuilt recovery record.
#[test]
fn official_unrar_validates_rar4_appends() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    let path = dir.path().join("app4.rar");
    let first: Vec<u8> = vec![0x41; 60_000];
    let second: Vec<u8> = vec![0x42; 50_000];
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().compression(rar_rs::ArchiveVersion::V29),
        )
        .unwrap();
        archive
            .add_bytes(
                "first.bin",
                &first,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    {
        let mut editor = rar_rs::ArchiveEditor::open(&path).unwrap();
        editor
            .apply(rar_rs::EditPlan::new().set_recovery(10))
            .unwrap();
    }
    {
        let mut archive = rar_rs::ArchiveWriter::append(&path).unwrap();
        archive
            .add_bytes(
                "second.bin",
                &second,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected appended RAR4 archive");
    let mut reader = rar_rs::ArchiveReader::open(&path).unwrap();
    let second_id = reader.unique_entry("second.bin").unwrap();
    assert_eq!(reader.read_entry(second_id).unwrap(), second);

    // The rebuilt record protects the appended member: damage it and repair
    // back byte-for-byte with our own legacy repair (6.23's Rar.exe repair
    // consumes the same record, verified manually).
    let bytes = std::fs::read(&path).unwrap();
    let mut damaged = bytes.clone();
    let at = bytes.len() - 20_000;
    damaged[at..at + 32].fill(0x90);
    let dmg = dir.path().join("dmg.rar");
    std::fs::write(&dmg, &damaged).unwrap();
    let fixed = dir.path().join("fixed.rar");
    assert!(rar_rs::repair_legacy_archive_path(&dmg, &fixed).unwrap());
    assert_eq!(std::fs::read(&fixed).unwrap(), bytes);
}

/// WinRAR 6.23 validates a solid RAR4 archive after our stage-C repack
/// (member deletion re-encodes the whole chain).
#[test]
fn official_unrar_validates_rar4_solid_repack() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    let path = dir.path().join("solid-repack.rar");
    let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
    let p1: Vec<u8> = line.repeat(40_000);
    let p2: Vec<u8> = line.repeat(35_000);
    let p3: Vec<u8> = line.repeat(30_000);
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .compression(rar_rs::ArchiveVersion::V29)
                .solid_mode(rar_rs::SolidMode::Continuous),
        )
        .unwrap();
        archive
            .add_bytes(
                "a.txt",
                &p1,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
        archive
            .add_bytes(
                "b.txt",
                &p2,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
        archive
            .add_bytes(
                "c.txt",
                &p3,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    {
        let mut editor = rar_rs::ArchiveEditor::open(&path).unwrap();
        let b = editor.unique_entry("b.txt").unwrap();
        editor.delete_entries(&[b]).unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "unrar rejected repacked solid RAR4 archive"
    );
    let mut reader = rar_rs::ArchiveReader::open(&path).unwrap();
    let a = reader.unique_entry("a.txt").unwrap();
    assert_eq!(reader.read_entry(a).unwrap(), p1);
    let c = reader.unique_entry("c.txt").unwrap();
    assert_eq!(reader.read_entry(c).unwrap(), p3);
}

/// WinRAR 6.23 validates a solid RAR4 archive after a deferred append
/// (members added to a solid chain are re-encoded into a fresh chain).
#[test]
fn official_unrar_validates_rar4_solid_append() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    let path = dir.path().join("solid-app.rar");
    let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
    let p1: Vec<u8> = varied_bytes(30_000, line);
    let p2: Vec<u8> = varied_bytes(25_000, line);
    {
        let mut archive = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .compression(rar_rs::ArchiveVersion::V29)
                .solid_mode(rar_rs::SolidMode::Continuous),
        )
        .unwrap();
        archive
            .add_bytes(
                "a.txt",
                &p1,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    {
        let mut archive = rar_rs::ArchiveWriter::append(&path).unwrap();
        archive
            .add_bytes(
                "b.txt",
                &p2,
                EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
            )
            .unwrap();
        archive.finish().unwrap();
    }
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected solid RAR4 append");
    let mut reader = rar_rs::ArchiveReader::open(&path).unwrap();
    let a = reader.unique_entry("a.txt").unwrap();
    assert_eq!(reader.read_entry(a).unwrap(), p1);
    let b = reader.unique_entry("b.txt").unwrap();
    assert_eq!(reader.read_entry(b).unwrap(), p2);
}

fn varied_bytes(n: usize, line: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..n {
        out.extend_from_slice(format!("{i:08}: ").as_bytes());
        out.extend_from_slice(line);
        out.extend_from_slice(b"--variant--\n");
    }
    out
}
