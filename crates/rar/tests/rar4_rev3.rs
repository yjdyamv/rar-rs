//! Legacy RAR 1.5–4.x recovery volumes (`.rev`): build (`rv`) and rebuild
//! (`rc`) round trips over our own multi-volume sets plus the RAR 3.00 /
//! 4.20 fixtures from the `rars` corpus (both legacy and trailer layouts).

use std::fs;
use std::path::{Path, PathBuf};

use rar_rs::{ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rar40/rev3/");

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
    let path = dir.join(format!("{name}.rar"));
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .compression(ArchiveVersion::V29)
            .volume_size(volume_size),
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
    // Our volumes end with a live ENDARC, so WinRAR's choice (and ours) is
    // the legacy full-parity layout with the counts in the file name.
    let expected = format!("set{}_1_1.rev", volumes.len());
    assert_eq!(volume_name(&revs[0]), expected);

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
        vec![
            format!("pair{}_2_1.rev", volumes.len()),
            format!("pair{}_2_2.rev", volumes.len())
        ]
    );

    fs::remove_file(&volumes[0]).unwrap();
    fs::remove_file(&volumes[2]).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&volumes[1]).unwrap();
    assert_eq!(rebuilt.len(), 2);
    assert_eq!(fs::read(&volumes[0]).unwrap(), originals[0]);
    assert_eq!(fs::read(&volumes[2]).unwrap(), originals[2]);
}

#[test]
fn trailer_layout_builds_and_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = create_set(dir.path(), "pad", 64 * 1024);
    // Give every volume WinRAR's zero tail (its 20-byte ENDARC layout ends
    // in eight zero bytes), which selects the trailer layout.
    for path in &volumes {
        let mut bytes = fs::read(path).unwrap();
        bytes.extend_from_slice(&[0u8; 7]);
        fs::write(path, &bytes).unwrap();
    }
    let originals: Vec<Vec<u8>> = volumes.iter().map(|p| fs::read(p).unwrap()).collect();

    let revs = rar_rs::build_recovery_volumes_for_set(&volumes, 1).unwrap();
    assert_eq!(
        revs.iter().map(|p| volume_name(p)).collect::<Vec<_>>(),
        vec!["pad1.rev".to_string()]
    );
    for rev in &revs {
        let bytes = fs::read(rev).unwrap();
        assert_eq!(bytes.len(), originals[0].len());
    }

    // Missing middle: the zero tail is preserved exactly.
    fs::remove_file(&volumes[1]).unwrap();
    rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    assert_eq!(fs::read(&volumes[1]).unwrap(), originals[1]);

    // Missing last: the rebuilt volume is truncated back to its live
    // `ENDARC` block (the synthetic seven-byte pad is not part of it).
    fs::remove_file(&volumes[3]).unwrap();
    rar_rs::rebuild_missing_volumes(&volumes[0]).unwrap();
    let rebuilt = fs::read(&volumes[3]).unwrap();
    let endarc = originals[3].len() - 7;
    assert_eq!(rebuilt, originals[3][..endarc]);
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
    let bad = victim.with_extension("r00.bad");
    assert_eq!(fs::read(&bad).unwrap(), damaged);
}

#[test]
fn rar300_fixtures_rebuild_from_both_layouts() {
    for (set, rev, victim, expected) in [
        (
            "oldstyle",
            "rev_oldstyle.part4_2_1.rev",
            1usize,
            "rev_oldstyle.part2.rar",
        ),
        (
            "newstyle",
            "rev_newstyle.part1.rev",
            1usize,
            "rev_newstyle.part2.rar",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        for entry in fs::read_dir(FIX).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("rev_{set}.")) {
                fs::copy(entry.path(), dir.path().join(&name)).unwrap();
            }
        }
        let rev_path = dir.path().join(rev);
        assert!(rev_path.exists(), "{set}: {rev} fixture missing");
        let volumes: Vec<PathBuf> = (1..=4)
            .map(|index| dir.path().join(format!("rev_{set}.part{index}.rar")))
            .collect();
        let expected_bytes = fs::read(dir.path().join(expected)).unwrap();
        fs::remove_file(&volumes[victim]).unwrap();

        let rebuilt = rar_rs::rebuild_missing_volumes(&rev_path).unwrap();
        assert_eq!(rebuilt, vec![volumes[victim].clone()], "{set}");
        assert_eq!(
            fs::read(&volumes[victim]).unwrap(),
            expected_bytes,
            "{set}: rebuilt volume bytes"
        );
    }
}
