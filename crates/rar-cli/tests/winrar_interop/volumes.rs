use std::process::Command;

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

use crate::support::{
    file_sha256, file_sha256_bytes, rar_bin, run, temp_dir, unrar_bin, unrar_extract, unrar_test,
    write_pattern_file,
};

/// The writer zero-pads volume names for sets of 10+ volumes (like
/// WinRAR). WinRAR must test our padded set, our `rc` must rebuild a
/// deleted volume byte-identically, and both tools must read it back.
#[test]
fn our_padded_volume_sets_validate_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 600_000, 21); // STORE: ~12 x 50k volumes

    // Our 12-volume set: the writer emits part01..part12.
    let arc = dir.path().join("p.rar");
    {
        let mut rar =
            ArchiveWriter::create_with(&arc, WriterOptions::default().volume_size(50_000)).unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let first = dir.path().join("p.part01.rar");
    let volumes = rar_rs::discover_volumes(&first);
    assert!(
        volumes.len() >= 10,
        "precondition: >= 10 volumes so the writer zero-pads, got {}",
        volumes.len()
    );
    assert!(
        !dir.path().join("p.part1.rar").exists(),
        "the writer must not emit unpadded volume names"
    );

    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&first, None);
        assert!(ok, "UnRAR rejected our padded volume set:\n{out}");
    }

    // Our `rv` adds .rev files with the set's padding, then our `rc`
    // rebuilds a deleted padded volume byte-identically.
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .arg("rv")
        .arg(&first)
        .args(["2", "-idq"]));
    assert!(ok, "our rar rv failed on our padded set:\n{out}");
    assert!(
        dir.path().join("p.part01.rev").exists(),
        "our .rev names must follow the set's padding"
    );
    let victim = dir.path().join("p.part05.rar");
    let victim_bytes = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["rc", "-idq"])
        .arg(&first));
    assert!(ok, "our rar rc failed on our padded set:\n{out}");
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        victim_bytes,
        "our rc must rebuild our padded volume byte-identically"
    );

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

/// CLI `-sd` (dependent solid volumes: keep the solid statistics across
/// volume boundaries, disabling the per-volume reset) interoperates with
/// WinRAR in both directions. Exercises the CLI `-sd` switch end-to-end
/// (its `normalize_switch` path maps `-sd` to `--solid-reset=continuous`),
/// which the library-API solid tests do not touch.
#[test]
fn cli_sd_dependent_volumes_interops_with_winrar() {
    let dir = temp_dir();
    // A few compressible files each sharing a long common text prefix plus
    // a deterministic pseudo-random tail: the common prefix gives the solid
    // chain something to share across volume boundaries (the observable
    // effect of `-sd`), while the random tail keeps the set big enough to
    // split into several volumes. Deterministic LCG keeps it cross-platform.
    let mut files = Vec::new();
    let mut data = Vec::new();
    for i in 0..4u32 {
        let p = dir.path().join(format!("m{i}.dat"));
        let mut body = Vec::new();
        let prefix =
            format!("COMMONPREFIX record {i}: the quick brown fox jumps over the lazy dog.\n");
        for _ in 0..20_000 {
            body.extend_from_slice(prefix.as_bytes());
        }
        let mut seed = (i as u64)
            .wrapping_mul(0x9E3779B97F4A7C15)
            .wrapping_add(0x1234567);
        for _ in 0..400_000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            body.push((seed >> 33) as u8);
        }
        std::fs::write(&p, &body).unwrap();
        data.push((p.file_name().unwrap().to_string_lossy().into_owned(), body));
        files.push(p);
    }

    // Ours -> WinRAR: our own `rar` binary with `-s -sd` multi-volume.
    let ours = dir.path().join("cli_sd.rar");
    {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_rar"));
        cmd.args(["a", "-s", "-sd", "-idq", "-v100k"])
            .arg(&ours)
            .args(files.iter().map(|p| p.file_name().unwrap()))
            .current_dir(dir.path());
        let (ok, out) = run(&mut cmd);
        assert!(ok, "our rar -s -sd failed:\n{out}");
    }
    let volumes = rar_rs::discover_volumes(&ours);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    if let Some(_unrar) = unrar_bin() {
        let (ok, out) = unrar_test(&volumes[0], None);
        assert!(ok, "UnRAR rejected our -sd dependent volume set:\n{out}");
        let win = dir.path().join("win_ours_cli_sd");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = unrar_extract(&volumes[0], &win, None);
        assert!(ok, "UnRAR failed to extract our -sd volume set:\n{out}");
        for (name, src) in &data {
            assert_eq!(
                file_sha256(&win.join(name)),
                file_sha256_bytes(src),
                "WinRAR extracted a different {name} from our -sd set"
            );
        }
    }

    // WinRAR -> ours: `-s -sd` multi-volume dependent set read back.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_cli_sd.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-s", "-sd", "-v100k", "-idq"])
            .arg(&theirs)
            .args(files.iter().map(|p| p.file_name().unwrap()))
            .current_dir(dir.path()));
        assert!(ok, "WinRAR -s -sd failed:\n{out}");
        let volumes = rar_rs::discover_volumes(&theirs);
        let mut ar = ArchiveReader::open(&volumes[0]).unwrap();
        for (name, src) in &data {
            assert_eq!(
                ar.read_entry(ar.unique_entry(name).unwrap()).unwrap(),
                *src,
                "rar-rs read a different {name} from WinRAR's -sd set"
            );
        }
    }
}

/// Our split chunks repeat the FILE_TIME record, so official UnRAR shows a
/// member's nanoseconds from any volume of the set (not just the first).
#[test]
fn our_middle_volumes_expose_nanosecond_times_to_unrar() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: UnRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("big.bin");
    write_pattern_file(&src, 200_000, 7);
    let arc = dir.path().join("t.rar");
    {
        let mut rar =
            ArchiveWriter::create_with(&arc, WriterOptions::default().volume_size(64 * 1024))
                .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&arc);
    assert!(volumes.len() >= 3, "need a middle volume");
    let (ok, out) = run(Command::new(&unrar).args(["lt"]).arg(&volumes[1]));
    assert!(ok, "UnRAR lt failed:\n{out}");
    let line = out
        .lines()
        .find(|line| line.contains("Modified:"))
        .expect("a Modified line");
    let fraction = line.rsplit(',').next().unwrap_or("").trim();
    assert_ne!(
        fraction, "000000000",
        "middle volume must carry the FILE_TIME record: {line}"
    );
}

fn volume_parts(dir: &std::path::Path, prefix: &str) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(prefix) && n.contains(".part") && n.ends_with(".rar")
            })
        })
        .collect();
    v.sort();
    v
}

/// The automatic quick-open rule keys on the volume's **cumulative** member
/// data, not one member's run: a volume of small members (none above 4096 B)
/// still earns the record's reserved tail — it caches nothing and writes no
/// record, but the tail is held back as padding. Ours keyed on the single
/// member, so such a volume fit more data and the set diverged.
#[test]
fn multivolume_auto_quick_open_many_small_members_is_byte_identical() {
    let Some(official) = rar_bin() else {
        eprintln!("skipped: WinRAR not installed");
        return;
    };
    let dir = temp_dir();
    let mut members = Vec::new();
    for i in 0..20u8 {
        let name = format!("f{i:02}.bin");
        std::fs::write(dir.path().join(&name), vec![i; 1000]).unwrap();
        members.push(name);
    }
    let make = |bin: &std::path::Path, arc: &str| {
        let status = Command::new(bin)
            .args(["a", "-idq", "-m0", "-v8k", arc])
            .args(&members)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "create {arc}");
    };
    make(std::path::Path::new(env!("CARGO_BIN_EXE_rar")), "ours.rar");
    make(&official, "win.rar");

    let ours = volume_parts(dir.path(), "ours");
    let win = volume_parts(dir.path(), "win");
    assert!(
        ours.len() > 2,
        "expected several volumes, got {}",
        ours.len()
    );
    assert_eq!(ours.len(), win.len(), "volume counts differ");
    for (a, b) in ours.iter().zip(&win) {
        assert_eq!(
            std::fs::read(a).unwrap(),
            std::fs::read(b).unwrap(),
            "volume {} differs from {}",
            a.display(),
            b.display()
        );
    }
}

/// The per-volume quick-open reserve when a volume caches several entries:
/// WinRAR reserves `header_len + 16` for each already-cached header and a fixed
/// 3-byte distance for the pending one. Ours used to under-reserve, shifting
/// where member data stops — a `-qo+` set with many small members must be
/// byte-identical to WinRAR's.
#[test]
fn multivolume_qoplus_set_is_byte_identical() {
    let Some(official) = rar_bin() else {
        eprintln!("skipped: WinRAR not installed");
        return;
    };
    let dir = temp_dir();
    let mut members = Vec::new();
    for i in 0..6u8 {
        let name = format!("s{i}.bin");
        std::fs::write(dir.path().join(&name), vec![i + 1; 128]).unwrap();
        members.push(name);
    }
    write_pattern_file(&dir.path().join("big.bin"), 200_000, 7);
    members.push("big.bin".to_string());

    let make = |bin: &std::path::Path, arc: &str| {
        let status = Command::new(bin)
            .args(["a", "-idq", "-m0", "-v16k", "-qo+", arc])
            .args(&members)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "create {arc}");
    };
    make(std::path::Path::new(env!("CARGO_BIN_EXE_rar")), "ours.rar");
    make(&official, "win.rar");

    let ours = volume_parts(dir.path(), "ours");
    let win = volume_parts(dir.path(), "win");
    assert!(
        ours.len() > 4,
        "expected several volumes, got {}",
        ours.len()
    );
    assert_eq!(ours.len(), win.len(), "volume counts differ");
    for (a, b) in ours.iter().zip(&win) {
        assert_eq!(
            std::fs::read(a).unwrap(),
            std::fs::read(b).unwrap(),
            "volume {} differs from {}",
            a.display(),
            b.display()
        );
    }
}
