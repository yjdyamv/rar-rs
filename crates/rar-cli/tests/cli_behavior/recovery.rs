use crate::support::{RAR_CLI, UNRAR_CLI};
/// `rar rv` on an existing volume set + `rar rc` round trip (WinRAR 7.23
/// semantics: bare count, capped at 10x the volume count).
#[test]
fn cli_rv_creates_recovery_volumes_and_rc_rebuilds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().join("mv");

    // A 10+ volume set (the writer zero-pads names to part01..partNN,
    // like WinRAR) covering both the default-percent and the count forms
    // of `rv`; pseudo-random bytes so the member actually spans the
    // -v100k volumes.
    let mut big = Vec::with_capacity(2_500_000);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..2_500_000 {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        big.push((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8);
    }
    let src = dir.path().join("big.bin");
    std::fs::write(&src, &big).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-v100k", "-y"])
        .arg(&base)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let first = format!("{}.part01.rar", base.display());
    assert!(std::path::Path::new(&first).exists());

    // Default rv = 10% of the volume count (ceil).
    let nd = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".rar")
        })
        .count();
    assert!(nd >= 10, "expected a multi-volume set, got {nd} volumes");
    let status = std::process::Command::new(RAR_CLI)
        .args(["rv"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());
    let default_count = (nd * 10).div_ceil(100); // ceil(10%)
    assert!(
        std::path::Path::new(&format!("{}.part{default_count:02}.rev", base.display())).exists()
    );
    assert!(
        !std::path::Path::new(&format!(
            "{}.part{:02}.rev",
            base.display(),
            default_count + 1
        ))
        .exists()
    );

    // Count form, embedded token (`rv3`) -> 3 .rev files.
    let status = std::process::Command::new(RAR_CLI)
        .args(["rv3"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(std::path::Path::new(&format!("{}.part03.rev", base.display())).exists());
    assert!(!std::path::Path::new(&format!("{}.part04.rev", base.display())).exists());

    // Delete a volume and rebuild it with `rc`; the archive must test OK.
    let vol3 = format!("{}.part03.rar", base.display());
    std::fs::remove_file(&vol3).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["rc"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(std::path::Path::new(&vol3).exists());
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["t"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());

    // Percent form via the subcommand positional (`rv 50%`) -> ceil(50%).
    let status = std::process::Command::new(RAR_CLI)
        .args(["rv"])
        .arg(&first)
        .arg("50%")
        .status()
        .unwrap();
    assert!(status.success());
    let expected = (nd as u32 * 50).div_ceil(100) as usize;
    assert!(std::path::Path::new(&format!("{}.part{expected:02}.rev", base.display())).exists());
    assert!(
        !std::path::Path::new(&format!("{}.part{:02}.rev", base.display(), expected + 1)).exists()
    );
}

/// `rar rv` / `rar rc` round trip on a legacy RAR4 (`.rar`/`.rNN`) volume
/// set: the recovery volume gets WinRAR's legacy full-parity layout with
/// the counts in the file name, and a deleted volume is rebuilt exactly.
#[test]
fn cli_rv_and_rc_roundtrip_rar4_volume_sets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut big = vec![0u8; 400_000];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for b in &mut big {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *b = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8;
    }
    let src = dir.path().join("rnd.bin");
    std::fs::write(&src, &big).unwrap();
    let base = dir.path().join("mv4.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-v100k", "-idq"])
        .arg(&base)
        .arg(&src)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // A modern `.partNN.rar` set is addressed by its first volume, matching
    // WinRAR (which cannot open the bare base name).
    let first = dir.path().join("mv4.part1.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["rv", "-idq"])
        .arg(&first)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar rv must accept a RAR4 set");
    let rev = dir.path().join("mv4.part1.rev");
    assert!(rev.exists(), "modern RAR4 rev name expected: {rev:?}");

    // Delete a middle volume and rebuild it byte-identically.
    let victim = dir.path().join("mv4.part2.rar");
    let saved = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["rc", "-idq"])
        .arg(&first)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar rc must rebuild a RAR4 set");
    assert_eq!(std::fs::read(&victim).unwrap(), saved);

    // The rebuilt set must pass our own test.
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());
}

/// `a -ma4 -v -rv2` creates the legacy recovery volumes at close time and
/// official-style `rc` finds them after a volume loss.
#[test]
fn cli_ma4_create_with_rv_creates_recovery_volumes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut big = vec![0u8; 400_000];
    let mut x: u64 = 0xDEAD_BEEF_CAFE_F00D;
    for b in &mut big {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *b = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8;
    }
    let src = dir.path().join("rnd.bin");
    std::fs::write(&src, &big).unwrap();
    let base = dir.path().join("rv4.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-v100k", "-rv2", "-idq"])
        .arg(&base)
        .arg(&src)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(dir.path().join("rv4.part1.rev").exists());
    assert!(dir.path().join("rv4.part2.rev").exists());

    let victim = dir.path().join("rv4.part3.rar");
    let saved = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["rc", "-idq"])
        .arg(dir.path().join("rv4.part1.rar"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(&victim).unwrap(), saved);
}

/// Legacy `.partN.rar` sets print `volume N` in the totals row, like WinRAR.
#[test]
fn cli_legacy_new_numbering_totals_show_the_volume() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rar/tests/fixtures/rar40/rev3/rev_newstyle.part2.rar");
    for command in ["l", "v"] {
        let out = std::process::Command::new(RAR_CLI)
            .arg(command)
            .arg(&fixture)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{command}: {text}");
        assert!(text.contains("volume 2"), "{command}: {text}");
    }
}

/// A switch between the embedded `rv<N>` token and the archive path must not
/// be mistaken for the archive.
#[test]
fn cli_rv_embedded_count_skips_switches_before_the_archive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().join("sw");
    let src = dir.path().join("payload.bin");
    std::fs::write(&src, vec![0x21u8; 200_000]).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-v50k", "-m0", "-idq"])
        .arg(&base)
        .arg(&src)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let first = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".part1.rar"))
        })
        .expect("first volume");

    let status = std::process::Command::new(RAR_CLI)
        .args(["rv2", "-m0", "-idq"])
        .arg(&first)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "rv2 must accept a switch between the token and the archive"
    );
    let revs = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".rev"))
        })
        .count();
    assert_eq!(revs, 2, "rv2 must write two recovery volumes");
}

/// `rar i<string>` must honor `-p<password>` after the archive: external
/// commands bypass clap, so the password used to be dropped and an
/// encrypted archive could never be searched.
#[test]
fn cli_find_honors_a_password_after_the_archive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("secret.txt");
    std::fs::write(&src, b"the needle lives here").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-hp1234", "-idq"])
        .arg("enc.rar")
        .arg("secret.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    for args in [
        vec!["ineedle", "enc.rar", "-p1234"],
        vec!["-p1234", "ineedle", "enc.rar"],
    ] {
        let output = std::process::Command::new(RAR_CLI)
            .args(&args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Found"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

/// A RAR5 archive with no inline recovery record has nothing to repair
/// *with*, so the CLI reconstructs `rebuilt.<name>` from the members that
/// still decode, like WinRAR: exit 0 and no `fixed.<name>`.
#[test]
fn cli_repair_without_a_recovery_record_reconstructs() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), b"plain member").unwrap();
    let arc = dir.path().join("no-rr.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma5", "-m0", "-idq"])
        .arg(&arc)
        .arg("a.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // Quiet run: exit 0, `rebuilt.<name>` written, no `fixed.<name>`.
    let out = std::process::Command::new(RAR_CLI)
        .args(["r", "-idq"])
        .arg(&arc)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "reconstruct must succeed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !dir.path().join("fixed.no-rr.rar").exists(),
        "there is no record, so no fixed.<name>"
    );
    let rebuilt = dir.path().join("rebuilt.no-rr.rar");
    assert!(rebuilt.exists(), "reconstruct must write rebuilt.<name>");

    // The rebuilt archive is valid and carries the member.
    let test = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&rebuilt)
        .output()
        .unwrap();
    assert!(
        test.status.success(),
        "the rebuilt archive must verify:\n{}",
        String::from_utf8_lossy(&test.stderr)
    );

    // The messages name both the missing record and the rebuild, and never
    // the old doubled `repair: repair:` prefix.
    let noisy = std::process::Command::new(RAR_CLI)
        .args(["r"])
        .arg(&arc)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(noisy.status.success());
    let text = String::from_utf8_lossy(&noisy.stdout);
    assert!(text.contains("Data recovery record not found"), "{text}");
    assert!(text.contains("Reconstructing"), "{text}");
    assert!(text.contains("Found  a.txt"), "{text}");
    assert!(!text.contains("repair: repair:"), "doubled prefix: {text}");
    assert!(!text.contains("RAR format error"), "{text}");
}

/// The record's own final sector covers the tail of the protected prefix —
/// including, in a small archive, a member header. `rar r` must find that
/// damage (WinRAR's `Sector 0 (offsets 0...200) damaged - data recovered`),
/// rebuild `fixed.<name>` byte-identically, and never call it "All OK".
#[test]
fn cli_repair_recovers_damage_in_the_records_final_sector() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("f2.txt"), b"two").unwrap();
    let arc = dir.path().join("rr.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-rr10", "-idq"])
        .arg(&arc)
        .args(["f1.txt", "f2.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let intact = std::fs::read(&arc).unwrap();

    // Damage f1's FILE_HEAD (the bytes holding its name); in this archive the
    // record's own final sector is the only one covering it.
    let mut bytes = intact.clone();
    let pos = bytes
        .windows(6)
        .position(|window| window == b"f1.txt")
        .expect("f1 header name");
    bytes[pos] ^= 0xFF;
    std::fs::write(&arc, &bytes).unwrap();

    let out = std::process::Command::new(RAR_CLI)
        .args(["r"])
        .arg(&arc)
        .current_dir(dir.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("damaged - data recovered"),
        "the sector report must name the outcome: {text}"
    );
    assert!(
        text.contains("Sector 0 (offsets 0...200)"),
        "WinRAR's sector line, hex offsets: {text}"
    );
    assert!(
        !text.contains("All OK"),
        "damage must not read as healthy: {text}"
    );
    assert!(
        text.contains("Repaired"),
        "a rebuilt archive is written: {text}"
    );
    // The parity restores the archive byte for byte.
    assert_eq!(
        std::fs::read(dir.path().join("fixed.rr.rar")).unwrap(),
        intact
    );
}

/// `rar r` asks before rebuilding when a recovery record cannot repair the
/// damage (two damaged sectors in one parity group), WinRAR's
/// `Reconstruct archive structure ? [Y]es, [N]o`: `N` leaves only the report,
/// `Y` also rebuilds. Both answer with exit 3.
#[test]
fn cli_repair_asks_before_rebuilding_after_an_unusable_record() {
    for (answer, expect_rebuilt) in [("y", true), ("n", false)] {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f1.txt"), vec![b'a'; 6000]).unwrap();
        let arc = dir.path().join("rr.rar");
        // `-rr2`: two parity sectors, so sectors 0 and 2 share a parity group
        // and no single parity sector can rebuild both.
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", "-ma4", "-m0", "-rr2", "-idq"])
            .arg(&arc)
            .arg("f1.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let mut bytes = std::fs::read(&arc).unwrap();
        bytes[10] ^= 0xFF; // sector 0
        bytes[2 * 512 + 8] ^= 0xFF; // sector 2
        std::fs::write(&arc, &bytes).unwrap();

        // A terminal is not available to the test binary, so the prompt is
        // forced on and answered from a file.
        let reply = dir.path().join("reply.txt");
        std::fs::write(&reply, format!("{answer}\n")).unwrap();
        let out = std::process::Command::new(RAR_CLI)
            .args(["r"])
            .arg(&arc)
            .current_dir(dir.path())
            .env("RAR_RS_FORCE_PROMPT", "1")
            .stdin(std::fs::File::open(&reply).unwrap())
            .output()
            .unwrap();

        assert_eq!(out.status.code(), Some(3), "answer {answer}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("damaged - cannot recover data"),
            "answer {answer}: {text}"
        );
        assert!(
            text.contains("Reconstruct archive structure ? [Y]es, [N]o"),
            "answer {answer}: {text}"
        );
        assert_eq!(
            dir.path().join("rebuilt.rr.rar").exists(),
            expect_rebuilt,
            "answer {answer}: {text}"
        );
    }
}

/// A no-record *legacy* archive whose header is damaged: `rar r` resyncs past
/// it, keeps the members around it, and reports the loss with exit 3 — the
/// same signal as the RAR5 path (WinRAR answers 0 for a legacy loss, a
/// container fork we do not copy).
#[test]
fn cli_repair_salvages_a_legacy_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("f2.txt"), b"two").unwrap();
    let arc = dir.path().join("legacy.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-idq"])
        .arg(&arc)
        .args(["f1.txt", "f2.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // Corrupt f2's FILE_HEAD (the bytes holding its name).
    let mut bytes = std::fs::read(&arc).unwrap();
    let pos = bytes
        .windows(6)
        .position(|window| window == b"f2.txt")
        .expect("f2 header name");
    bytes[pos] ^= 0xFF;
    std::fs::write(&arc, &bytes).unwrap();

    let out = std::process::Command::new(RAR_CLI)
        .args(["r", "-idq"])
        .arg(&arc)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "a lost member is a data error, whatever the container:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let rebuilt = dir.path().join("rebuilt.legacy.rar");
    assert!(rebuilt.exists(), "the salvage must write rebuilt.<name>");
    let list = std::process::Command::new(RAR_CLI)
        .args(["lb"])
        .arg(&rebuilt)
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&list.stdout);
    assert!(names.contains("f1.txt"), "salvaged member: {names}");
    assert!(!names.contains("f2.txt"), "corrupt member dropped: {names}");

    let test = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&rebuilt)
        .output()
        .unwrap();
    assert!(
        test.status.success(),
        "the salvaged legacy archive must verify:\n{}",
        String::from_utf8_lossy(&test.stderr)
    );
}

/// A no-record archive whose *header* is damaged: `rar r` resyncs past the
/// corrupt block, salvages the members around it, and exits 3 like WinRAR.
#[test]
fn cli_repair_salvages_past_a_corrupt_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("f2.txt"), b"two").unwrap();
    let arc = dir.path().join("dmg.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma5", "-m0", "-idq"])
        .arg(&arc)
        .args(["f1.txt", "f2.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // Corrupt f2's FILE_HEADER (the bytes holding its name).
    let mut bytes = std::fs::read(&arc).unwrap();
    let pos = bytes
        .windows(6)
        .position(|window| window == b"f2.txt")
        .expect("f2 header name");
    bytes[pos] ^= 0xFF;
    std::fs::write(&arc, &bytes).unwrap();

    let out = std::process::Command::new(RAR_CLI)
        .args(["r", "-idq"])
        .arg(&arc)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "a corrupt header exits 3 like WinRAR:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let rebuilt = dir.path().join("rebuilt.dmg.rar");
    assert!(rebuilt.exists(), "the salvage must write rebuilt.<name>");
    let list = std::process::Command::new(RAR_CLI)
        .args(["lb"])
        .arg(&rebuilt)
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&list.stdout);
    assert!(names.contains("f1.txt"), "salvaged member: {names}");
    assert!(!names.contains("f2.txt"), "corrupt member dropped: {names}");

    let test = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&rebuilt)
        .output()
        .unwrap();
    assert!(
        test.status.success(),
        "the salvaged archive must verify:\n{}",
        String::from_utf8_lossy(&test.stderr)
    );
}
