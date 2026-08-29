//! Cross-validation against the official rar/unrar console tools (env-gated via SA_OFFICIAL_RAR / SA_OFFICIAL_UNRAR).

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar5::RarArchive;

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
            let mut ar = rar5::RarArchive::create_with_options(
                &rr,
                rar5::CreateOptions {
                    recovery_percent: Some(10),
                    ..Default::default()
                },
            )
            .unwrap();
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
            let mut ar = rar5::RarArchive::create_with_options(
                &multi,
                rar5::CreateOptions {
                    volume_size: Some(1024 * 1000),
                    recovery_volume_count: Some(2),
                    ..Default::default()
                },
            )
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
            let mut rar =
                rar5::RarArchive::create_with_options(&path, rar5::CreateOptions::default())
                    .unwrap();
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
            rar5::RarArchive::create_with_options(&path, rar5::CreateOptions::default()).unwrap();
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
        let mut ar =
            rar5::RarArchive::create_with_options(&ours, rar5::CreateOptions::default()).unwrap();
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
        let mut ar =
            rar5::RarArchive::create_with_options(&ours, rar5::CreateOptions::default()).unwrap();
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
        let mut ar =
            rar5::RarArchive::create_with_options(&ours, rar5::CreateOptions::default()).unwrap();
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
