use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, SolidMode, WriterOptions,
};

use crate::support::{file_sha256, rar_bin, run, temp_dir, unrar_bin, unrar_extract, unrar_test};

// ── Solid multi-volume (WinRAR 7.23 aligned) ────────────────────────────────

/// Solid + multi-volume creation: the LZ window carries across volume
/// boundaries; WinRAR must be able to test/extract both directions.
#[test]
fn solid_multivolume_interops_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("rand8.bin");
    let mut data = vec![0u8; 8 * 1024 * 1024];
    for chunk in data.chunks_mut(4096) {
        let mut seed = (chunk.as_ptr() as usize) as u64;
        for b in chunk.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 33) as u8;
        }
    }
    std::fs::write(&src, &data).unwrap();
    let small = dir.path().join("s.txt");
    std::fs::write(&small, b"solid volume second member ".repeat(500)).unwrap();

    // Ours -> WinRAR.
    let ours = dir.path().join("ours_sv.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &ours,
            WriterOptions::default()
                .solid_mode(SolidMode::Continuous)
                .volume_size(2 * 1024 * 1024),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.add_path(
            &small,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&ours);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "WinRAR rejected our solid volume set:\n{out}");
        let win = dir.path().join("win_ours_sv");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, None);
        assert!(ok, "WinRAR failed to extract our solid volume set:\n{out}");
        assert_eq!(file_sha256(&win.join("rand8.bin")), file_sha256(&src));
    }

    // WinRAR -> ours.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_sv.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-v2m", "-idq"])
            .arg(&theirs)
            .arg(&src)
            .arg(&small));
        assert!(ok, "WinRAR solid volumes failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&theirs);
        assert!(
            volumes.len() >= 3,
            "expected several volumes, got {}",
            volumes.len()
        );
        let mut ar = ArchiveReader::open(&volumes[0]).unwrap();
        let names: Vec<String> = ar.entries().map(|e| e.name().to_string()).collect();
        let bin_name = names
            .iter()
            .find(|n| n.ends_with("rand8.bin"))
            .unwrap_or_else(|| panic!("rand8.bin not found in {names:?}"))
            .clone();
        let data = ar.read_entry(ar.unique_entry(&bin_name).unwrap()).unwrap();
        assert_eq!(data, std::fs::read(&src).unwrap());
    }
}

/// Solid chain with a filtered (delta/x86) member at the boundary: both
/// directions. A filtered member must be written standalone (non-solid)
/// even inside a solid archive, so the neighbours must still decode
/// byte-for-byte.
#[test]
fn solid_chain_with_filtered_boundary_interops() {
    let dir = temp_dir();
    let code = dir.path().join("lib.dll");
    // Synthetic x86-ish code with E8/E8E9 patterns so our auto-x86 filter
    // (and WinRAR's) fires.
    let mut dll = Vec::with_capacity(2 * 1024 * 1024);
    let mut x = 0x1234_5678u32;
    while dll.len() < 2 * 1024 * 1024 {
        x = x.wrapping_mul(2654435761).wrapping_add(0x9E37_79B9);
        dll.push((x & 0xFF) as u8);
        if dll.len() % 37 == 0 {
            dll.push(0xE8); // CALL rel32
            dll.extend_from_slice(&(0x0010_2000u32).to_le_bytes());
        }
    }
    std::fs::write(&code, &dll).unwrap();
    let text = dir.path().join("doc.txt");
    std::fs::write(
        &text,
        b"plain text neighbour in the solid chain ".repeat(40_000),
    )
    .unwrap();

    // WinRAR -> ours: -s with mixed code + text.
    if let Some(rar) = rar_bin() {
        let arc = dir.path().join("win_solid.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-m5", "-idq"])
            .arg(&arc)
            .arg("lib.dll")
            .arg("doc.txt")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -s mixed failed:\n{out}");
        let mut ar = ArchiveReader::open(&arc).unwrap();
        let names: Vec<String> = ar.entries().map(|e| e.name().to_string()).collect();
        let code_name = names
            .iter()
            .find(|n| n.ends_with("lib.dll"))
            .unwrap()
            .clone();
        let text_name = names
            .iter()
            .find(|n| n.ends_with("doc.txt"))
            .unwrap()
            .clone();
        assert_eq!(
            ar.read_entry(ar.unique_entry(&code_name).unwrap()).unwrap(),
            std::fs::read(&code).unwrap(),
            "rar-rs read a different dll from WinRAR's solid archive"
        );
        assert_eq!(
            ar.read_entry(ar.unique_entry(&text_name).unwrap()).unwrap(),
            std::fs::read(&text).unwrap(),
            "rar-rs read a different txt from WinRAR's solid archive"
        );
    }

    // Ours -> WinRAR: -s with a filtered (.dll) member.
    let arc = dir.path().join("ours_solid.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default().solid_mode(SolidMode::Continuous),
        )
        .unwrap();
        rar.add_path(
            &code,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(5u8).unwrap()),
        )
        .unwrap();
        rar.add_path(
            &text,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(5u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&arc, None);
        assert!(
            ok,
            "UnRAR rejected our solid archive with a filtered member:\n{out}"
        );
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&arc, &dest, None);
        assert!(ok, "UnRAR failed to extract our solid archive:\n{out}");
        let code_out = std::fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().ends_with("lib.dll"))
            .unwrap()
            .path();
        assert_eq!(
            file_sha256(&code_out),
            file_sha256(&code),
            "WinRAR extracted a different dll from our solid archive"
        );
    }
}

/// Solid chain split modifiers (`-sv` / `-se`) interoperate with WinRAR in
/// both directions. `-sv` resets the solid statistics at every volume
/// boundary; `-se` resets them when the file extension changes.
#[test]
fn solid_reset_volume_interops_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("rand8.bin");
    let mut data = vec![0u8; 8 * 1024 * 1024];
    for chunk in data.chunks_mut(4096) {
        let mut seed = (chunk.as_ptr() as usize) as u64;
        for b in chunk.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 33) as u8;
        }
    }
    std::fs::write(&src, &data).unwrap();
    let small = dir.path().join("s.txt");
    std::fs::write(&small, b"solid volume second member ".repeat(500)).unwrap();

    // WinRAR -> ours: -s -sv multi-volume.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_sv.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-sv", "-v2m", "-idq"])
            .arg(&theirs)
            .arg("rand8.bin")
            .arg("s.txt")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -s -sv failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&theirs);
        assert!(
            volumes.len() >= 3,
            "expected several volumes, got {}",
            volumes.len()
        );
        let mut ar = ArchiveReader::open(&volumes[0]).unwrap();
        let names: Vec<String> = ar.entries().map(|e| e.name().to_string()).collect();
        let bin = names
            .iter()
            .find(|n| n.ends_with("rand8.bin"))
            .unwrap()
            .clone();
        assert_eq!(
            ar.read_entry(ar.unique_entry(&bin).unwrap()).unwrap(),
            std::fs::read(&src).unwrap()
        );
    }

    // Ours -> WinRAR: -sv multi-volume.
    let ours = dir.path().join("ours_sv.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &ours,
            WriterOptions::default()
                .solid_mode(SolidMode::PerVolume)
                .volume_size(2 * 1024 * 1024),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.add_path(
            &small,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&ours);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected our -sv volume set:\n{out}");
        let win = dir.path().join("win_ours_sv");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, None);
        assert!(ok, "UnRAR failed to extract our -sv volume set:\n{out}");
        assert_eq!(file_sha256(&win.join("rand8.bin")), file_sha256(&src));
    }
}

#[test]
fn solid_reset_extension_interops_with_winrar() {
    let dir = temp_dir();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.bin");
    let c = dir.path().join("c.txt");
    std::fs::write(&a, b"alpha text block ".repeat(20_000)).unwrap();
    std::fs::write(&b, vec![0xABu8; 1_000_000]).unwrap();
    std::fs::write(&c, b"gamma text block ".repeat(20_000)).unwrap();

    // WinRAR -> ours: -s -se multi-volume (groups reset on extension).
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_se.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-se", "-v1m", "-idq"])
            .arg(&theirs)
            .arg("a.txt")
            .arg("b.bin")
            .arg("c.txt")
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -s -se failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&theirs);
        let mut ar = ArchiveReader::open(&volumes[0]).unwrap();
        let names: Vec<String> = ar.entries().map(|e| e.name().to_string()).collect();
        for (name, src) in [("a.txt", &a), ("b.bin", &b), ("c.txt", &c)] {
            let n = names.iter().find(|m| m.ends_with(name)).unwrap().clone();
            assert_eq!(
                ar.read_entry(ar.unique_entry(&n).unwrap()).unwrap(),
                std::fs::read(src).unwrap(),
                "rar-rs read a different {name} from WinRAR's -se archive"
            );
        }
    }

    // Ours -> WinRAR: -se (reset the solid chain on an extension change;
    // input order is preserved, we no longer sort by extension).
    let ours = dir.path().join("ours_se.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &ours,
            WriterOptions::default()
                .solid_mode(SolidMode::PerExtension)
                .volume_size(1024 * 1024),
        )
        .unwrap();
        rar.add_path(
            &a,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.add_path(
            &b,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.add_path(
            &c,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&ours);
    // Order must follow the call sequence (a.txt, b.bin, c.txt): -se resets
    // the solid chain on an extension change but must not reorder members by
    // extension, which would diverge from WinRAR.
    {
        let ar = ArchiveReader::open(&volumes[0]).unwrap();
        assert_eq!(
            ar.entries()
                .map(|e| e.name().to_string())
                .collect::<Vec<String>>(),
            vec!["a.txt", "b.bin", "c.txt"],
            "-se must preserve input order, not sort by extension"
        );
    }
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected our -se archive:\n{out}");
        let win = dir.path().join("win_ours_se");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, None);
        assert!(ok, "UnRAR failed to extract our -se archive:\n{out}");
        for (name, src) in [("a.txt", &a), ("b.bin", &b), ("c.txt", &c)] {
            assert_eq!(
                file_sha256(&win.join(name)),
                file_sha256(src),
                "WinRAR extracted a different {name} from our -se archive"
            );
        }
    }
}
