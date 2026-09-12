use rar_rs::{CompressionLevel, EntryWriteOptions};

use crate::support::{
    RAR_CLI, UNRAR_CLI, make_temp_dir, pseudo_random_bytes, set_mtime_ago, write_rep_text,
};
// ── update/freshen and miscellaneous switches ─────────────────────────────

#[test]
fn cli_version_control_keeps_previous_versions() {
    let dir = make_temp_dir();
    let file = dir.path().join("ver.txt");
    let archive = dir.path().join("ver.rar");
    let base = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);

    // Pin the source mtimes instead of sleeping between updates: `u`
    // compares them with the archive's stored stamps, so crossing a
    // second boundary is all that matters.
    std::fs::write(&file, b"v1").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(base)
        .unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let update = |content: &[u8], seconds: u64, flag: &str| {
        std::fs::write(&file, content).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(base + std::time::Duration::from_secs(seconds))
            .unwrap();
        let status = std::process::Command::new(RAR_CLI)
            .args(["u", flag, "-idq"])
            .arg(&archive)
            .arg("ver.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "u {flag} must succeed");
    };

    // First update with -ver: old version kept as `ver.txt;1`.
    update(b"v2", 10, "-ver");
    {
        let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let vt_id = rar.unique_entry("ver.txt").unwrap();
        assert_eq!(rar.read_entry(vt_id).unwrap(), b"v2");
        let vt1_id = rar.unique_entry("ver.txt;1").unwrap();
        assert_eq!(rar.read_entry(vt1_id).unwrap(), b"v1");
    }

    // Second update: the chain shifts (ver.txt;1 -> ver.txt;2).
    update(b"v3", 20, "-ver");
    {
        let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let vt_id = rar.unique_entry("ver.txt").unwrap();
        assert_eq!(rar.read_entry(vt_id).unwrap(), b"v3");
        let vt1_id = rar.unique_entry("ver.txt;1").unwrap();
        assert_eq!(rar.read_entry(vt1_id).unwrap(), b"v2");
        let vt2_id = rar.unique_entry("ver.txt;2").unwrap();
        assert_eq!(rar.read_entry(vt2_id).unwrap(), b"v1");
    }

    // -ver1 caps the history at one previous version.
    update(b"v4", 30, "-ver1");
    {
        let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let vt_id = rar.unique_entry("ver.txt").unwrap();
        assert_eq!(rar.read_entry(vt_id).unwrap(), b"v4");
        let vt1_id = rar.unique_entry("ver.txt;1").unwrap();
        assert_eq!(rar.read_entry(vt1_id).unwrap(), b"v3");
        assert!(!rar.entries().any(|e| e.name() == "ver.txt;2"));
    }
}

#[test]
fn cli_lock_command_freezes_the_archive_against_edits() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("keep.txt"), b"k").unwrap();
    let archive = dir.path().join("locked.rar");

    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("keep.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let status = std::process::Command::new(RAR_CLI)
        .args(["k"])
        .arg(&archive)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar k must succeed on a normal archive");

    // The locked archive refuses further edits but stays readable.
    let status = std::process::Command::new(RAR_CLI)
        .args(["d", "-idq"])
        .arg(&archive)
        .arg("keep.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(!status.success(), "rar d must refuse a locked archive");
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let keep_id = rar.unique_entry("keep.txt").unwrap();
    assert_eq!(rar.read_entry(keep_id).unwrap(), b"k");
    drop(rar);
    match rar_rs::ArchiveWriter::append(&archive) {
        Err(rar_rs::RarError::ArchiveLocked) => {}
        Err(e) => panic!("expected ArchiveLocked after rar k, got {e:?}"),
        Ok(_) => panic!("expected ArchiveLocked after rar k"),
    }
}

#[test]
fn cli_update_pure_addition_adds_the_missing_member() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("seed.txt"), b"seed").unwrap();
    std::fs::write(dir.path().join("added.txt"), b"added").unwrap();
    let archive = dir.path().join("update-add.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("seed.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-idq"])
        .arg(&archive)
        .arg("added.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let seed_id = rar.unique_entry("seed.txt").unwrap();
    assert_eq!(rar.read_entry(seed_id).unwrap(), b"seed");
    let added_id = rar.unique_entry("added.txt").unwrap();
    assert_eq!(rar.read_entry(added_id).unwrap(), b"added");
}

#[test]
fn cli_update_replaces_newer_members_and_adds_missing_members() {
    let dir = make_temp_dir();
    let existing = dir.path().join("existing.txt");
    std::fs::write(&existing, b"old").unwrap();
    set_mtime_ago(&existing, 120);
    let archive = dir.path().join("update-replace.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("existing.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    std::fs::write(&existing, b"new").unwrap();
    std::fs::write(dir.path().join("added.txt"), b"added").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-idq"])
        .arg(&archive)
        .args(["existing.txt", "added.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let existing_id = rar.unique_entry("existing.txt").unwrap();
    assert_eq!(rar.read_entry(existing_id).unwrap(), b"new");
    let added_id = rar.unique_entry("added.txt").unwrap();
    assert_eq!(rar.read_entry(added_id).unwrap(), b"added");
}

#[test]
fn cli_freshen_replaces_existing_members_without_adding_missing_members() {
    let dir = make_temp_dir();
    let existing = dir.path().join("existing.txt");
    std::fs::write(&existing, b"old").unwrap();
    set_mtime_ago(&existing, 120);
    let archive = dir.path().join("freshen.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("existing.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    std::fs::write(&existing, b"new").unwrap();
    std::fs::write(dir.path().join("missing.txt"), b"missing").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["f", "-idq"])
        .arg(&archive)
        .args(["existing.txt", "missing.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let existing_id = rar.unique_entry("existing.txt").unwrap();
    assert_eq!(rar.read_entry(existing_id).unwrap(), b"new");
    assert!(!rar.entries().any(|e| e.name() == "missing.txt"));
}

#[test]
fn cli_update_and_freshen_expand_directory_arguments() {
    let dir = make_temp_dir();

    let update_tree = dir.path().join("update-tree");
    std::fs::create_dir_all(&update_tree).unwrap();
    std::fs::write(update_tree.join("changed.txt"), b"old").unwrap();
    std::fs::write(update_tree.join("unchanged.txt"), b"same").unwrap();
    set_mtime_ago(&update_tree.join("changed.txt"), 120);
    set_mtime_ago(&update_tree.join("unchanged.txt"), 120);
    let update_archive = dir.path().join("directory-update.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&update_archive)
        .arg("update-tree")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    std::fs::write(update_tree.join("changed.txt"), b"new").unwrap();
    std::fs::write(update_tree.join("added.txt"), b"added").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-idq"])
        .arg(&update_archive)
        .arg("update-tree")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&update_archive).unwrap();
    let changed_id = rar.unique_entry("update-tree/changed.txt").unwrap();
    assert_eq!(rar.read_entry(changed_id).unwrap(), b"new");
    let unchanged_id = rar.unique_entry("update-tree/unchanged.txt").unwrap();
    assert_eq!(rar.read_entry(unchanged_id).unwrap(), b"same");
    let added_id = rar.unique_entry("update-tree/added.txt").unwrap();
    assert_eq!(rar.read_entry(added_id).unwrap(), b"added");

    let freshen_tree = dir.path().join("freshen-tree");
    std::fs::create_dir_all(&freshen_tree).unwrap();
    std::fs::write(freshen_tree.join("changed.txt"), b"old").unwrap();
    std::fs::write(freshen_tree.join("unchanged.txt"), b"same").unwrap();
    set_mtime_ago(&freshen_tree.join("changed.txt"), 120);
    set_mtime_ago(&freshen_tree.join("unchanged.txt"), 120);
    let freshen_archive = dir.path().join("directory-freshen.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&freshen_archive)
        .arg("freshen-tree")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    std::fs::write(freshen_tree.join("changed.txt"), b"new").unwrap();
    std::fs::write(freshen_tree.join("missing.txt"), b"missing").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["f", "-idq"])
        .arg(&freshen_archive)
        .arg("freshen-tree")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&freshen_archive).unwrap();
    let changed_id = rar.unique_entry("freshen-tree/changed.txt").unwrap();
    assert_eq!(rar.read_entry(changed_id).unwrap(), b"new");
    let unchanged_id = rar.unique_entry("freshen-tree/unchanged.txt").unwrap();
    assert_eq!(rar.read_entry(unchanged_id).unwrap(), b"same");
    assert!(
        !rar.entries()
            .any(|e| e.name() == "freshen-tree/missing.txt")
    );
}

#[test]
fn cli_failed_update_preserves_the_original_archive() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("existing.txt"), b"original").unwrap();
    let archive = dir.path().join("transaction.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("existing.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let original = std::fs::read(&archive).unwrap();

    let out = std::process::Command::new(RAR_CLI)
        .args(["u", "-idq"])
        .arg(&archive)
        .arg("missing.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read(&archive).unwrap(), original);

    let out = std::process::Command::new(RAR_CLI)
        .args(["u", "-md3m", "-idq"])
        .arg(&archive)
        .arg("existing.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read(&archive).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("rar-rs-update"))
            .count(),
        0
    );
}

#[test]
fn cli_update_rejects_multi_volume_archives_without_modifying_them() {
    let dir = make_temp_dir();
    std::fs::write(
        dir.path().join("payload.bin"),
        pseudo_random_bytes(16 * 1024, 91),
    )
    .unwrap();
    std::fs::write(dir.path().join("added.txt"), b"added").unwrap();
    let base = dir.path().join("multi");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-v1k", "-idq"])
        .arg(&base)
        .arg("payload.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut volumes: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rar"))
        .collect();
    volumes.sort();
    assert!(volumes.len() > 1, "expected a multi-volume archive");
    let original: Vec<Vec<u8>> = volumes
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();

    let out = std::process::Command::new(RAR_CLI)
        .args(["u", "-idq"])
        .arg(&volumes[0])
        .arg("added.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("multi-volume"));
    for (path, expected) in volumes.iter().zip(original) {
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }
}

#[test]
fn cli_member_selection_uses_exact_paths_or_basenames() {
    let dir = make_temp_dir();
    let archive = dir.path().join("selectors.rar");
    {
        let mut rar =
            rar_rs::ArchiveWriter::create_with(&archive, rar_rs::WriterOptions::default()).unwrap();
        rar.add_bytes(
            "a",
            b"A",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.add_bytes(
            "dir/base.txt",
            b"BASE",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.add_bytes(
            "full/path.txt",
            b"FULL",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }

    for binary in [RAR_CLI, UNRAR_CLI] {
        let out = std::process::Command::new(binary)
            .args(["x", "-so"])
            .arg(&archive)
            .arg("data")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{binary} must not match a as data");
        assert!(out.stdout.is_empty());

        let out = std::process::Command::new(binary)
            .args(["x", "-so"])
            .arg(&archive)
            .arg("base.txt")
            .output()
            .unwrap();
        assert!(out.status.success(), "{binary} basename selection failed");
        assert_eq!(out.stdout, b"BASE");

        let out = std::process::Command::new(binary)
            .args(["x", "-so"])
            .arg(&archive)
            .arg("full/path.txt")
            .output()
            .unwrap();
        assert!(out.status.success(), "{binary} full-path selection failed");
        assert_eq!(out.stdout, b"FULL");

        let out = std::process::Command::new(binary)
            .args(["x", "-so"])
            .arg(&archive)
            .arg("full\\path.txt")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{binary} backslash path selection failed"
        );
        assert_eq!(out.stdout, b"FULL");
    }
}

#[test]
fn unrar_stdout_honors_the_extraction_dictionary_limit() {
    let dir = make_temp_dir();
    let source = dir.path().join("dict.bin");
    write_rep_text(&source, 8 * 1024 * 1024);
    let archive = dir.path().join("dict-limit.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma7", "-md16m", "-idq"])
        .arg(&archive)
        .arg("dict.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let dict_id = rar.unique_entry("dict.bin").unwrap();
    assert!(rar.entry(dict_id).unwrap().dict_size_bytes() > Some(8 * 1024 * 1024));

    let out = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-so", "-mdx8m"])
        .arg(&archive)
        .arg("dict.bin")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("dictionary"));
}

#[test]
fn cli_misc_switches_are_accepted_and_ilog_logs_errors() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("m.txt"), b"m").unwrap();

    // Platform-specific / informational switches are accepted.
    let archive = dir.path().join("misc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args([
            "a", "-idc", "-idd", "-idn", "-idp", "-ac", "-ai", "-os", "-scu", "-oni", "-ri5",
            "-vp", "-oi1", "-ams", "-e1", "-ow", "-idq",
        ])
        .arg(&archive)
        .arg("m.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "misc switches must be accepted");

    // Destructive switches whose semantics are not implemented must fail
    // explicitly and leave source data untouched.
    for switch in ["-vd", "-dw", "-dr"] {
        let out = std::process::Command::new(RAR_CLI)
            .args(["a", switch, "-idq"])
            .arg(dir.path().join(format!("unsafe-{}.rar", &switch[1..])))
            .arg("m.txt")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{switch} must be rejected");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("not supported"),
            "{switch}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dir.path().join("m.txt").exists());
    }

    // -ilog writes the error to the log file.
    let log = dir.path().join("err.log");
    let out = std::process::Command::new(RAR_CLI)
        .arg("a")
        .arg(format!("-ilog{}", log.display()))
        .arg("-idq")
        .arg(dir.path().join("bad.rar"))
        .arg("missing.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("missing.txt"),
        "-ilog must record the error"
    );
}
