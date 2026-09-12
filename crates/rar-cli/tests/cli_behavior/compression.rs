use crate::support::{RAR_CLI, entry_dict_log, make_temp_dir, pseudo_random_bytes, write_rep_text};
// ── -md dictionary size (aligned with WinRAR 7.23) ─────────────────────────

#[test]
fn cli_dict_size_switch_matches_winrar() {
    let dir = make_temp_dir();
    let file = dir.path().join("rep32t.bin");
    write_rep_text(&file, 32 * 1024 * 1024);

    // Default: 32 MiB (log 8) — WinRAR 7.23's default at every level.
    let archive = dir.path().join("def.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("rep32t.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(entry_dict_log(&archive, "rep32t.bin"), 8);

    // -md64m on a 32 MiB file: 2x file size caps it at 64 MiB (log 9).
    let archive = dir.path().join("md64.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-md64m", "-idq"])
        .arg(&archive)
        .arg("rep32t.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(entry_dict_log(&archive, "rep32t.bin"), 9);

    // -md128k is honored (log 0) even for a large file.
    let archive = dir.path().join("md128.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-md128k", "-idq"])
        .arg(&archive)
        .arg("rep32t.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(entry_dict_log(&archive, "rep32t.bin"), 0);

    // The archive with the larger dictionary round-trips.
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let rep_id = rar.unique_entry("rep32t.bin").unwrap();
    let data = rar.read_entry(rep_id).unwrap();
    assert_eq!(data, std::fs::read(&file).unwrap());

    // -md above 4 GiB is accepted (WinRAR 7.23 accepts arbitrary values,
    // e.g. -md6g/-md65g). For a small file the 2x-file-size cap lands in
    // the RAR5 range, so the member stays a plain v50 with the capped log.
    for md in ["6g", "8g", "65g"] {
        let archive = dir.path().join(format!("md{md}.rar"));
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", &format!("-md{md}"), "-idq"])
            .arg(&archive)
            .arg("rep32t.bin")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "-md{md} must be accepted");
        let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let rep_id = rar.unique_entry("rep32t.bin").unwrap();
        let e = rar.entry(rep_id).unwrap();
        assert_eq!(e.comp_version(), 0, "-md{md} small file stays v50");
        assert_eq!(e.dict_size_bytes(), None, "-md{md}");
        // 2x floor_pow2(32 MiB) = 64 MiB -> log 9.
        assert_eq!(e.comp_dict_size(), 9, "-md{md} cap");
        let data = rar.read_entry(rep_id).unwrap();
        assert_eq!(data, std::fs::read(&file).unwrap(), "-md{md} roundtrip");
    }

    // Invalid sizes are rejected with WinRAR's wording.
    for bad in ["-md3m", "-md", "-md100k", "-md129g"] {
        let out = std::process::Command::new(RAR_CLI)
            .args(["a", bad, "-idq"])
            .arg(
                dir.path()
                    .join(format!("bad_{}.rar", bad.trim_start_matches('-'))),
            )
            .arg("rep32t.bin")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{bad} must be rejected");
        let msg = String::from_utf8_lossy(&out.stderr);
        assert!(
            msg.contains("Unknown option") && msg.contains(bad.trim_start_matches('-')),
            "{bad}: unexpected message {msg}"
        );
    }
}

// ── Long-range matching (WinRAR -mcl semantics) ────────────────────────────

/// A 32 MiB file whose second half is an exact copy of its (random)
/// first half: the 16 MiB match distance is far beyond the near match
/// window, so only the long-range search can compress it. WinRAR applies
/// this automatically for -m2..-m5; we must too.
#[test]
fn long_range_compresses_distant_copies() {
    let dir = make_temp_dir();
    let file = dir.path().join("pair32.bin");
    let half = 16 * 1024 * 1024usize;
    let mut data = pseudo_random_bytes(half, 42);
    data.extend_from_slice(&data.clone());
    std::fs::write(&file, &data).unwrap();

    let archive = dir.path().join("pair.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-md32m", "-idq"])
        .arg(&archive)
        .arg("pair32.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    // The 16 MiB copy must compress to a small fraction: well below
    // 1.25x the random half (16 MiB + small overhead + tiny copy).
    let packed = std::fs::metadata(&archive).unwrap().len();
    assert!(
        packed < 20 * 1024 * 1024,
        "distant copy must compress: {packed} bytes"
    );
    // And it must round-trip byte-identically through our extractor.
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    rar.extract_all_with_options(
        &out_dir,
        rar_rs::ExtractOptions {
            max_unpacked_bytes: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(std::fs::read(out_dir.join("pair32.bin")).unwrap(), data);
}
