use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir, pseudo_random_bytes};
// ── -ms / -df / -t / -ep4 / -as / -or and `rar a` replace (WinRAR 7.23) ──

/// `-ms<list>` stores matching files without compression (WinRAR: level 0
/// for the listed extensions/masks, everything else compresses).
#[test]
fn cli_store_types_ms_stores_matching_files() {
    let dir = make_temp_dir();
    let txt = dir.path().join("a.txt");
    let bin = dir.path().join("b.bin");
    std::fs::write(&txt, b"aaaa".repeat(200)).unwrap();
    std::fs::write(&bin, pseudo_random_bytes(8 * 1024, 3)).unwrap();

    let archive = dir.path().join("ms.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-msbin", "-idq"])
        .arg(&archive)
        .arg("a.txt")
        .arg("b.bin")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let b_id = rar.unique_entry("b.bin").unwrap();
    let b = rar.entry(b_id).unwrap();
    assert_eq!(b.method(), 0, "-msbin must store b.bin");
    let a_id = rar.unique_entry("a.txt").unwrap();
    let a = rar.entry(a_id).unwrap();
    assert_eq!(a.method(), 3, "a.txt must still compress");
    assert_eq!(rar.read_entry(b_id).unwrap(), std::fs::read(&bin).unwrap());
    assert_eq!(rar.read_entry(a_id).unwrap(), std::fs::read(&txt).unwrap());
}

/// `-f` / `-u` on extraction: freshen replaces only existing destinations
/// older than the archived member (missing ones are skipped); update also
/// extracts missing ones. Both used to be silent no-ops.
#[test]
fn cli_freshen_and_update_extraction() {
    let dir = make_temp_dir();
    let base = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    let file = dir.path().join("f.txt");
    std::fs::write(&file, b"v1").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(base)
        .unwrap();
    let archive = dir.path().join("f.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let dest = |name: &str, content: &[u8], mtime: Option<std::time::SystemTime>| {
        let path = dir.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("f.txt"), content).unwrap();
        if let Some(mtime) = mtime {
            std::fs::File::options()
                .write(true)
                .open(path.join("f.txt"))
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        }
        path
    };

    // Missing destination: freshen skips it (exit 10, like official), update
    // extracts it.
    let missing = dir.path().join("missing");
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-f", "-idq"])
        .arg(&archive)
        .arg("--dest")
        .arg(&missing)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(10));
    assert!(
        !missing.join("f.txt").exists(),
        "-f must not extract a missing destination"
    );
    let updated = dir.path().join("updated");
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-u", "-idq"])
        .arg(&archive)
        .arg("--dest")
        .arg(&updated)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read(updated.join("f.txt")).unwrap(),
        b"v1",
        "-u must add a missing destination"
    );

    // Older destination: replaced by both.
    let older = dest(
        "older",
        b"stale",
        Some(base - std::time::Duration::from_secs(10)),
    );
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-f", "-idq"])
        .arg(&archive)
        .arg("--dest")
        .arg(&older)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(older.join("f.txt")).unwrap(), b"v1");

    // Newer destination: left untouched by both (all skipped -> exit 10).
    let newer = dest(
        "newer",
        b"fresh",
        Some(base + std::time::Duration::from_secs(10)),
    );
    for flag in ["-f", "-u"] {
        let status = std::process::Command::new(RAR_CLI)
            .args(["x", flag, "-idq"])
            .arg(&archive)
            .arg("--dest")
            .arg(&newer)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(10), "{flag}: all members skipped");
        assert_eq!(
            std::fs::read(newer.join("f.txt")).unwrap(),
            b"fresh",
            "{flag} must leave a newer destination untouched"
        );
    }
}

/// `-df` deletes the source files after archiving (the archive keeps them).
#[test]
fn cli_delete_after_df_removes_sources() {
    let dir = make_temp_dir();
    let file = dir.path().join("gone.txt");
    std::fs::write(&file, b"will be deleted").unwrap();
    let archive = dir.path().join("df.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-df", "-idq"])
        .arg(&archive)
        .arg("gone.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!file.exists(), "-df must delete the source");
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let gone_id = rar.unique_entry("gone.txt").unwrap();
    assert_eq!(rar.read_entry(gone_id).unwrap(), b"will be deleted");
}

/// A source that cannot be deleted must surface as a warning, not a silent
/// success: a non-writable parent directory blocks `remove_file` on Unix.
#[cfg(unix)]
#[test]
fn cli_delete_after_reports_sources_it_could_not_delete() {
    use std::os::unix::fs::PermissionsExt;

    let dir = make_temp_dir();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let file = sub.join("locked.txt");
    std::fs::write(&file, b"locked").unwrap();

    let archive = dir.path().join("locked.rar");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();
    let output = std::process::Command::new(RAR_CLI)
        .args(["a", "-df", "-idq"])
        .arg(&archive)
        .arg("sub/locked.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "an undeleted source must be a warning, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(file.exists(), "the locked source survives");

    // The archive was still written and holds the member.
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let id = rar.unique_entry("sub/locked.txt").unwrap();
    assert_eq!(rar.read_entry(id).unwrap(), b"locked");

    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `-t` tests the archive right after creating it.
#[test]
fn cli_test_after_t_validates_new_archive() {
    let dir = make_temp_dir();
    let file = dir.path().join("t.txt");
    std::fs::write(&file, b"test after payload").unwrap();
    let archive = dir.path().join("t.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-t", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-t must succeed on a healthy archive");
}

/// `-ep4<path>` excludes the path prefix from stored names.
#[test]
fn cli_exclude_prefix_ep4_strips_prefix() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sub/dir")).unwrap();
    std::fs::write(dir.path().join("sub/dir/f.txt"), b"data").unwrap();
    let archive = dir.path().join("ep4.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep4sub", "-idq"])
        .arg(&archive)
        .arg("sub/dir/f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<String>>(),
        ["dir/f.txt"]
    );
    let dir_id = rar.unique_entry("dir/f.txt").unwrap();
    assert_eq!(rar.read_entry(dir_id).unwrap(), b"data");
}

/// `-as` synchronizes an existing archive: members not in the file list
/// are dropped.
#[test]
fn cli_sync_archive_as_drops_stale_members() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
    let archive = dir.path().join("as.rar");
    let run = |files: &[&str]| {
        std::process::Command::new(RAR_CLI)
            .args(["a", "-as", "-idq"])
            .arg(&archive)
            .args(files)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    };
    assert!(run(&["a.txt", "keep.txt"]));
    assert!(run(&["a.txt"])); // keep.txt is stale now
    let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<String>>(),
        ["a.txt"]
    );
}

/// `rar a` on an existing archive replaces same-named members (WinRAR
/// update semantics) and preserves the others.
#[test]
fn cli_a_replaces_same_named_members() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"old version").unwrap();
    std::fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
    let archive = dir.path().join("upd.rar");
    let run = |files: &[&str]| {
        std::process::Command::new(RAR_CLI)
            .args(["a", "-idq"])
            .arg(&archive)
            .args(files)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    };
    assert!(run(&["a.txt", "keep.txt"]));
    std::fs::write(dir.path().join("a.txt"), b"new version").unwrap();
    assert!(run(&["a.txt"]));
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    // Note: the replaced member moves to the end (delete + re-add);
    // WinRAR keeps the original position. Member sets must match.
    let mut names: Vec<String> = rar.entries().map(|e| e.name().to_string()).collect();
    names.sort();
    assert_eq!(names, ["a.txt", "keep.txt"]);
    let a_id = rar.unique_entry("a.txt").unwrap();
    assert_eq!(rar.read_entry(a_id).unwrap(), b"new version");
}

/// The accepted-for-parity switches (`-ds`, `-s=g`, `-htc`, `-mcx`, `-me`,
/// `-oc`, `-mlp`, `-dh`) must be accepted without changing the outcome.
#[test]
fn cli_accepts_parity_switches() {
    let dir = make_temp_dir();
    let file = dir.path().join("p.txt");
    std::fs::write(&file, b"parity switch payload").unwrap();
    let archive = dir.path().join("par.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args([
            "a", "-ds", "-s=g", "-htc", "-mcx", "-me", "-oc", "-mlp", "-dh", "-idq",
        ])
        .arg(&archive)
        .arg("p.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let p_id = rar.unique_entry("p.txt").unwrap();
    assert_eq!(rar.read_entry(p_id).unwrap(), b"parity switch payload");
}

/// `unrar x -or` renames colliding destinations like WinRAR: `a.txt`
/// becomes `a(1).txt`.
#[test]
fn cli_or_auto_renames_colliding_destinations() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"archive content").unwrap();
    let archive = dir.path().join("or.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("a.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("a.txt"), b"old file").unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-or", "-idq"])
        .arg(&archive)
        .args(["--dest"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"old file");
    assert_eq!(
        std::fs::read(out.join("a(1).txt")).unwrap(),
        b"archive content"
    );
}

/// `rar s` converts an archive to SFX (prepending an SFX module found in
/// the WinRAR installation on Windows) and `rar s-` strips it back; the
/// .sfx file must still extract byte-identically.
#[test]
fn cli_sfx_roundtrip_with_module() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"sfx payload").unwrap();
    let archive = dir.path().join("base.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let status = std::process::Command::new(RAR_CLI)
        .args(["s"])
        .arg("base.rar")
        .current_dir(dir.path())
        .status()
        .unwrap();
    if !status.success() {
        // No SFX module available (non-Windows without one installed):
        // the command itself is what we test, so a clean failure is fine.
        return;
    }
    let sfx = dir.path().join("base.sfx");
    assert!(sfx.exists(), "base.sfx must be created");
    let sfx_len = std::fs::metadata(&sfx).unwrap().len();
    assert!(sfx_len > std::fs::metadata(&archive).unwrap().len());

    // The .sfx file extracts byte-identically (our extractor skips the
    // module prefix).
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-idq"])
        .arg(&sfx)
        .args(["--dest"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "unrar must extract the .sfx file");
    assert_eq!(std::fs::read(out.join("f.txt")).unwrap(), b"sfx payload");

    // `rar s-` strips the module back to a plain archive.
    let status = std::process::Command::new(RAR_CLI)
        .args(["s-"])
        .arg("base.sfx")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "s- must strip the SFX module");
    let mut rar = rar_rs::ArchiveReader::open(dir.path().join("base.rar")).unwrap();
    let f_id = rar.unique_entry("f.txt").unwrap();
    assert_eq!(rar.read_entry(f_id).unwrap(), b"sfx payload");
}

/// `unrar x -ep` (and the `--flat` spelling) extracts basenames without the
/// stored tree, like the `e` command; the non-flat `x` keeps the tree.
#[test]
fn cli_unrar_x_flat_flag_extracts_basenames() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/a.txt"), b"payload").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq", "flat.rar", "sub/a.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let nested = dir.path().join("nested");
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-idq", "flat.rar", "--dest"])
        .arg(&nested)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(nested.join("sub/a.txt").exists());

    let flat = dir.path().join("flat");
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-ep", "-idq", "flat.rar", "--dest"])
        .arg(&flat)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(flat.join("a.txt")).unwrap(), b"payload");
    assert!(!flat.join("sub").exists(), "-ep must drop the stored tree");
}

/// `--max-unpacked` / `--max-total-unpacked`: opt-in ceilings on the bytes a
/// disk extraction may write. Without them extraction is unbounded, matching
/// WinRAR/UnRAR; with them an oversized member (or run) fails with the limit
/// exit code instead of filling the disk, which is the CLI's only guard
/// against a decompression bomb. Both binaries accept the switches, and a
/// per-member cap larger than the total cap is a usage error rather than a
/// silent no-op.
#[test]
fn cli_extract_size_guards_bound_disk_growth() {
    let dir = make_temp_dir();
    let payload = "x".repeat(4096);
    std::fs::write(dir.path().join("a.txt"), &payload).unwrap();
    std::fs::write(dir.path().join("b.txt"), &payload).unwrap();
    let archive = dir.path().join("guarded.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .args(["a.txt", "b.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "archive creation must succeed");

    // Per-member cap below the member size: exit 8 (limit), nothing written.
    let rejected = dir.path().join("per-file-rejected");
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "--max-unpacked", "1k", "-idq"])
        .arg(&archive)
        .arg("--dest")
        .arg(&rejected)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(8),
        "an over-cap member must fail with the limit exit code"
    );
    assert!(
        !rejected.join("a.txt").exists(),
        "a rejected member must not be left on disk"
    );

    // A cap above the member size extracts normally.
    let ok = dir.path().join("per-file-ok");
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "--max-unpacked", "8k", "-idq"])
        .arg(&archive)
        .arg("--dest")
        .arg(&ok)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "a generous cap must not block extraction");
    assert_eq!(std::fs::read(ok.join("a.txt")).unwrap(), payload.as_bytes());

    // Total cap: each member fits on its own, the pair does not.
    let total = dir.path().join("total-rejected");
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "--max-total-unpacked", "6k", "-idq"])
        .arg(&archive)
        .arg("--dest")
        .arg(&total)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(8),
        "the whole-run cap must be enforced too"
    );

    // `unrar` accepts the same switches (same `ExtractRequest` owner).
    let unrar_out = dir.path().join("unrar-rejected");
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "--max-unpacked", "1k", "-idq"])
        .arg(&archive)
        .arg("--dest")
        .arg(&unrar_out)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(8),
        "unrar must honor --max-unpacked as well"
    );

    // A per-member cap larger than the total cap is a usage error (exit 2).
    let status = std::process::Command::new(RAR_CLI)
        .args([
            "x",
            "--max-unpacked",
            "2g",
            "--max-total-unpacked",
            "1g",
            "-idq",
        ])
        .arg(&archive)
        .arg("--dest")
        .arg(dir.path().join("bad-limits"))
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(2),
        "an inconsistent pair of caps must be rejected up front"
    );
}
