use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir};
// ── WinRAR CLI parity batch 2: -x@/-n@, -ta/-tb, -ag, -ep2/-ep3, -r0 ──────

#[test]
fn cli_mask_list_file_excludes_loaded_masks() {
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("keep.txt"), b"k").unwrap();
    std::fs::write(src.join("drop.tmp"), b"d").unwrap();
    std::fs::write(dir.path().join("masks.lst"), b"*.tmp\n").unwrap();

    let archive = dir.path().join("x.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(format!("-x@{}", dir.path().join("masks.lst").display()))
        .arg(&archive)
        .arg("src")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "src/keep.txt"), "{names:?}");
    assert!(
        !names.iter().any(|n| n == "src/drop.tmp"),
        "mask list must exclude *.tmp: {names:?}"
    );
}

#[test]
fn cli_time_filter_after_only_adds_newer_files() {
    let dir = make_temp_dir();
    let old = dir.path().join("old.txt");
    let new = dir.path().join("new.txt");
    std::fs::write(&old, b"o").unwrap();
    std::fs::write(&new, b"n").unwrap();
    let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    let times = std::fs::FileTimes::new().set_modified(past);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_times(times)
        .unwrap();

    let archive = dir.path().join("ta.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ta20200101", "-idq"])
        .arg(&archive)
        .arg("old.txt")
        .arg("new.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "new.txt"), "{names:?}");
    assert!(
        !names.iter().any(|n| n == "old.txt"),
        "-ta must drop older files: {names:?}"
    );
}

#[test]
fn cli_auto_name_inserts_date_stamp() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ag", "-idq"])
        .arg(dir.path().join("auto.rar"))
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    // The stamp (YYYYMMDDHHMMSS) is inserted before the extension.
    let created: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("auto") && n.ends_with(".rar") && n.len() > "auto.rar".len()
        })
        .collect();
    assert_eq!(created.len(), 1, "one stamped archive expected");
    let name = created[0].file_name().to_string_lossy().into_owned();
    let stamp = &name["auto".len()..name.len() - ".rar".len()];
    assert_eq!(stamp.len(), 14, "stamp must be YYYYMMDDHHMMSS: {name}");
    assert!(stamp.chars().all(|c| c.is_ascii_digit()), "{name}");
}

#[test]
fn cli_full_paths_ep2_ep3() {
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.txt"), b"f").unwrap();

    let archive = dir.path().join("ep2.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep2", "-idq"])
        .arg(&archive)
        .arg(src.join("f.txt"))
        .status()
        .unwrap();
    assert!(status.success());
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    let _stored = names[0].clone();
    #[cfg(windows)]
    assert!(
        !_stored.starts_with([
            'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q',
            'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z'
        ]) || !_stored.contains(":/"),
        "-ep2 must drop the drive letter: {_stored}"
    );

    let archive3 = dir.path().join("ep3.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep3", "-idq"])
        .arg(&archive3)
        .arg(src.join("f.txt"))
        .status()
        .unwrap();
    assert!(status.success());
    let rar = rar_rs::ArchiveReader::open(&archive3).unwrap();
    let _stored = rar.entries().next().unwrap().name().to_string();
    #[cfg(windows)]
    assert!(
        _stored.contains("_/") || _stored.contains("_/"),
        "-ep3 must keep the drive as X_: {_stored}"
    );
}

#[test]
fn cli_recurse_zero_does_not_descend_wildcards() {
    let dir = make_temp_dir();
    let src = dir.path().join("r0src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("top.txt"), b"t").unwrap();
    std::fs::write(src.join("sub").join("deep.txt"), b"d").unwrap();

    let archive = dir.path().join("r0.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-r0", "-idq"])
        .arg(&archive)
        .arg("r0src/*")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    assert!(
        names.iter().any(|n| n == "r0src/top.txt"),
        "-r0 must match top-level files: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("deep.txt")),
        "-r0 must not descend into matched dirs: {names:?}"
    );
}

/// `-ol` stores symbolic links as redirect records (unix-only: creating
/// the source symlink needs symlink(2)).
#[test]
#[cfg(unix)]
fn cli_links_ol_stores_symlink_redirects() {
    let dir = make_temp_dir();
    let src = dir.path().join("lnk");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("target.txt"), b"target").unwrap();
    std::os::unix::fs::symlink("target.txt", src.join("lnk.txt")).unwrap();

    let archive = dir.path().join("ol.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ol", "-idq"])
        .arg(&archive)
        .arg("lnk")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "lnk/target.txt"), "{names:?}");
    assert!(names.iter().any(|n| n == "lnk/lnk.txt"), "{names:?}");
    // The link member carries no data (redirect record), and extraction
    // recreates the symlink.
    let lnk_id = rar.unique_entry("lnk/lnk.txt").unwrap();
    let entry = rar.entry(lnk_id).unwrap();
    assert_eq!(entry.size(), 0);
    let out = dir.path().join("out");
    rar.extract_all_with_options(&out, rar_rs::ExtractOptions::default())
        .unwrap();
    let link = std::fs::read_link(out.join("lnk/lnk.txt")).unwrap();
    assert_eq!(link, std::path::Path::new("target.txt"));
}

// ── Audit batch 2026-09-13 (2): -w, -htc, -ad1/2, -sfx at create ─────────

/// `-w<p>` only names the directory RAR uses for temporary files (WinRAR
/// semantics); it must never change where the archive is written. A
/// missing directory is rejected before anything is created.
#[test]
fn cli_work_dir_does_not_relocate_outputs() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"w").unwrap();
    std::fs::create_dir(dir.path().join("work")).unwrap();
    let archive = dir.path().join("w.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq", "-wwork"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        archive.exists(),
        "archive must stay in the working directory"
    );
    assert!(!dir.path().join("work").join("w.rar").exists());

    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq", "-wmissing-dir"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "a missing work directory must be rejected"
    );
}

/// `-htc` (the default CRC32 hash) is accepted on every command, like
/// WinRAR's parser.
#[test]
fn cli_htc_is_accepted_on_read_commands() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"h").unwrap();
    let archive = dir.path().join("h.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    for command in ["t", "l", "x"] {
        let mut probe = std::process::Command::new(RAR_CLI);
        probe.args([command, "-htc", "-idq"]).arg(&archive);
        if command == "x" {
            probe.arg("--dest").arg(dir.path().join("out"));
        }
        let status = probe.current_dir(dir.path()).status().unwrap();
        assert!(status.success(), "{command} -htc must be accepted");
    }
}

/// `-ad1` puts each archive into its own directory next to the archive;
/// `-ad2` extracts straight into the archive's directory. Both ignore the
/// destination parameter.
#[test]
fn cli_ad1_ad2_pick_the_archive_directory() {
    let dir = make_temp_dir();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("g.txt"), b"g").unwrap();
    let archive = dir.path().join("sub").join("a.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("sub/g.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    for (mode, expected) in [
        (
            "-ad",
            dir.path().join("out").join("a").join("sub").join("g.txt"),
        ),
        (
            "-ad1",
            dir.path().join("sub").join("a").join("sub").join("g.txt"),
        ),
        ("-ad2", dir.path().join("sub").join("sub").join("g.txt")),
    ] {
        let out = dir.path().join("out");
        let _ = std::fs::remove_dir_all(&out);
        let status = std::process::Command::new(RAR_CLI)
            .args(["x", mode, "-idq", "--dest"])
            .arg(&out)
            .arg(&archive)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{mode} extraction");
        assert!(expected.exists(), "{mode}: expected {}", expected.display());
        let _ = std::fs::remove_file(&expected);
    }
}

/// `a -sfx` prepends the default SFX module at create time (skipped when
/// no module is installed).
#[test]
fn cli_create_sfx_prepends_module() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"s").unwrap();
    let exe = dir.path().join("out.exe");
    let output = std::process::Command::new(RAR_CLI)
        .args(["a", "-sfx", "-idq"])
        .arg(&exe)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if text.contains("default.sfx not found") {
            eprintln!("skipped: no SFX module installed");
            return;
        }
        panic!("a -sfx failed: {text}");
    }
    let head = std::fs::read(&exe).unwrap();
    assert_eq!(&head[..2], b"MZ", "SFX output must carry the module");

    // Our own extractor reads the SFX archive.
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&exe)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "unrar t on the created SFX");
}

/// Overwrite policy: with no `-y`/`-o±` (no interactive prompt) extraction
/// skips existing files, `-y`/`-o+` overwrite and `-o-` skips, matching
/// WinRAR's non-interactive outcomes.
#[test]
fn cli_extract_overwrite_defaults_to_skip() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"new").unwrap();
    let archive = dir.path().join("ow.rar");
    assert!(
        std::process::Command::new(RAR_CLI)
            .args(["a", "-idq"])
            .arg(&archive)
            .arg("f.txt")
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out_file = out.join("f.txt");

    for (label, extra, expected, code) in [
        ("default", vec!["-idq"], "old", 10),
        ("-y", vec!["-y", "-idq"], "new", 0),
        ("-o+", vec!["-o+", "-idq"], "new", 0),
        ("-o-", vec!["-o-", "-idq"], "old", 10),
    ] {
        std::fs::write(&out_file, b"old").unwrap();
        let mut command = std::process::Command::new(UNRAR_CLI);
        command.args(["x"]).args(&extra).arg(&archive);
        command.arg("--dest").arg(&out);
        let status = command.current_dir(dir.path()).status().unwrap();
        assert_eq!(
            status.code(),
            Some(code),
            "{label}: an all-skipped run exits 10 like official UnRAR"
        );
        assert_eq!(
            std::fs::read(&out_file).unwrap(),
            expected.as_bytes(),
            "{label}"
        );
    }
}

/// `l` / `v` / `lt` render WinRAR's table shape: the `Archive:`/`Details:`
/// preamble, attribute column and per-member technical blocks.
#[test]
fn list_tables_follow_the_official_shape() {
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
    std::fs::write(src.join("c.txt"), b"nested").unwrap();

    let archive = dir.path().join("shape.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "-idq"])
        .arg(&archive)
        .arg("a.txt")
        .arg("src")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let listing = std::process::Command::new(RAR_CLI)
        .arg("l")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(text.contains("Archive:"), "{text}");
    assert!(text.contains("Details: RAR 5"), "{text}");
    assert!(
        text.contains(" Attributes       Size     Date    Time   Name"),
        "{text}"
    );
    assert!(
        text.contains("----------- ----------  ---------- -----  ----"),
        "{text}"
    );
    // The attribute cell follows the host marker the writer stored: Unix
    // modes on Unix, DOS flags on Windows (like WinRAR on each platform).
    let (file_attrs, dir_attrs) = if cfg!(windows) {
        ("..A....", "...D...")
    } else {
        ("-rw-r--r--", "drwxr-xr-x")
    };
    assert!(text.contains(file_attrs), "{text}");
    assert!(text.contains(dir_attrs), "{text}");

    let verbose = std::process::Command::new(RAR_CLI)
        .arg("v")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&verbose.stdout);
    assert!(text.contains("Checksum"), "{text}");

    let tech = std::process::Command::new(RAR_CLI)
        .arg("lt")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&tech.stdout);
    let nested = if cfg!(windows) {
        "src\\c.txt"
    } else {
        "src/c.txt"
    };
    assert!(text.contains(&format!("        Name: {nested}")), "{text}");
    assert!(text.contains("        Type: File"), "{text}");
    assert!(text.contains(" Compression: RAR 5.0(v50) -m0"), "{text}");
    let host = if cfg!(windows) {
        "     Host OS: Windows"
    } else {
        "     Host OS: Unix"
    };
    assert!(text.contains(host), "{text}");

    // Unknown `-z...` values are accepted and ignored outside the comment
    // commands, like WinRAR.
    let ignored = std::process::Command::new(RAR_CLI)
        .args(["l", "-zz"])
        .arg(&archive)
        .output()
        .unwrap();
    assert!(ignored.status.success());
    assert!(
        String::from_utf8_lossy(&ignored.stdout).contains("Archive:"),
        "{}",
        String::from_utf8_lossy(&ignored.stdout)
    );
}

/// Redirect members (`-oh` hardlinks) keep the link's modification time,
/// like official `rar`; the previous writer stored `mtime = 0` (1970).
#[test]
fn cli_hardlink_redirects_keep_the_file_mtime() {
    let dir = make_temp_dir();
    let first = dir.path().join("h1.txt");
    std::fs::write(&first, b"hard link payload").unwrap();
    std::fs::hard_link(&first, dir.path().join("h2.txt")).unwrap();

    let archive = dir.path().join("links.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "--hardlinks", "-idq"])
        .arg(&archive)
        .arg("h1.txt")
        .arg("h2.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "create the hardlink archive");

    let expected = std::fs::metadata(&first)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let id = rar.unique_entry("h2.txt").unwrap();
    let entry = rar.entry(id).unwrap();
    assert_eq!(
        entry.mtime(),
        expected.as_secs() as u32,
        "the redirect must carry the link's mtime"
    );
    assert_ne!(entry.mtime(), 0, "mtime must not be the 1970 default");

    // `lt` renders the link kind and its target, like WinRAR.
    let tech = std::process::Command::new(RAR_CLI)
        .arg("lt")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&tech.stdout);
    assert!(text.contains("Type: Hard link"), "{text}");
    assert!(text.contains("Target: h1.txt"), "{text}");
}

/// WinRAR's volume-set listing semantics: only the members with data in the
/// opened volume, fragment packed/ratio/CRC columns and a numbered
/// `Details:` suffix.
#[test]
fn cli_volume_set_listing_matches_winrar() {
    let dir = make_temp_dir();
    let mut state = 0x9e37_79b9u32;
    let big: Vec<u8> = (0..30_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect();
    std::fs::write(dir.path().join("big.bin"), &big).unwrap();
    std::fs::write(dir.path().join("tail.txt"), b"tail member\r\n").unwrap();

    let archive = dir.path().join("mv.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "--volume-size=8k", "-idq"])
        .arg(&archive)
        .arg("big.bin")
        .arg("tail.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let volumes = rar_rs::discover_volumes(&archive);
    assert!(volumes.len() >= 2, "{} volumes", volumes.len());

    let run = |command: &str, target: &std::path::Path| {
        let out = std::process::Command::new(RAR_CLI)
            .arg(command)
            .arg(target)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let first = run("l", &archive);
    assert!(first.contains("Details: RAR 5, volume 1"), "{first}");
    assert!(first.contains("big.bin"), "{first}");
    assert!(
        !first.contains("tail.txt"),
        "volume 1 must not list members that start later:\n{first}"
    );

    let verbose = run("v", &archive);
    assert!(verbose.contains("-->"), "first fragment marker:\n{verbose}");

    let last = volumes.last().unwrap();
    let verbose = run("v", last);
    assert!(verbose.contains("tail.txt"), "{verbose}");
    assert!(verbose.contains("<--"), "last fragment marker:\n{verbose}");
    let listing = run("l", last);
    assert!(
        listing.contains(&format!("Details: RAR 5, volume {}", volumes.len())),
        "{listing}"
    );
}

/// Solid archives carry the `, solid` suffix and mark chain continuations
/// with WinRAR's `Flags: solid` line.
#[test]
fn cli_solid_listing_shows_the_archive_and_member_flags() {
    let dir = make_temp_dir();
    std::fs::write(
        dir.path().join("a.txt"),
        b"solid first member payload ".repeat(40),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.txt"),
        b"solid second member payload ".repeat(40),
    )
    .unwrap();
    let archive = dir.path().join("solid.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-s", "-m5", "-idq"])
        .arg(&archive)
        .arg("a.txt")
        .arg("b.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let listing = std::process::Command::new(RAR_CLI)
        .arg("l")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(text.contains("Details: RAR 5, solid"), "{text}");

    let tech = std::process::Command::new(RAR_CLI)
        .arg("lt")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&tech.stdout);
    assert!(text.contains("       Flags: solid "), "{text}");
}

/// Members without a stored timestamp render `????-??-?? ??:??` in the
/// tables and omit the `Modified:` line in `lt`, like WinRAR.
#[test]
fn cli_listing_marks_missing_timestamps() {
    let dir = make_temp_dir();
    let archive = dir.path().join("nomtime.rar");
    {
        let mut writer =
            rar_rs::ArchiveWriter::create_with(&archive, rar_rs::WriterOptions::new()).unwrap();
        writer
            .add_bytes("file.txt", b"payload", rar_rs::EntryWriteOptions::new())
            .unwrap();
        // A redirect created without a time (the pre-2026-09 CLI shape).
        writer.add_redirect("link.txt", 1, "file.txt").unwrap();
        writer.finish().unwrap();
    }

    let listing = std::process::Command::new(RAR_CLI)
        .arg("l")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(text.contains("????-??-?? ??:??"), "{text}");

    let tech = std::process::Command::new(RAR_CLI)
        .arg("lt")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&tech.stdout);
    let link_block = text
        .split("\n\n")
        .find(|block| block.contains("Name: link.txt"))
        .unwrap_or_default();
    assert!(
        !link_block.contains("Modified:"),
        "a member without time has no Modified line:\n{text}"
    );
    assert!(
        text.contains("Modified:"),
        "file.txt keeps its time:\n{text}"
    );
}

/// WinRAR accepts the reset modes both bare (`-se`) and as `-s=e`, and the
/// switch is tolerated on read commands.
#[test]
fn cli_solid_reset_alias_forms() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("x1.txt"), b"solid ".repeat(400)).unwrap();
    std::fs::write(dir.path().join("x2.bin"), b"binary ".repeat(400)).unwrap();

    let a = dir.path().join("alias-a.rar");
    let b = dir.path().join("alias-b.rar");
    for (archive, flag) in [(&a, "-s=e"), (&b, "-se")] {
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", flag, "-m5", "-idq"])
            .arg(archive)
            .arg("x1.txt")
            .arg("x2.bin")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{flag} must be accepted");
    }
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "-s=e and -se must select the same reset mode"
    );

    let listing = std::process::Command::new(RAR_CLI)
        .args(["l", "-se"])
        .arg(&a)
        .output()
        .unwrap();
    assert!(listing.status.success(), "read commands accept -se");

    let rejected = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-s=v", "-m0", "-idq"])
        .arg(dir.path().join("reject.rar"))
        .arg("x1.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(
        !rejected.success(),
        "-ma4 -s=v must be rejected (unrepresentable reset)"
    );
}

/// A directory symlink is stored as a redirect with `-ol` instead of being
/// walked (the previous behavior duplicated the target's contents).
#[cfg(windows)]
#[test]
fn cli_directory_symlinks_are_stored_as_redirects() {
    use std::os::windows::fs::symlink_dir;
    let dir = make_temp_dir();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::fs::write(dir.path().join("real/inner.txt"), b"inner").unwrap();
    symlink_dir(dir.path().join("real"), dir.path().join("link")).unwrap();

    let archive = dir.path().join("links.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "-ol", "-idq"])
        .arg(&archive)
        .arg("link")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let bare = std::process::Command::new(RAR_CLI)
        .args(["lb"])
        .arg(&archive)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&bare.stdout).trim(), "link");

    let tech = std::process::Command::new(RAR_CLI)
        .args(["lt"])
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&tech.stdout);
    assert!(text.contains("Type: Windows symbolic link"), "{text}");
    assert!(text.contains("Target:"), "{text}");
    assert!(text.contains("real"), "{text}");
    assert!(
        !text.contains("inner.txt"),
        "the target must not be walked:\n{text}"
    );
}

/// A Windows junction is stored as a redirect type 3 and the technical
/// listing uses the official `NTFS junction point` label, not a Windows
/// symlink.
#[cfg(windows)]
#[test]
fn cli_junctions_use_the_official_type_label() {
    let dir = make_temp_dir();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::fs::write(dir.path().join("real/inner.txt"), b"inner").unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(dir.path().join("link"))
        .arg(dir.path().join("real"))
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");

    let archive = dir.path().join("junc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "-ol", "-idq"])
        .arg(&archive)
        .arg("link")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let tech = std::process::Command::new(RAR_CLI)
        .args(["lt"])
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&tech.stdout);
    assert!(text.contains("Type: NTFS junction point"), "{text}");
    assert!(text.contains("Target:"), "{text}");
    assert!(
        !text.contains("inner.txt"),
        "the target must not be walked:\n{text}"
    );
}

/// A stored junction (redirect type 3) must be recreated as a real NTFS
/// mount point, not as a directory *symlink*: creating a symlink needs
/// `SeCreateSymbolicLinkPrivilege`, which ordinary users do not hold, so the
/// old behavior failed to extract junctions for them. The extracted entry is
/// checked through `FindFirstFileW`'s reparse tag, which distinguishes the
/// two, and the link must still resolve to the archived target.
#[cfg(windows)]
#[test]
fn cli_extracts_a_junction_as_a_real_mount_point() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

    fn reparse_tag(path: &std::path::Path) -> u32 {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
        assert_ne!(handle, INVALID_HANDLE_VALUE, "FindFirstFileW failed");
        unsafe { FindClose(handle) };
        data.dwReserved0
    }

    let dir = make_temp_dir();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::fs::write(dir.path().join("real/inner.txt"), b"inner").unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(dir.path().join("link"))
        .arg(dir.path().join("real"))
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");
    assert_eq!(
        reparse_tag(&dir.path().join("link")),
        IO_REPARSE_TAG_MOUNT_POINT,
        "the source fixture must be a junction"
    );

    let archive = dir.path().join("junc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "-ol", "-idq"])
        .arg(&archive)
        .arg("link")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "archiving the junction must succeed");

    let out = dir.path().join("out");
    let status = std::process::Command::new(RAR_CLI)
        // `-ola`: a junction's target is absolute, and the safe-path policy
        // refuses absolute targets unless unsafe links were asked for (the
        // same rule official UnRAR applies). The safety policy itself is
        // covered by the `-ola` extraction tests elsewhere.
        .args(["x", "-ola", "-idq"])
        .arg(&archive)
        .arg("--dest")
        .arg(&out)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "extraction must succeed");

    let extracted = out.join("link");
    assert_eq!(
        reparse_tag(&extracted),
        IO_REPARSE_TAG_MOUNT_POINT,
        "the extracted redirect must be a real NTFS mount point, not a symlink"
    );
    // The recreated junction must point at the archived target.
    assert_eq!(
        std::fs::read(extracted.join("inner.txt")).unwrap(),
        b"inner",
        "the extracted junction must resolve to the archived target"
    );
}
