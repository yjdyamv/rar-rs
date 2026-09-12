use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir, rarfiles_lst_lock, write_rep_text};
// ── -ma archive format version (extension: -ma7 forces RAR7/v70) ───────────

/// `-ma7` forces RAR7 (v70) members at any dictionary size (an extension
/// beyond WinRAR 7.23, which only writes v70 above 4 GiB); `-ma5` is the
/// default RAR5 format (a no-op, like WinRAR's inert `-ma5`); `-ma4` selects
/// the legacy RAR4 container (covered separately by `cli_ma4_*`); other
/// versions are rejected with WinRAR's wording.
#[test]
fn cli_archive_format_ma_switch() {
    let dir = make_temp_dir();
    let file = dir.path().join("f.bin");
    write_rep_text(&file, 32 * 1024 * 1024);

    // -ma7: v70 headers (comp_version 1, declared dict) even for a small
    // file, and the round trip stays byte-identical.
    let archive = dir.path().join("ma7.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma7", "-idq"])
        .arg(&archive)
        .arg("f.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-ma7 must be accepted");
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let f_id = rar.unique_entry("f.bin").unwrap();
    let e = rar.entry(f_id).unwrap();
    assert_eq!(e.comp_version(), 1, "-ma7 forces v70");
    assert_eq!(
        e.dict_size_bytes(),
        Some(32 * 1024 * 1024),
        "default 32 MiB declared"
    );
    assert_eq!(rar.read_entry(f_id).unwrap(), std::fs::read(&file).unwrap());

    // -ma7 with -md: the -md dictionary is declared.
    let archive = dir.path().join("ma7md.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma7", "-md16m", "-idq"])
        .arg(&archive)
        .arg("f.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let e = rar.entry(rar.unique_entry("f.bin").unwrap()).unwrap();
    assert_eq!(e.comp_version(), 1);
    assert_eq!(
        e.dict_size_bytes(),
        Some(16 * 1024 * 1024),
        "-md16m declared"
    );

    // -ma7 with a non-power-of-two -md through 4 GiB: the 1/32 increment
    // header encodes it exactly (an extension; WinRAR rejects -md6m only
    // because it cannot be a v50 log).
    let archive = dir.path().join("ma7md6m.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma7", "-md6m", "-idq"])
        .arg(&archive)
        .arg("f.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-ma7 -md6m must be accepted");
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let e = rar.entry(rar.unique_entry("f.bin").unwrap()).unwrap();
    assert_eq!(e.comp_version(), 1, "v70 header");
    assert_eq!(
        e.dict_size_bytes(),
        Some(6 * 1024 * 1024),
        "-md6m declared exactly"
    );

    // The same -md6m without -ma7 stays rejected (plain v50 cannot carry
    // it), matching WinRAR.
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-md6m", "-idq"])
        .arg(dir.path().join("bad6m.rar"))
        .arg("f.bin")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "-md6m alone must be rejected");
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("Unknown option") && msg.contains("md6m"),
        "-md6m alone: unexpected message {msg}"
    );

    // -ma5 equals the default output byte-for-byte.
    let ma5 = dir.path().join("ma5.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma5", "-idq"])
        .arg(&ma5)
        .arg("f.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let def = dir.path().join("def.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&def)
        .arg("f.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read(&ma5).unwrap(),
        std::fs::read(&def).unwrap(),
        "-ma5 is the default format"
    );
    let rar = rar_rs::ArchiveReader::open(&ma5).unwrap();
    let e = rar.entry(rar.unique_entry("f.bin").unwrap()).unwrap();
    assert_eq!(e.comp_version(), 0, "-ma5 stays v50");

    // -ma6, -ma8 (and other unsupported versions) are rejected with
    // WinRAR's wording.
    for bad in ["6", "8"] {
        let out = std::process::Command::new(RAR_CLI)
            .args(["a", &format!("-ma{bad}"), "-idq"])
            .arg(dir.path().join(format!("bad{bad}.rar")))
            .arg("f.bin")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success(), "-ma{bad} must be rejected");
        let msg = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            msg.contains("Unknown option") && msg.contains(&format!("ma{bad}")),
            "-ma{bad}: unexpected message {msg}"
        );
    }
}

// ── -so (extract to stdout), -se/-sv/-sd (solid split), -mct/-mcd ──────────

/// `-so` writes the extracted member(s) to stdout instead of to disk, which
/// is convenient for piping. All file members are concatenated in archive
/// order; directories carry no data and are skipped.
#[test]
fn cli_stdout_extract_writes_members_to_stdout() {
    let dir = make_temp_dir();
    let f = dir.path().join("f.txt");
    let g = dir.path().join("g.bin");
    std::fs::write(&f, b"stdout payload one").unwrap();
    std::fs::write(&g, vec![0x5u8; 128]).unwrap();
    let archive = dir.path().join("so.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .arg("g.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // Single member is extracted byte-for-byte to stdout. With a destination
    // directory given first (WinRAR semantics: `x archive dest name`), the
    // trailing token is treated as a member name rather than a destination.
    let out = std::process::Command::new(RAR_CLI)
        .args(["x", "-so"])
        .arg(&archive)
        .arg("f.txt")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"stdout payload one");

    // All members concatenated to stdout with `unrar x -so`.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_unrar"))
        .args(["x", "-so", "-idq"])
        .arg(&archive)
        .output()
        .unwrap();
    assert!(out.status.success());
    // Archive order is f.txt then g.bin; the stream is their concatenation.
    let mut expected = b"stdout payload one".to_vec();
    expected.extend_from_slice(&[0x5u8; 128]);
    assert_eq!(out.stdout, expected, "-so must concatenate all members");
}

/// `-se` / `-sv` / `-sd` (WinRAR `-s` modifiers that split the solid chain)
/// are accepted and behave: `-sd` keeps the statistics across the archive
/// (default), `-sv` resets them at every volume boundary, `-se` resets on a
/// file-extension change. All must round-trip byte-identically.
#[test]
fn cli_solid_reset_switches_accepted_and_roundtrip() {
    let _guard = rarfiles_lst_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.bin");
    let c = dir.path().join("c.txt");
    std::fs::write(&a, b"alpha text block ".repeat(20_000)).unwrap();
    std::fs::write(&b, vec![0xABu8; 2_000_000]).unwrap();
    std::fs::write(&c, b"gamma text block ".repeat(20_000)).unwrap();

    // `-sd` (continuous, the default) solid archive: accepted, round-trips.
    let sd = dir.path().join("sd.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-s", "-sd", "-idq"])
        .arg(&sd)
        .arg("a.txt")
        .arg("b.bin")
        .arg("c.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-sd must be accepted");
    let mut rar = rar_rs::ArchiveReader::open(&sd).unwrap();
    let a_id = rar.unique_entry("a.txt").unwrap();
    assert_eq!(rar.read_entry(a_id).unwrap(), std::fs::read(&a).unwrap());
    let b_id = rar.unique_entry("b.bin").unwrap();
    assert_eq!(rar.read_entry(b_id).unwrap(), std::fs::read(&b).unwrap());
    let c_id = rar.unique_entry("c.txt").unwrap();
    assert_eq!(rar.read_entry(c_id).unwrap(), std::fs::read(&c).unwrap());

    // `-se` (reset on extension change): accepted, round-trips.
    let se = dir.path().join("se.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-s", "-se", "-idq"])
        .arg(&se)
        .arg("a.txt")
        .arg("b.bin")
        .arg("c.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-se must be accepted");
    let mut rar = rar_rs::ArchiveReader::open(&se).unwrap();
    let a_id = rar.unique_entry("a.txt").unwrap();
    assert_eq!(rar.read_entry(a_id).unwrap(), std::fs::read(&a).unwrap());
    let b_id = rar.unique_entry("b.bin").unwrap();
    assert_eq!(rar.read_entry(b_id).unwrap(), std::fs::read(&b).unwrap());
    let c_id = rar.unique_entry("c.txt").unwrap();
    assert_eq!(rar.read_entry(c_id).unwrap(), std::fs::read(&c).unwrap());

    // `-sv` (reset at each volume boundary): multi-volume, byte-exact
    // non-final volumes, and a full round-trip.
    let sv = dir.path().join("sv.rar");
    let vol = 1024 * 1024u64;
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-s", "-sv", "-m0", "--volume-size=1m", "-idq"])
        .arg(&sv)
        .arg("a.txt")
        .arg("b.bin")
        .arg("c.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-sv must be accepted");
    let volumes = rar_rs::discover_volumes(&sv);
    assert!(
        volumes.len() >= 3,
        "expected several volumes, got {}",
        volumes.len()
    );
    for v in &volumes[..volumes.len() - 1] {
        assert_eq!(
            std::fs::metadata(v).unwrap().len(),
            vol,
            "-sv non-final volume {} must be exactly {vol} bytes",
            v.display()
        );
    }
    let mut rar = rar_rs::ArchiveReader::open(&volumes[0]).unwrap();
    let a_id = rar.unique_entry("a.txt").unwrap();
    assert_eq!(rar.read_entry(a_id).unwrap(), std::fs::read(&a).unwrap());
    let b_id = rar.unique_entry("b.bin").unwrap();
    assert_eq!(rar.read_entry(b_id).unwrap(), std::fs::read(&b).unwrap());
    let c_id = rar.unique_entry("c.txt").unwrap();
    assert_eq!(rar.read_entry(c_id).unwrap(), std::fs::read(&c).unwrap());
}

/// `-mct` / `-mcd` (advanced compression sub-switches) are accepted without
/// changing the outcome. WinRAR recognizes them; mapping them through the
/// existing `-mc` no-op keeps our CLI parity-complete.
#[test]
fn cli_mct_mcd_accepted_as_noops() {
    let dir = make_temp_dir();
    let file = dir.path().join("p.txt");
    std::fs::write(&file, b"advanced compression switch payload").unwrap();
    for sw in ["-mct", "-mcd"] {
        let archive = dir.path().join(format!("mc{sw}.rar"));
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", sw, "-idq"])
            .arg(&archive)
            .arg("p.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{sw} must be accepted");
        let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let p_id = rar.unique_entry("p.txt").unwrap();
        assert_eq!(
            rar.read_entry(p_id).unwrap(),
            b"advanced compression switch payload",
            "{sw} must not alter the payload"
        );
    }
}

/// Member selection on extract (`x`/`e`) never treats a name as a destination
/// directory. `rar x archive name` extracts only the matching member(s) to
/// the default directory; a name that matches nothing is a hard error, not a
/// silent dump into a `name/` folder.
#[test]
fn cli_extract_member_selection_never_hijacks_dest() {
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.bin");
    let c = dir.path().join("c.txt");
    std::fs::write(&a, b"alpha").unwrap();
    std::fs::write(&b, vec![0x7u8; 64]).unwrap();
    std::fs::write(&c, b"gamma").unwrap();
    let archive = dir.path().join("sel.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("a.txt")
        .arg("b.bin")
        .arg("c.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // `rar x archive a.txt` extracts ONLY a.txt here, not into an `a.txt/`
    // directory, and does not extract b.bin/c.txt.
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "--dest"])
        .arg(&out)
        .arg(&archive)
        .arg("a.txt")
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        out.join("a.txt").exists(),
        "selected member must be extracted"
    );
    assert!(
        !out.join("b.bin").exists(),
        "unselected member must not appear"
    );
    assert!(
        !out.join("c.txt").exists(),
        "unselected member must not appear"
    );
    // The selected name must not be interpreted as a directory.
    assert!(
        !out.join("a.txt").is_dir(),
        "name must not become a directory"
    );

    // A name matching nothing is a hard error (clear message), not a silent
    // extraction into a `<name>/` directory.
    let miss = dir.path().join("miss");
    std::fs::create_dir_all(&miss).unwrap();
    let res = std::process::Command::new(RAR_CLI)
        .args(["x", "--dest"])
        .arg(&miss)
        .arg(&archive)
        .arg("nope.txt")
        .output()
        .unwrap();
    assert!(!res.status.success(), "matching no member must fail");
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&res.stdout),
        String::from_utf8_lossy(&res.stderr)
    );
    assert!(
        msg.contains("no archive members matched"),
        "expected a clear no-match error, got: {msg}"
    );
    assert!(
        !miss.join("nope.txt").exists(),
        "a non-matching name must not be created as a file/dir"
    );
}

/// `-se` (reset the solid chain on a file-extension change) must preserve
/// WinRAR's input order — it does NOT reorder members by extension. The solid
/// statistics are simply reset as a new extension is encountered.
#[test]
fn cli_se_preserves_input_order() {
    let _guard = rarfiles_lst_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.bin");
    let c = dir.path().join("c.txt");
    let d = dir.path().join("d.bin");
    std::fs::write(&a, b"alpha text block ".repeat(20_000)).unwrap();
    std::fs::write(&b, vec![0xABu8; 1_000_000]).unwrap();
    std::fs::write(&c, b"gamma text block ".repeat(20_000)).unwrap();
    std::fs::write(&d, vec![0xCDu8; 1_000_000]).unwrap();

    let arc = dir.path().join("se_orig.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-s", "-se", "-idq"])
        .arg(&arc)
        .arg("a.txt")
        .arg("b.bin")
        .arg("c.txt")
        .arg("d.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // Input order a.txt, b.bin, c.txt, d.bin must be preserved exactly.
    let mut rar = rar_rs::ArchiveReader::open(&arc).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<String>>(),
        vec!["a.txt", "b.bin", "c.txt", "d.bin"],
        "-se must not reorder members by extension"
    );
    let a_id = rar.unique_entry("a.txt").unwrap();
    assert_eq!(rar.read_entry(a_id).unwrap(), std::fs::read(&a).unwrap());
    let b_id = rar.unique_entry("b.bin").unwrap();
    assert_eq!(rar.read_entry(b_id).unwrap(), std::fs::read(&b).unwrap());
    let c_id = rar.unique_entry("c.txt").unwrap();
    assert_eq!(rar.read_entry(c_id).unwrap(), std::fs::read(&c).unwrap());
    let d_id = rar.unique_entry("d.bin").unwrap();
    assert_eq!(rar.read_entry(d_id).unwrap(), std::fs::read(&d).unwrap());

    // The order-preserving -se archive must read back through our own
    // `unrar t` (self-consistency check; cli_behavior is cross-platform and
    // has no WinRAR dependency).
    let res = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&arc)
        .output()
        .unwrap();
    assert!(
        res.status.success(),
        "unrar t rejected our order-preserving -se archive:\n{}",
        String::from_utf8_lossy(&res.stderr)
    );
}
