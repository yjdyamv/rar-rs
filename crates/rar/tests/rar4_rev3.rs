//! Legacy RAR 1.5–4.x recovery volumes (`.rev`): build (`rv`) and rebuild
//! (`rc`) round trips over our own multi-volume sets plus recovery sets that
//! WinRAR 3.00 / 3.93 / 4.20 generate on demand, covering all three naming
//! shapes ever written: `<base>4_3_N.rev`, `<base>N.rev` and the modern
//! `.partN.rev` trailer layout.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use std::fs;
use std::path::{Path, PathBuf};

use rar_rs::{ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

use support::corpus;

fn payload() -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    (0..200_000)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 16) as u8
        })
        .collect()
}

fn create_set(dir: &Path, name: &str, volume_size: u64) -> Vec<PathBuf> {
    create_set_with(dir, name, volume_size, false)
}

/// [`create_set`] with WinRAR's `-vn` old volume naming (`.rar`/`.rNN`)
/// when `old` is set, instead of the default `.partNN.rar` scheme.
fn create_set_with(dir: &Path, name: &str, volume_size: u64, old: bool) -> Vec<PathBuf> {
    let path = dir.join(format!("{name}.rar"));
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .compression(ArchiveVersion::V29)
            .volume_size(volume_size)
            .old_numbering(old),
    )
    .unwrap();
    writer
        .add_bytes(
            "big.bin",
            &payload(),
            EntryWriteOptions::new().compression_level(CompressionLevel::STORE),
        )
        .unwrap();
    writer.finish().unwrap();
    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() >= 4, "{name}: {} volumes", volumes.len());
    volumes
}

fn volume_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

#[test]
fn legacy_build_and_rebuild_each_missing_volume() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = create_set(dir.path(), "set", 64 * 1024);
    let originals: Vec<Vec<u8>> = volumes.iter().map(|p| fs::read(p).unwrap()).collect();

    let revs = rar_rs::build_recovery_volumes_for_set(&volumes, 1).unwrap();
    assert_eq!(revs.len(), 1);
    // Our volumes end with WinRAR's 20-byte `ENDARC` (a zero tail), so the
    // modern `.partNN.rev` trailer layout is chosen, matching WinRAR.
    assert_eq!(volume_name(&revs[0]), "set.part1.rev");

    for victim in [0usize, 1, volumes.len() - 1] {
        let saved = originals[victim].clone();
        fs::remove_file(&volumes[victim]).unwrap();
        let probe = volumes
            .iter()
            .find(|path| path.exists())
            .expect("a surviving volume");
        let rebuilt = rar_rs::rebuild_missing_volumes(probe).unwrap();
        assert_eq!(rebuilt, vec![volumes[victim].clone()], "victim {victim}");
        assert_eq!(
            fs::read(&volumes[victim]).unwrap(),
            saved,
            "victim {victim} bytes"
        );
        fs::write(&volumes[victim], &saved).unwrap();
    }

    // Nothing missing: no rebuild and no new files.
    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert!(rebuilt.is_empty());
}

#[test]
fn two_missing_volumes_with_two_recovery_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = create_set(dir.path(), "pair", 64 * 1024);
    let originals: Vec<Vec<u8>> = volumes.iter().map(|p| fs::read(p).unwrap()).collect();

    let revs = rar_rs::build_recovery_volumes_for_set(&volumes, 2).unwrap();
    assert_eq!(
        revs.iter().map(|p| volume_name(p)).collect::<Vec<_>>(),
        vec!["pair.part1.rev", "pair.part2.rev"]
    );

    fs::remove_file(&volumes[0]).unwrap();
    fs::remove_file(&volumes[2]).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[1]).unwrap();
    assert_eq!(rebuilt.len(), 2);
    assert_eq!(fs::read(&volumes[0]).unwrap(), originals[0]);
    assert_eq!(fs::read(&volumes[2]).unwrap(), originals[2]);
}

#[test]
fn old_numbering_layout_builds_and_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    // `-vn`: old-style `.rar`/`.rNN` volume names with `{base}N.rev` trailer
    // recovery files (WinRAR's `-vn` output shape).
    let volumes = create_set_with(dir.path(), "vn", 64 * 1024, true);
    assert_eq!(volume_name(&volumes[0]), "vn.rar");
    assert_eq!(volume_name(&volumes[1]), "vn.r00");
    let originals: Vec<Vec<u8>> = volumes.iter().map(|p| fs::read(p).unwrap()).collect();

    let revs = rar_rs::build_recovery_volumes_for_set(&volumes, 1).unwrap();
    assert_eq!(
        revs.iter().map(|p| volume_name(p)).collect::<Vec<_>>(),
        vec!["vn1.rev".to_string()]
    );
    for rev in &revs {
        let bytes = fs::read(rev).unwrap();
        assert_eq!(bytes.len(), originals[0].len());
    }

    // Missing middle: rebuilds byte-identically from the trailer parity.
    fs::remove_file(&volumes[1]).unwrap();
    rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(fs::read(&volumes[1]).unwrap(), originals[1]);

    // Missing last: the rebuilt volume keeps its own 20-byte `ENDARC`.
    fs::remove_file(&volumes[3]).unwrap();
    rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(fs::read(&volumes[3]).unwrap(), originals[3]);
}

#[test]
fn damaged_volume_is_rebuilt_and_kept_as_bad() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = create_set(dir.path(), "dmg", 64 * 1024);
    let originals: Vec<Vec<u8>> = volumes.iter().map(|p| fs::read(p).unwrap()).collect();
    // Two recovery volumes: one error position is locatable with two
    // syndrome symbols (floor(recovery_count / 2) unknown errors).
    rar_rs::build_recovery_volumes_for_set(&volumes, 2).unwrap();

    let victim = &volumes[1];
    let mut damaged = originals[1].clone();
    damaged[100] ^= 0xff;
    fs::write(victim, &damaged).unwrap();

    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(rebuilt, vec![victim.clone()]);
    assert_eq!(fs::read(victim).unwrap(), originals[1]);
    // The damaged original is parked as its `*.bad` sibling
    // (`dmg.part2.rar` → `dmg.part2.rar.bad`).
    let bad = dir.path().join(format!("{}.bad", volume_name(victim)));
    assert_eq!(fs::read(&bad).unwrap(), damaged);
}

#[test]
fn winrar_recovery_layouts_rebuild_a_missing_volume() {
    // Both layouts WinRAR ever wrote: 3.00's `<base>4_3_N.rev` next to an
    // old-numbered set, and 4.20's `set.partN.rev` trailer next to `.partN.rar`.
    for (layout, made, rev_prefix) in [
        ("oldstyle", corpus::rev3_oldstyle(), "arc4_3_"),
        ("late", corpus::rev3_oldstyle_late(), "arc1."),
        ("newstyle", corpus::rev3_newstyle(), "arc.part"),
    ] {
        let Some(made) = made else { continue };
        let revs: Vec<PathBuf> = made
            .files
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "rev"))
            .cloned()
            .collect();
        assert!(!revs.is_empty(), "{layout}: no .rev volumes generated");
        assert!(
            revs.iter().any(|p| p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with(rev_prefix)),
            "{layout}: unexpected rev names {:?}",
            made.names()
        );

        // Copy the whole set to a scratch dir so deleting a volume does not
        // disturb the cached generation.
        let dir = tempfile::tempdir().unwrap();
        let mut volumes: Vec<PathBuf> = Vec::new();
        for path in &made.files {
            let name = path.file_name().unwrap();
            let copy = dir.path().join(name);
            fs::copy(path, &copy).unwrap();
            let lower = name.to_string_lossy().to_lowercase();
            let ext = lower.rsplit('.').next().unwrap_or_default();
            let legacy_part = ext
                .strip_prefix('r')
                .is_some_and(|tail| tail.len() == 2 && tail.chars().all(|c| c.is_ascii_digit()));
            if ext == "rar" || legacy_part {
                volumes.push(copy);
            }
        }
        volumes.sort();
        assert!(volumes.len() >= 4, "{layout}: {} volumes", volumes.len());

        let victim = &volumes[1];
        let expected = fs::read(victim).unwrap();
        let rev = dir.path().join(revs[0].file_name().unwrap());
        fs::remove_file(victim).unwrap();

        let rebuilt = rar_rs::rebuild_missing_volumes(&rev).unwrap();
        assert_eq!(rebuilt, vec![victim.clone()], "{layout}");
        assert_eq!(
            fs::read(victim).unwrap(),
            expected,
            "{layout}: rebuilt volume bytes"
        );
    }
}

/// Legacy sets created with WinRAR's new `.partN.rar` numbering carry
/// MHD_NEWNUMBERING, which the listing uses for the `volume N` annotation.
#[test]
fn legacy_new_numbering_sets_are_reported() {
    let Some(made) = corpus::rev3_newstyle() else {
        return;
    };
    let second = made
        .files
        .iter()
        .find(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .ends_with("part2.rar")
        })
        .expect("second volume");
    let rar = rar_rs::ArchiveReader::open(second).unwrap();
    assert!(rar.is_new_numbering());
    assert!(rar.entries().count() >= 1);

    // The legacy layout reports the old numbering instead.
    let Some(old) = corpus::rev3_oldstyle() else {
        return;
    };
    let rar = rar_rs::ArchiveReader::open(old.path()).unwrap();
    assert!(!rar.is_new_numbering());
}
