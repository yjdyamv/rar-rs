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

    for (label, extra, expected) in [
        ("default", vec!["-idq"], "old"),
        ("-y", vec!["-y", "-idq"], "new"),
        ("-o+", vec!["-o+", "-idq"], "new"),
        ("-o-", vec!["-o-", "-idq"], "old"),
    ] {
        std::fs::write(&out_file, b"old").unwrap();
        let mut command = std::process::Command::new(UNRAR_CLI);
        command.args(["x"]).args(&extra).arg(&archive);
        command.arg("--dest").arg(&out);
        assert!(
            command.current_dir(dir.path()).status().unwrap().success(),
            "{label}"
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
    assert!(text.contains("-rw-r--r--"), "{text}");
    assert!(text.contains("drwxr-xr-x"), "{text}");

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
    assert!(text.contains("     Host OS: Unix"), "{text}");

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
