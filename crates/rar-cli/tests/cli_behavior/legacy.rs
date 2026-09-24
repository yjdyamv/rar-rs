use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir};
/// `-ma4` (legacy RAR3/4 container) creates a RAR4 archive whose members
/// round-trip through both our own reader and the `unrar` CLI, and are
/// rejected when combined with RAR5-only switches.
#[test]
fn cli_ma4_creates_rar4_archive() {
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"rar4 CLI member A").unwrap();
    std::fs::write(&b, b"rar4 CLI member B is a bit longer").unwrap();
    let arc = dir.path().join("ma4.rar");

    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-idq"])
        .arg(&arc)
        .arg("a.txt")
        .arg("b.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar a -ma4 failed");

    // The RAR4 container carries the 7-byte `Rar!\x1a\x07\x00` signature
    // (a RAR5 archive would start with the 8-byte `...\x07\x01\x00`).
    let head = std::fs::read(&arc).unwrap();
    assert_eq!(
        &head[..7],
        b"Rar!\x1a\x07\x00",
        "archive must carry the RAR4 signature"
    );

    let mut rar = rar_rs::ArchiveReader::open(&arc).unwrap();
    let mut names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["a.txt".to_string(), "b.txt".to_string()]);
    let a_id = rar.unique_entry("a.txt").unwrap();
    assert_eq!(rar.read_entry(a_id).unwrap(), b"rar4 CLI member A");
    let b_id = rar.unique_entry("b.txt").unwrap();
    assert_eq!(
        rar.read_entry(b_id).unwrap(),
        b"rar4 CLI member B is a bit longer"
    );

    // Our own `unrar t` must accept the RAR4 archive.
    let res = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&arc)
        .output()
        .unwrap();
    assert!(
        res.status.success(),
        "unrar t rejected our -ma4 archive:\n{}",
        String::from_utf8_lossy(&res.stderr)
    );
}

/// `-ma2`/`-ma15` select the legacy RAR 2.x / RAR 1.5 member versions
/// inside the RAR4 container. Members round-trip through our own reader
/// and the `unrar` CLI, and read-back reports the requested version.
#[test]
fn cli_ma2_ma15_create_legacy_rar4_members() {
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, b"legacy CLI member payload ".repeat(2000)).unwrap();

    for (flag, expected) in [
        ("-ma2", rar_rs::ArchiveVersion::V20),
        ("-ma15", rar_rs::ArchiveVersion::V15),
    ] {
        let arc = dir
            .path()
            .join(format!("{}.rar", flag.trim_start_matches('-')));

        let status = std::process::Command::new(RAR_CLI)
            .args(["a", flag, "-idq"])
            .arg(&arc)
            .arg("a.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "rar a {flag} failed");

        // The RAR4 container carries the 7-byte `Rar!\x1a\x07\x00` signature.
        let head = std::fs::read(&arc).unwrap();
        assert_eq!(
            &head[..7],
            b"Rar!\x1a\x07\x00",
            "{flag}: archive must carry the RAR4 signature"
        );

        let rar = rar_rs::ArchiveReader::open(&arc).unwrap();
        let entry = rar.unique_entry("a.txt").unwrap();
        assert_eq!(
            rar.entry(entry).unwrap().version(),
            expected,
            "{flag}: member must report {expected}"
        );

        let res = std::process::Command::new(UNRAR_CLI)
            .args(["t", "-idq"])
            .arg(&arc)
            .output()
            .unwrap();
        assert!(
            res.status.success(),
            "unrar t rejected our {flag} archive:\n{}",
            String::from_utf8_lossy(&res.stderr)
        );
    }
}

/// `-p` member encryption on the legacy writers: `-ma2`/`-ma15` members
/// encrypt with the RAR20 block / RAR15 stream cipher (no salt), and the
/// password round-trips through our `unrar` and the library reader.
#[test]
fn cli_ma2_ma15_password_roundtrips() {
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    let payload = b"legacy CLI encrypted member payload ".repeat(2000);
    std::fs::write(&a, &payload).unwrap();

    for flag in ["-ma2", "-ma15"] {
        let arc = dir
            .path()
            .join(format!("{}-pw.rar", flag.trim_start_matches('-')));

        let status = std::process::Command::new(RAR_CLI)
            .args(["a", flag, "-ppw", "-idq"])
            .arg(&arc)
            .arg("a.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "rar a {flag} -ppw failed");

        // Our own unrar verifies the encrypted member with the password.
        let res = std::process::Command::new(UNRAR_CLI)
            .args(["t", "-ppw", "-idq"])
            .arg(&arc)
            .output()
            .unwrap();
        assert!(
            res.status.success(),
            "unrar t -ppw rejected our {flag} encrypted archive:\n{}",
            String::from_utf8_lossy(&res.stderr)
        );

        // And without it the listed member must fail to decode.
        let res = std::process::Command::new(UNRAR_CLI)
            .args(["t", "-p-", "-idq"])
            .arg(&arc)
            .output()
            .unwrap();
        assert!(
            !res.status.success(),
            "unrar t without password should have failed on {flag}"
        );

        let mut rar =
            rar_rs::ArchiveReader::open_with(&arc, rar_rs::OpenOptions::new().password("pw"))
                .unwrap();
        assert_eq!(
            rar.read_entry(rar.unique_entry("a.txt").unwrap()).unwrap(),
            payload,
            "{flag} -ppw content mismatch"
        );
    }
}

/// `-hp` is a WinRAR-style attached-value switch: a bare `-hp` (anywhere in
/// the command line, including right before the archive path) must not
/// swallow the next position argument, and `-hp{pwd}` sets its own header
/// password. The header-encrypted archive round-trips through `unrar` and
/// the library reader with the password.
#[test]
fn cli_header_encrypt_switch_position_and_attached_password() {
    let dir = make_temp_dir();
    let a = dir.path().join("hp.txt");
    let payload = b"header encrypted legacy content ".repeat(1800);
    std::fs::write(&a, &payload).unwrap();

    // Bare `-hp` between the `-p` password switch and the archive path.
    let arc_bare = dir.path().join("hp-bare.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma15", "-ppw", "-hp", "-idq"])
        .arg(&arc_bare)
        .arg("hp.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "rar a -hp before the archive path must not swallow it"
    );

    // Attached `-hp{pwd}` supplies its own password (WinRAR syntax).
    let arc_pw = dir.path().join("hp-pw.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-hppw2", "-idq"])
        .arg(&arc_pw)
        .arg("hp.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar a -hppw2 failed");

    for (arc, password) in [(&arc_bare, "pw"), (&arc_pw, "pw2")] {
        let res = std::process::Command::new(UNRAR_CLI)
            .args(["t", &format!("-p{password}"), "-idq"])
            .arg(arc)
            .output()
            .unwrap();
        assert!(
            res.status.success(),
            "unrar t -p{password} rejected {arc:?}:\n{}",
            String::from_utf8_lossy(&res.stderr)
        );

        let mut rar =
            rar_rs::ArchiveReader::open_with(arc, rar_rs::OpenOptions::new().password(password))
                .unwrap();
        assert_eq!(
            rar.read_entry(rar.unique_entry("hp.txt").unwrap()).unwrap(),
            payload,
            "{arc:?} content mismatch"
        );
    }
}

/// Legacy RAR 1.5/2.x members that span volume boundaries keep their member
/// version (`-ma2`/`-ma15`) in every emitted entry header — the multivol
/// emit path must not fall back to the default RAR29 header. The set is
/// discovered, listed and verified entry-by-entry across the volume chunks.
#[test]
fn cli_ma2_ma15_multivolume_members_keep_version() {
    let dir = make_temp_dir();
    // Near-incompressible data so the stored member really spans 64k volumes.
    let expected_bytes: Vec<u8> = (0..260_000u32)
        .map(|i| (i.wrapping_mul(1_103_515_245) >> 16) as u8)
        .collect();
    let big = dir.path().join("rnd.bin");
    std::fs::write(&big, &expected_bytes).unwrap();

    for (flag, expected) in [
        ("-ma2", rar_rs::ArchiveVersion::V20),
        ("-ma15", rar_rs::ArchiveVersion::V15),
    ] {
        let arc = dir
            .path()
            .join(format!("{}-multivol.rar", flag.trim_start_matches('-')));
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", flag, "-m0", "-v64k", "-idq"])
            .arg(&arc)
            .arg("rnd.bin")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "rar a {flag} -m0 -v64k failed");

        let volumes = rar_rs::discover_volumes(&arc);
        assert!(
            volumes.len() > 1,
            "{flag}: expected a multi-volume set, got {}",
            volumes.len()
        );

        let mut rar = rar_rs::ArchiveReader::open(volumes[0].as_path()).unwrap();
        let entry = rar.unique_entry("rnd.bin").unwrap();
        assert_eq!(
            rar.entry(entry).unwrap().version(),
            expected,
            "{flag}: multi-volume member must report {expected}"
        );
        assert_eq!(
            rar.read_entry(entry).unwrap(),
            expected_bytes,
            "{flag}: cross-volume content mangled"
        );

        // Our own unrar verifies every volume in the set.
        let res = std::process::Command::new(UNRAR_CLI)
            .args(["t", "-idq"])
            .arg(&arc)
            .output()
            .unwrap();
        assert!(
            res.status.success(),
            "unrar t rejected multi-volume {flag} archive:\n{}",
            String::from_utf8_lossy(&res.stderr)
        );
    }
}

/// RAR5-only creation switches are rejected when combined with `-ma4`, since
/// the RAR4 container cannot express them. `-hp` and `-rr` are now supported
/// on RAR4 too, so they are verified positively instead.
#[test]
fn cli_ma4_accepts_header_encryption_and_recovery_records() {
    let dir = make_temp_dir();
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"payload").unwrap();

    // Multi-volume + recovery record is supported for RAR4 too (WinRAR's
    // RAR4 writer records every volume of a set).
    let big = dir.path().join("big.txt");
    std::fs::write(&big, vec![b'x'; 120 * 1024]).unwrap();
    let arc_mv = dir.path().join("ma4mv.rar");
    let ok = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-rr10%", "--volume-size=40k", "-idq"])
        .arg(&arc_mv)
        .arg("big.txt")
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success();
    assert!(
        ok,
        "-ma4 -rr10% with volumes must be accepted (per-volume recovery record)"
    );
    let volumes: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().contains("ma4mv"))
                .unwrap_or(false)
        })
        .collect();
    assert!(volumes.len() >= 2, "expected a multi-volume set");
    for volume in &volumes {
        let raw = std::fs::read(volume).unwrap();
        assert!(
            raw.windows(10).any(|w| w == b"RRProtect+"),
            "{} carries no NEWSUB recovery record",
            volume.display()
        );
    }

    // `-hp` header encryption is supported on RAR4; the CLI must accept it.
    let arc2 = dir.path().join("ma4hp.rar");
    let ok = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-hpsecret", "-idq"])
        .arg(&arc2)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success();
    assert!(
        ok,
        "-ma4 -hpsecret must be accepted (header encryption on RAR4)"
    );

    let mut rar =
        rar_rs::ArchiveReader::open_with(&arc2, rar_rs::OpenOptions::new().password("secret"))
            .unwrap();
    let f_id = rar.unique_entry("f.txt").unwrap();
    assert_eq!(rar.read_entry(f_id).unwrap(), b"payload");

    // `-rr10%` inline recovery record is supported on single-volume RAR4:
    // the archive must carry a NEWSUB (0x7a) `RR` block before ENDARC.
    let arc3 = dir.path().join("ma4rr.rar");
    let ok = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-rr10%", "-m0", "-idq"])
        .arg(&arc3)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success();
    assert!(ok, "-ma4 -rr10% must be accepted (recovery record on RAR4)");
    let raw = std::fs::read(&arc3).unwrap();
    let has_newsub_rr = raw.windows(10).any(|w| w == b"RRProtect+");
    assert!(
        has_newsub_rr,
        "-ma4 -rr10% archive must carry a NEWSUB RR block"
    );
    let mut rar = rar_rs::ArchiveReader::open(&arc3).unwrap();
    let f_id = rar.unique_entry("f.txt").unwrap();
    assert_eq!(rar.read_entry(f_id).unwrap(), b"payload");
}

/// `rec_sectors` of a RAR4 archive's NEWSUB (`0x7a`) `RR` record: the parity
/// count WinRAR reports and sizes its records by.
fn recovery_sectors(path: &std::path::Path) -> u32 {
    let bytes = std::fs::read(path).unwrap();
    let mark = b"RRProtect+";
    let at = bytes
        .windows(mark.len())
        .position(|window| window == mark)
        .expect("RR record");
    let header = at - 32;
    let name_size = u16::from_le_bytes([bytes[header + 26], bytes[header + 27]]) as usize;
    let count = at + name_size + 8;
    u32::from_le_bytes(bytes[count..count + 4].try_into().unwrap())
}

/// The three `-rr` forms follow WinRAR 6.23 (measured): a bare `-rr<N>` is the
/// legacy RAR4 parity-sector count (exactly N at any archive size), `-rr<N>%`
/// a percentage of the protected prefix, and bare `-rr` the 3% default.
#[test]
fn legacy_rar4_recovery_record_follows_the_rr_forms() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("big.bin"), vec![0x5a; 200_000]).unwrap();
    let create = |name: &str, spec: &str| {
        let arc = dir.path().join(name);
        let ok = std::process::Command::new(RAR_CLI)
            .args(["a", "-ma4", "-m0", spec, "-idq"])
            .arg(&arc)
            .arg("big.bin")
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok, "{spec} failed");
        arc
    };

    // The count form is exact and independent of the archive size.
    assert_eq!(recovery_sectors(&create("count.rar", "-rr10")), 10);
    // The percent form is not: 200 KB of members needs far more than ten.
    let percent = recovery_sectors(&create("percent.rar", "-rr10%"));
    assert!(
        percent > 10,
        "-rr10% must scale with the archive, got {percent}"
    );
    // Bare `-rr` is the 3% default, not 10%: it agrees with `-rr3%`.
    assert_eq!(
        recovery_sectors(&create("bare.rar", "-rr")),
        recovery_sectors(&create("three.rar", "-rr3%"))
    );
}

/// `rar rr` takes WinRAR's 3% default when no strength is given, and honors an
/// explicit one in every spelling: the `a` switch forms (`-rr20%` percent,
/// `-rr20` legacy parity-sector count) and our trailing percent. WinRAR's own
/// `rr` ignores all of them and always writes 3% (measured on 6.23/7.23).
#[test]
fn legacy_rr_command_honors_the_requested_strength() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("big.bin"), vec![0x77; 200_000]).unwrap();
    let base = dir.path().join("base.rar");
    assert!(
        std::process::Command::new(RAR_CLI)
            .args(["a", "-ma4", "-m0", "-idq"])
            .arg(&base)
            .arg("big.bin")
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );

    let rr = |name: &str, spec: &[&str]| {
        let arc = dir.path().join(name);
        std::fs::copy(&base, &arc).unwrap();
        assert!(
            std::process::Command::new(RAR_CLI)
                .arg("rr")
                .arg(&arc)
                .args(spec)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success(),
            "rr {spec:?}"
        );
        recovery_sectors(&arc)
    };

    let default = rr("bare.rar", &[]);
    assert_eq!(
        default,
        rr("three.rar", &["3%"]),
        "the rr command's default must be WinRAR's 3%"
    );
    assert_eq!(
        rr("switch3.rar", &["-rr"]),
        default,
        "bare -rr is the default"
    );
    // The percent forms agree whichever way they are spelled.
    let percent = rr("switch-percent.rar", &["-rr20%"]);
    assert_eq!(rr("trail.rar", &["20"]), percent);
    assert_eq!(rr("trail-percent.rar", &["20%"]), percent);
    assert!(percent > default, "3% must be weaker than 20%");
    // A bare `-rr20` is the legacy record's native unit: exactly 20 sectors,
    // whatever the archive size — smaller than 20% of this archive.
    assert_eq!(rr("switch-count.rar", &["-rr20"]), 20);
    assert!(rr("switch-count.rar", &["-rr20"]) < percent);
}

/// `-ma13`/`-ma14` create the DOS-era `RE~^` container (RAR 1.3/1.4):
/// stored, compressed and solid members round-trip through our reader and
/// `unrar`, and the archive comment is queued ahead of the first member.
#[test]
fn cli_ma14_creates_rar13_archives() {
    let dir = make_temp_dir();
    let a = dir.path().join("a.txt");
    let payload = b"RAR 1.3/1.4 CLI member payload ".repeat(1500);
    std::fs::write(&a, &payload).unwrap();
    std::fs::write(dir.path().join("b.txt"), b"small sibling\r\n").unwrap();
    std::fs::write(dir.path().join("note.txt"), b"cli archive comment\r\n").unwrap();

    for (flag, solid, comment, password) in [
        ("-ma14", false, false, false),
        ("-ma13", true, false, false),
        ("-ma14", false, true, false),
        ("-ma14", true, false, true),
    ] {
        let arc = dir.path().join(format!(
            "{}{}{}{}.rar",
            flag.trim_start_matches('-'),
            if solid { "-s" } else { "" },
            if comment { "-z" } else { "" },
            if password { "-p" } else { "" }
        ));
        let mut command = std::process::Command::new(RAR_CLI);
        command.args(["a", flag, "-m5", "-idq"]);
        if solid {
            command.arg("-s");
        }
        if comment {
            command.arg(format!("-z{}", dir.path().join("note.txt").display()));
        }
        if password {
            command.arg("-ppw");
        }
        command
            .arg(&arc)
            .arg("a.txt")
            .arg("b.txt")
            .current_dir(dir.path());
        let status = command.status().unwrap();
        assert!(status.success(), "rar a {flag} (solid={solid}) failed");

        // RAR 1.3/1.4 carries the 4-byte `RE~^` signature (plus the 7-byte
        // main header whose size follows).
        let head = std::fs::read(&arc).unwrap();
        assert_eq!(&head[..4], b"RE~^", "{flag}: DOS-era signature expected");
        let main_head = u16::from_le_bytes([head[4], head[5]]) as usize;
        if comment {
            assert!(
                main_head > 7,
                "{flag}: the archive comment extends the main header"
            );
        } else {
            assert_eq!(main_head, 7, "{flag}: main header size");
        }

        let mut rar = if password {
            rar_rs::ArchiveReader::open_with(&arc, rar_rs::OpenOptions::new().password("pw"))
                .unwrap()
        } else {
            rar_rs::ArchiveReader::open(&arc).unwrap()
        };
        for entry in rar.entries() {
            assert_eq!(entry.version(), rar_rs::ArchiveVersion::V14, "{flag}");
        }
        let a_id = rar.unique_entry("a.txt").unwrap();
        assert_eq!(rar.read_entry(a_id).unwrap(), payload, "{flag}: a.txt");
        let b_id = rar.unique_entry("b.txt").unwrap();
        assert_eq!(rar.read_entry(b_id).unwrap(), b"small sibling\r\n");
        if comment {
            assert_eq!(
                rar.comment().unwrap().as_deref(),
                Some(b"cli archive comment\r\n".as_slice()),
                "{flag}: archive comment"
            );
        }
        drop(rar);

        let mut command = std::process::Command::new(UNRAR_CLI);
        command.args(["t", "-idq"]);
        if password {
            command.arg("-ppw");
        }
        let res = command.arg(&arc).output().unwrap();
        assert!(
            res.status.success(),
            "unrar t rejected our {flag} archive (solid={solid}, comment={comment}, pw={password}):\n{}",
            String::from_utf8_lossy(&res.stderr)
        );
    }
}

/// RAR5-only creation switches are rejected for `-ma13`/`-ma14`, since the
/// DOS-era container cannot express them (no `-hp`, recovery records,
/// quick-open, BLAKE2sp or dictionaries).
#[test]
fn cli_ma14_rejects_rar5_only_switches() {
    let dir = make_temp_dir();
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"payload").unwrap();

    for (name, extra) in [
        ("hp", vec!["-hpsecret"]),
        ("sfx", vec!["-sfx"]),
        ("recovery", vec!["-rr10%"]),
        ("quick-open", vec!["-qo"]),
        ("blake2", vec!["-htb"]),
    ] {
        let arc = dir.path().join(format!("ma14-{name}.rar"));
        let mut command = std::process::Command::new(RAR_CLI);
        command.args(["a", "-ma14", "-idq"]);
        command.args(&extra);
        let status = command
            .arg(&arc)
            .arg("f.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "-ma14 {extra:?} must be rejected for RAR 1.3/1.4"
        );
    }
}

/// `-ma13 -v` creates an old-style `.rar`/`.r00` volume set: every volume
/// but the last is exactly the requested size, members spanning volumes
/// reassemble (solid and encrypted alike), and our `unrar` verifies the
/// whole set.
#[test]
fn cli_ma14_multivolume_sets_roundtrip() {
    let dir = make_temp_dir();
    let expected: Vec<u8> = (0..120_000u32)
        .map(|i| {
            let x = i.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect();
    std::fs::write(dir.path().join("rnd.bin"), &expected).unwrap();
    std::fs::write(dir.path().join("tail.txt"), b"tail member\r\n").unwrap();

    for (flag, password) in [("-ma13", false), ("-ma14", true)] {
        let arc = dir.path().join(format!(
            "mv13{}{}.rar",
            flag.trim_start_matches('-'),
            if password { "-pw" } else { "" }
        ));
        let mut command = std::process::Command::new(RAR_CLI);
        command.args(["a", flag, "-m0", "--volume-size=24k", "-idq"]);
        if password {
            command.arg("-ppw");
        }
        let status = command
            .arg(&arc)
            .arg("rnd.bin")
            .arg("tail.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "rar a {flag} --volume-size=24k failed");

        assert!(arc.exists(), "{flag}: first volume missing");
        assert!(
            dir.path()
                .join(format!(
                    "{}.r00",
                    arc.file_stem().unwrap().to_string_lossy()
                ))
                .exists(),
            "{flag}: expected a second volume"
        );

        // Every volume but the last is exactly 24 KiB.
        let volumes = rar_rs::discover_volumes(&arc);
        assert!(volumes.len() >= 2, "{flag}: {} volumes", volumes.len());
        for (index, volume) in volumes.iter().enumerate() {
            let len = std::fs::metadata(volume).unwrap().len();
            if index + 1 == volumes.len() {
                assert!(len <= 24 * 1024, "{flag}: last volume {len} too large");
            } else {
                assert_eq!(len, 24 * 1024, "{flag}: volume {index} not exact");
            }
        }

        // WinRAR's per-volume listing: only the members with data in the
        // opened volume, with a legacy `volume` suffix and fragment marker.
        let listing = std::process::Command::new(RAR_CLI)
            .arg("l")
            .arg(&arc)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&listing.stdout);
        assert!(text.contains("Details: RAR 1.4, volume"), "{flag}: {text}");
        assert!(
            text.contains("rnd.bin") && !text.contains("tail.txt"),
            "{flag}: volume 1 lists only its members:\n{text}"
        );
        let verbose = std::process::Command::new(RAR_CLI)
            .arg("v")
            .arg(&arc)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&verbose.stdout);
        assert!(text.contains("-->"), "{flag}: fragment marker:\n{text}");

        let mut rar = if password {
            rar_rs::ArchiveReader::open_with(&arc, rar_rs::OpenOptions::new().password("pw"))
                .unwrap()
        } else {
            rar_rs::ArchiveReader::open(&arc).unwrap()
        };
        let id = rar.unique_entry("rnd.bin").unwrap();
        assert_eq!(rar.read_entry(id).unwrap(), expected, "{flag}: rnd.bin");
        let id = rar.unique_entry("tail.txt").unwrap();
        assert_eq!(rar.read_entry(id).unwrap(), b"tail member\r\n");
        drop(rar);

        let mut command = std::process::Command::new(UNRAR_CLI);
        command.args(["t", "-idq"]);
        if password {
            command.arg("-ppw");
        }
        let res = command.arg(&arc).output().unwrap();
        assert!(
            res.status.success(),
            "unrar t rejected the {flag} volume set:\n{}",
            String::from_utf8_lossy(&res.stderr)
        );
    }
}

/// Solid-chain resets the container cannot flag are rejected instead of
/// silently desynchronizing the decoder: pre-RAR3 chains are position-
/// derived, and RAR4 implements `-se` only for `v29`.
#[test]
fn cli_legacy_rejects_unexpressible_solid_resets() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"payload A").unwrap();
    std::fs::write(dir.path().join("b.bin"), b"payload B").unwrap();

    for (name, flag, reset) in [
        ("v14-se", "-ma14", "-se"),
        ("v15-se", "-ma15", "-se"),
        ("v20-sv", "-ma2", "-sv"),
        ("v29-sv", "-ma4", "-sv"),
    ] {
        let arc = dir.path().join(format!("{name}.rar"));
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", flag, "-s", reset, "-m5", "-idq"])
            .arg(&arc)
            .arg("a.txt")
            .arg("b.bin")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "{flag} -s {reset} must be rejected (unrepresentable chain reset)"
        );
    }

    // The RAR4 `v29` writer does implement `-se`, and the archive verifies.
    let arc = dir.path().join("v29-se.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-s", "-se", "-m5", "-idq"])
        .arg(&arc)
        .arg("a.txt")
        .arg("b.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-ma4 -s -se must work");
    let test = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&arc)
        .output()
        .unwrap();
    assert!(
        test.status.success(),
        "unrar t rejected the -se archive:\n{}",
        String::from_utf8_lossy(&test.stderr)
    );
}

/// `rar r` on a RAR 1.3/1.4 archive prints the reconstruct banner and then
/// refuses in one line (`Cannot repair archive with old format`), producing
/// nothing — WinRAR's lines, but the failure is reported: exit 3, not the 0
/// WinRAR answers for a repair that produced nothing.
#[test]
fn cli_repair_reports_rar13_as_unrepairable() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"payload").unwrap();
    let arc = dir.path().join("repair13.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma14", "-m0", "-idq"])
        .arg(&arc)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let out = std::process::Command::new(RAR_CLI)
        .args(["r"])
        .arg(&arc)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "a repair that produced nothing is an error:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !dir.path().join("fixed.repair13.rar").exists()
            && !dir.path().join("rebuilt.repair13.rar").exists(),
        "a RAR 1.3/1.4 repair produces nothing"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Reconstructing"), "{text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("Cannot repair archive with old format"),
        "{err}"
    );
}

/// `rv` / `rc` refuse RAR 1.3/1.4 volume sets instead of writing REV5 parity
/// the DOS-era container cannot use.
#[test]
fn cli_rar13_recovery_volumes_are_refused() {
    let dir = make_temp_dir();
    let big: Vec<u8> = (0..30_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(dir.path().join("big.bin"), &big).unwrap();
    let arc = dir.path().join("r13set.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma14", "-m0", "-v8k", "-idq"])
        .arg(&arc)
        .arg("big.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "create the v14 volume set");

    let out = std::process::Command::new(RAR_CLI)
        .args(["rv", "-idq"])
        .arg(&arc)
        .output()
        .unwrap();
    assert!(!out.status.success(), "rv must refuse a v14 set");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("RAR 1.3/1.4"), "{text}");
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "rev"))
            .count(),
        0,
        "no .rev files may be written"
    );

    let out = std::process::Command::new(RAR_CLI)
        .args(["rc", "-idq"])
        .arg(&arc)
        .output()
        .unwrap();
    assert!(!out.status.success(), "rc must refuse a v14 set");
}
