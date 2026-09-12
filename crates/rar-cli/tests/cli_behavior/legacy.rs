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
fn cli_ma4_rejects_rar5_only_switches() {
    let dir = make_temp_dir();
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"payload").unwrap();
    let arc = dir.path().join("ma4x.rar");

    // Multi-volume + recovery record stays rejected for RAR4 (WinRAR
    // forbids inline recovery records on volume sets there too).
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-rr10%", "--volume-size=100k"])
        .arg(&arc)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "-ma4 -rr10% with volumes must be rejected"
    );

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
