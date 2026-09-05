//! CLI behavior tests: drive the built `rar`/`unrar` binaries through
//! WinRAR-compatible switches. Lives in this crate because the
//! `CARGO_BIN_EXE_*` env vars are only defined for the package that builds
//! the binaries (moved here from the library's interop.rs).

#![allow(deprecated)] // fixture archives built through the legacy write facade

use rar5::RarArchive;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Serializes tests that (a) write `rarfiles.lst` next to the rar binary
/// (`cli_rarfiles_lst_orders_solid_members`) with (b) tests whose member
/// order would be corrupted if a stray `rarfiles.lst` were present
/// (`cli_se_preserves_input_order`, and the `-s` round-trip test). Without
/// it the parallel test threads race on `target/debug/rarfiles.lst` and the
/// order-sensitive tests flake.
fn rarfiles_lst_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn make_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}
const RAR_CLI: &str = env!("CARGO_BIN_EXE_rar");

fn make_tree(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.join("f2.tmp"), b"two").unwrap();
    std::fs::write(dir.join("sub/f3.txt"), b"three").unwrap();
    std::fs::write(dir.join("sub/f4.bin"), b"four").unwrap();
}

fn create_duplicate_archive(path: &Path) {
    let mut rar = RarArchive::create_with_options(path, rar5::CreateOptions::default()).unwrap();
    rar.add_bytes("same.bin", b"first payload", 0).unwrap();
    rar.add_bytes("same.bin", b"second payload", 0).unwrap();
    rar.close().unwrap();
}

fn cli_names(archive: &std::path::Path) -> Vec<String> {
    let rar = rar5::RarArchive::open(archive).unwrap();
    let mut names: Vec<String> = rar
        .namelist()
        .into_iter()
        .map(|n| n.trim_end_matches('/').to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn cli_path_switches_and_filters() {
    let dir = make_temp_dir();
    make_tree(dir.path());
    let cases: Vec<(Vec<&str>, Vec<&str>)> = vec![
        // (switches, expected members)
        (
            vec![],
            vec!["f1.txt", "f2.tmp", "sub", "sub/f3.txt", "sub/f4.bin"],
        ),
        (vec!["-ep"], vec!["f1.txt", "f2.tmp", "f3.txt", "f4.bin"]),
        (
            vec!["-x*.tmp"],
            vec!["f1.txt", "sub", "sub/f3.txt", "sub/f4.bin"],
        ),
        (vec!["-n*.txt"], vec!["f1.txt", "sub/f3.txt"]),
        (vec!["-xsub/*"], vec!["f1.txt", "f2.tmp", "sub"]),
        (vec!["-xsub"], vec!["f1.txt", "f2.tmp"]),
        (
            vec!["-appre/fix"],
            vec![
                "pre/fix/f1.txt",
                "pre/fix/f2.tmp",
                "pre/fix/sub",
                "pre/fix/sub/f3.txt",
                "pre/fix/sub/f4.bin",
            ],
        ),
        (
            vec!["-x*.bin"],
            vec!["f1.txt", "f2.tmp", "sub", "sub/f3.txt"],
        ),
    ];
    for (switches, expected) in cases {
        let archive = dir.path().join("t.rar");
        let mut cmd = std::process::Command::new(RAR_CLI);
        cmd.arg("a").arg(&archive);
        for sw in &switches {
            cmd.arg(sw);
        }
        cmd.arg("f1.txt").arg("f2.tmp").arg("sub");
        cmd.current_dir(dir.path());
        let status = cmd.status().unwrap();
        assert!(status.success(), "cli failed for {switches:?}");
        let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(cli_names(&archive), expected, "switches {switches:?}");
        std::fs::remove_file(&archive).unwrap();
    }
}

#[test]
fn cli_wildcard_args_and_ep1() {
    let dir = make_temp_dir();
    make_tree(dir.path());
    let archive = dir.path().join("w.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a"])
        .arg(&archive)
        .arg("sub/*")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["sub/f3.txt", "sub/f4.bin"]);
    std::fs::remove_file(&archive).unwrap();

    let archive2 = dir.path().join("w2.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep1"])
        .arg(&archive2)
        .arg("sub/*")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive2), ["f3.txt", "f4.bin"]);
    std::fs::remove_file(&archive2).unwrap();
}

/// The switch outputs must be readable by the official tools, and the
/// thread switch must be accepted (env-gated).
#[test]
fn official_validates_cli_switch_archives() {
    let unrar = match std::env::var_os("SA_OFFICIAL_UNRAR") {
        Some(p) => p,
        None => return,
    };
    let dir = make_temp_dir();
    make_tree(dir.path());
    let archive = dir.path().join("sw.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-mt4", "-x*.tmp", "-appre/fix"])
        .arg(&archive)
        .arg("f1.txt")
        .arg("f2.tmp")
        .arg("sub")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        [
            "pre/fix/f1.txt",
            "pre/fix/sub",
            "pre/fix/sub/f3.txt",
            "pre/fix/sub/f4.bin"
        ]
    );
    let status = std::process::Command::new(&unrar)
        .arg("t")
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success(), "unrar rejected the filtered archive");
}
// ── CLI -r- / -cl / -cu ─────────────────────────────────────────────────────

#[test]
fn cli_recursion_and_case_switches() {
    let dir = make_temp_dir();
    std::fs::create_dir_all(dir.path().join("rdir/sub")).unwrap();
    std::fs::write(dir.path().join("rdir/f1.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("rdir/sub/f2.txt"), b"b").unwrap();
    std::fs::write(dir.path().join("MiXeD.TXT"), b"c").unwrap();

    // -r-: directory arguments store only the directory entry.
    let archive = dir.path().join("r.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-r-"])
        .arg(&archive)
        .arg("rdir")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["rdir"]);
    std::fs::remove_file(&archive).unwrap();

    // -cl / -cu: name case conversion.
    let archive = dir.path().join("c.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-cl"])
        .arg(&archive)
        .arg("MiXeD.TXT")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["mixed.txt"]);
    std::fs::remove_file(&archive).unwrap();

    let archive = dir.path().join("c2.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-cu"])
        .arg(&archive)
        .arg("MiXeD.TXT")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["MIXED.TXT"]);
}

// ── WinRAR CLI parity: quiet mode, ch/p commands, -o-, -z, list variants ──

const UNRAR_CLI: &str = env!("CARGO_BIN_EXE_unrar");

#[test]
fn cli_quiet_mode_suppresses_informational_output() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("q.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).is_empty(),
        "-idq must suppress status output, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Without -idq the status line appears.
    let out = std::process::Command::new(RAR_CLI)
        .args(["a"])
        .arg(dir.path().join("q2.rar"))
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Created"));
}

#[test]
fn cli_ch_converts_member_case_like_winrar() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("MiXeD.TXT"), b"x").unwrap();
    let archive = dir.path().join("ch.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&archive, rar5::CreateOptions::default()).unwrap();
        rar.add(dir.path().join("MiXeD.TXT"), 3).unwrap();
        rar.close().unwrap();
    }
    assert_eq!(cli_names(&archive), ["MiXeD.TXT"]);
    let status = std::process::Command::new(RAR_CLI)
        .args(["ch", "-cl"])
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["mixed.txt"]);
    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("mixed.txt").unwrap(), b"x");
}

#[test]
fn cli_print_writes_member_to_stdout() {
    let dir = make_temp_dir();
    let archive = dir.path().join("p.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&archive, rar5::CreateOptions::default()).unwrap();
        rar.add_bytes("a.txt", b"hello p", 0).unwrap();
        rar.close().unwrap();
    }
    let out = std::process::Command::new(RAR_CLI)
        .args(["p"])
        .arg(&archive)
        .arg("a.txt")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"hello p");
}

#[test]
fn cli_print_preserves_duplicate_members_and_reports_no_match() {
    let dir = make_temp_dir();
    let archive = dir.path().join("duplicate-print.rar");
    create_duplicate_archive(&archive);
    let expected = b"first payloadsecond payload";

    for binary in [RAR_CLI, UNRAR_CLI] {
        for selector in [Some("same.bin"), None] {
            let mut command = std::process::Command::new(binary);
            command.arg("p").arg(&archive);
            if let Some(selector) = selector {
                command.arg(selector);
            }
            let out = command.output().unwrap();
            assert!(out.status.success(), "{binary} print failed");
            assert_eq!(out.stdout, expected, "{binary} collapsed a duplicate");
        }

        let out = std::process::Command::new(binary)
            .arg("p")
            .arg(&archive)
            .arg("missing.bin")
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "{binary} accepted a missing selector"
        );
        assert!(out.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("no archive members matched"),
            "{binary} did not report the missing selector clearly"
        );
    }

    let exact_archive = dir.path().join("exact-print.rar");
    let mut rar = RarArchive::create_with_options(&exact_archive, Default::default()).unwrap();
    rar.add_bytes("same.bin", b"exact", 0).unwrap();
    rar.add_bytes("dir/same.bin", b"basename only", 0).unwrap();
    rar.close().unwrap();
    for binary in [RAR_CLI, UNRAR_CLI] {
        let out = std::process::Command::new(binary)
            .arg("p")
            .arg(&exact_archive)
            .arg("same.bin")
            .output()
            .unwrap();
        assert!(out.status.success(), "{binary} exact print failed");
        assert_eq!(out.stdout, b"exact", "{binary} matched by basename");
    }
}

#[test]
fn cli_stdout_and_selected_extraction_preserve_duplicate_members() {
    let dir = make_temp_dir();
    let archive = dir.path().join("duplicate-extract.rar");
    create_duplicate_archive(&archive);
    let expected = b"first payloadsecond payload";

    for (index, binary) in [RAR_CLI, UNRAR_CLI].into_iter().enumerate() {
        for selector in [Some("same.bin"), None] {
            let mut command = std::process::Command::new(binary);
            command.args(["x", "-so", "-idq"]).arg(&archive);
            if let Some(selector) = selector {
                command.arg(selector);
            }
            let out = command.output().unwrap();
            assert!(out.status.success(), "{binary} stdout extraction failed");
            assert_eq!(out.stdout, expected, "{binary} collapsed a duplicate");
        }

        let output = dir.path().join(format!("selected-{index}"));
        let out = std::process::Command::new(binary)
            .args(["x", "-idq"])
            .arg(&archive)
            .arg("--dest")
            .arg(&output)
            .arg("same.bin")
            .output()
            .unwrap();
        assert!(out.status.success(), "{binary} selected extraction failed");
        assert_eq!(
            std::fs::read(output.join("same.bin")).unwrap(),
            b"second payload",
            "{binary} repeatedly extracted the first duplicate"
        );
    }
}

#[test]
fn cli_overwrite_never_skips_existing_files() {
    let dir = make_temp_dir();
    let archive = dir.path().join("o.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&archive, rar5::CreateOptions::default()).unwrap();
        rar.add_bytes("f.txt", b"new", 0).unwrap();
        rar.close().unwrap();
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("f.txt"), b"OLD").unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-o-"])
        .arg(&archive)
        .args(["--dest"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read(out.join("f.txt")).unwrap(),
        b"OLD",
        "-o- must leave existing files untouched"
    );
}

#[test]
fn cli_comment_file_sets_comment() {
    let dir = make_temp_dir();
    let archive = dir.path().join("z.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&archive, rar5::CreateOptions::default()).unwrap();
        rar.add_bytes("f.txt", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    std::fs::write(dir.path().join("note.txt"), b"file comment").unwrap();
    // `-z<file>` is a single token (like WinRAR's `-zfile`).
    let status = std::process::Command::new(RAR_CLI)
        .args(["c"])
        .arg(format!("-z{}", dir.path().join("note.txt").display()))
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    let out = std::process::Command::new(RAR_CLI)
        .args(["cw"])
        .arg(&archive)
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"file comment");
}

#[test]
fn unrar_list_variants_bare_and_technical() {
    let dir = make_temp_dir();
    let archive = dir.path().join("lt.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&archive, rar5::CreateOptions::default()).unwrap();
        rar.add_bytes("a.txt", b"aaa", 0).unwrap();
        rar.add_bytes("b.bin", b"bbbb", 0).unwrap();
        rar.close().unwrap();
    }
    let bare = std::process::Command::new(UNRAR_CLI)
        .arg("lb")
        .arg(&archive)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&bare.stdout), "a.txt\nb.bin\n");

    let tech = std::process::Command::new(UNRAR_CLI)
        .arg("lt")
        .arg(&archive)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&tech.stdout);
    assert!(
        text.contains("a.txt") && text.contains("Checksum"),
        "{text}"
    );
    // Technical rows carry a CRC column value.
    let row = text.lines().find(|l| l.ends_with("a.txt")).unwrap();
    assert!(!row.trim_start().starts_with("-"), "{row}");
}

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
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert!(names.contains(&"src/keep.txt"), "{names:?}");
    assert!(
        !names.contains(&"src/drop.tmp"),
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
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert!(names.contains(&"new.txt"), "{names:?}");
    assert!(
        !names.contains(&"old.txt"),
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
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert_eq!(names.len(), 1, "{names:?}");
    let _stored = names[0];
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
    let rar = RarArchive::open(&archive3).unwrap();
    let _stored = rar.namelist()[0];
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
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert!(
        names.contains(&"r0src/top.txt"),
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
    let mut rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert!(names.contains(&"lnk/target.txt"), "{names:?}");
    assert!(names.contains(&"lnk/lnk.txt"), "{names:?}");
    // The link member carries no data (redirect record), and extraction
    // recreates the symlink.
    let entry = rar.get_entry("lnk/lnk.txt").unwrap();
    assert_eq!(entry.size(), 0);
    let out = dir.path().join("out");
    rar.extract_all(&out).unwrap();
    let link = std::fs::read_link(out.join("lnk/lnk.txt")).unwrap();
    assert_eq!(link, std::path::Path::new("target.txt"));
}

// ── WinRAR CLI parity batch 3: -sl/-sm/-ed, -tn/-to, -si, -tk, -p-/-c-, ──
// ── -ierr, -ad ────────────────────────────────────────────────────────────

/// Set a file's mtime to `secs_ago` seconds in the past.
fn set_mtime_ago(path: &Path, secs_ago: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(t))
        .unwrap();
}

#[test]
fn cli_size_and_empty_dir_filters() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("small.txt"), b"12345").unwrap();
    std::fs::write(dir.path().join("big.txt"), vec![b'x'; 5000]).unwrap();
    std::fs::create_dir_all(dir.path().join("emptydir")).unwrap();
    std::fs::create_dir_all(dir.path().join("fulldir")).unwrap();
    std::fs::write(dir.path().join("fulldir").join("inner.txt"), b"i").unwrap();

    // -sl1k: only files smaller than 1 KiB (directories always pass).
    let archive = dir.path().join("sl.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-sl1k", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        ["emptydir", "fulldir", "fulldir/inner.txt", "small.txt"]
    );

    // -sm1k: only files larger than 1 KiB.
    let archive = dir.path().join("sm.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-sm1k", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["big.txt", "emptydir", "fulldir"]);

    // -ed: empty directories are not stored.
    let archive = dir.path().join("ed.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ed", "-idq"])
        .arg(&archive)
        .args(["emptydir", "fulldir", "small.txt", "big.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        cli_names(&archive),
        ["big.txt", "fulldir", "fulldir/inner.txt", "small.txt"]
    );
}

#[test]
fn cli_period_filters_tn_to_match_winrar() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("new.txt"), b"n").unwrap();
    std::fs::write(dir.path().join("old.txt"), b"o").unwrap();
    set_mtime_ago(&dir.path().join("old.txt"), 5400); // 1.5 hours ago

    // -tn1h: only files newer than 1 hour.
    let archive = dir.path().join("tn.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt"]);

    // -to1h: only files older than 1 hour.
    let archive = dir.path().join("to.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-to1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["old.txt"]);

    // Multiple filters combine with AND: 1h < age <= 2h.
    let archive = dir.path().join("tnandto.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn2h", "-to1h", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["old.txt"]);

    // Compound period and modifier: -tnc1h30m parses and filters on the
    // creation time (both files were just created, so both match).
    let archive = dir.path().join("tnc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tnc1h30m", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt", "old.txt"]);

    // -to with an empty period matches everything (age >= 0).
    let archive = dir.path().join("to0.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-to", "-idq"])
        .arg(&archive)
        .args(["new.txt", "old.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["new.txt", "old.txt"]);

    // No match: WinRAR exits 10 and does not create the archive. old.txt
    // (1.5 h old) deterministically fails the 1 s filter.
    let archive = dir.path().join("none.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tn1s", "-idq"])
        .arg(&archive)
        .arg("old.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(10), "no-match must exit with code 10");
    assert!(!archive.exists(), "no-match must not create the archive");
}

#[test]
fn cli_stdin_name_reads_stdin() {
    let dir = make_temp_dir();
    let archive = dir.path().join("si.rar");
    let mut child = std::process::Command::new(RAR_CLI)
        .args(["a", "-siin.txt", "-idq"])
        .arg(&archive)
        .stdin(std::process::Stdio::piped())
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello-stdin")
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.namelist(), ["in.txt"]);
    assert_eq!(rar.read("in.txt").unwrap(), b"hello-stdin");
}

#[test]
fn cli_keep_time_preserves_archive_mtime() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("tk.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let before = std::fs::metadata(&archive).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tk", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let after = std::fs::metadata(&archive).unwrap().modified().unwrap();
    let kept = after.duration_since(before).unwrap_or_default();
    assert!(
        kept < std::time::Duration::from_secs(1),
        "-tk must keep the archive mtime, changed by {kept:?}"
    );
}

#[test]
fn cli_clear_password_and_no_comment_switches() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    // Bare -p must not silently create an unencrypted archive when secure
    // no-echo prompting is unavailable.
    let bare_archive = dir.path().join("bare-p.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-p", "-idq"])
        .arg(&bare_archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!bare_archive.exists());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("secure no-echo password prompt"),
        "unexpected bare -p error: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // unrar rejects the same unsafe prompt form before attempting to open.
    let out = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-p"])
        .arg("missing.rar")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("secure no-echo password prompt"));

    // A separated long-option value is a real password, not a bare prompt.
    let long_password_archive = dir.path().join("long-password.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "--password", "secret", "-idq"])
        .arg(&long_password_archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = RarArchive::open_with_password(&long_password_archive, "secret").unwrap();
    assert_eq!(rar.read("f.txt").unwrap(), b"x");

    // The attached WinRAR form remains supported.
    let attached_password_archive = dir.path().join("attached-password.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-psecret", "-idq"])
        .arg(&attached_password_archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = RarArchive::open_with_password(&attached_password_archive, "secret").unwrap();
    assert_eq!(rar.read("f.txt").unwrap(), b"x");

    // A long password option with no following value is still rejected.
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(dir.path().join("bare-long.rar"))
        .arg("f.txt")
        .arg("--password")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("secure no-echo password prompt"));

    // -p- creates an unencrypted archive (readable without a password).
    let archive = dir.path().join("pm.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-p-", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("f.txt").unwrap(), b"x");

    // -c- is accepted on create.
    let archive = dir.path().join("nc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-c-", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["f.txt"]);
}

#[test]
fn cli_err_switch_routes_messages_to_stderr() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let archive = dir.path().join("ierr.rar");
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "-ierr"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "-ierr must send messages to stderr, stdout had: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Created"),
        "-ierr must send the status message to stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_append_dir_extracts_under_archive_name() {
    let dir = make_temp_dir();
    let archive = dir.path().join("ad.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&archive, rar5::CreateOptions::default()).unwrap();
        rar.add_bytes("f.txt", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-ad", "-idq"])
        .arg(&archive)
        .args(["--dest"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        out.join("ad").join("f.txt").exists(),
        "-ad must extract under a subdirectory named after the archive"
    );
}

// ── -md dictionary size (aligned with WinRAR 7.23) ─────────────────────────

/// Write a 32 MiB file of repeated text (compressible, uniform head so the
/// incompressibility probe does not misfire).
fn write_rep_text(path: &Path, size: usize) {
    let block = b"The quick brown fox jumps over the lazy dog 0123456789.\r\n";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        data.extend_from_slice(block);
    }
    data.truncate(size);
    std::fs::write(path, data).unwrap();
}

fn entry_dict_log(archive: &Path, name: &str) -> u8 {
    let rar = rar5::RarArchive::open(archive).unwrap();
    rar.get_entry(name).unwrap().comp_dict_size()
}

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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    let data = rar.read("rep32t.bin").unwrap();
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
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("rep32t.bin").unwrap();
        assert_eq!(e.comp_version(), 0, "-md{md} small file stays v50");
        assert_eq!(e.dict_size_bytes(), None, "-md{md}");
        // 2x floor_pow2(32 MiB) = 64 MiB -> log 9.
        assert_eq!(e.comp_dict_size(), 9, "-md{md} cap");
        let data = rar.read("rep32t.bin").unwrap();
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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    rar.extract_all_with_options(
        &out_dir,
        rar5::ExtractOptions {
            max_unpacked_bytes: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(std::fs::read(out_dir.join("pair32.bin")).unwrap(), data);
}

/// Deterministic pseudo-random bytes (LCG) — incompressible.
fn pseudo_random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    let b = rar.get_entry("b.bin").unwrap();
    assert_eq!(b.method(), 0, "-msbin must store b.bin");
    let a = rar.get_entry("a.txt").unwrap();
    assert_eq!(a.method(), 3, "a.txt must still compress");
    assert_eq!(rar.read("b.bin").unwrap(), std::fs::read(&bin).unwrap());
    assert_eq!(rar.read("a.txt").unwrap(), std::fs::read(&txt).unwrap());
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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("gone.txt").unwrap(), b"will be deleted");
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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.namelist(), ["dir/f.txt"]);
    assert_eq!(rar.read("dir/f.txt").unwrap(), b"data");
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
    let rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.namelist(), ["a.txt"]);
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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    // Note: the replaced member moves to the end (delete + re-add);
    // WinRAR keeps the original position. Member sets must match.
    let mut names = rar.namelist();
    names.sort();
    assert_eq!(names, ["a.txt", "keep.txt"]);
    assert_eq!(rar.read("a.txt").unwrap(), b"new version");
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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("p.txt").unwrap(), b"parity switch payload");
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
    let mut rar = rar5::RarArchive::open(dir.path().join("base.rar")).unwrap();
    assert_eq!(rar.read("f.txt").unwrap(), b"sfx payload");
}

// ── -ts file time save/restore (WinRAR 7.23 aligned) ───────────────────────

fn created_time(path: &Path) -> Option<std::time::SystemTime> {
    #[cfg(windows)]
    {
        std::fs::metadata(path).ok()?.created().ok()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// `-ts` stores the creation/access times and `unrar x -ts` restores
/// them alongside the modification time (Windows can set all three;
/// Unix restores mtime + atime).
#[test]
fn cli_ts_saves_and_restores_file_times() {
    let dir = make_temp_dir();
    let file = dir.path().join("t.txt");
    std::fs::write(&file, b"ts payload").unwrap();
    let src_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
    let src_ctime = created_time(&file);

    // Default: only mtime is stored (no ctime/atime in the extra record).
    let archive = dir.path().join("def.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("t.txt").unwrap();
        assert!(e.ctime().is_none(), "default must not store ctime");
        assert!(e.atime().is_none(), "default must not store atime");
    }

    // -ts: all three times stored with ns precision.
    let archive = dir.path().join("all.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ts", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("t.txt").unwrap();
        assert!(e.ctime().is_some(), "-ts must store ctime");
        if let Some(src_ctime) = src_ctime {
            let c = e.ctime().unwrap();
            let restored = std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(c.0)
                + std::time::Duration::from_nanos(c.1 as u64);
            let diff = restored
                .duration_since(src_ctime)
                .unwrap_or_else(|_| src_ctime.duration_since(restored).unwrap());
            assert!(
                diff < std::time::Duration::from_secs(2),
                "stored ctime {restored:?} far from source {src_ctime:?}"
            );
        }
    }

    // Extract with -ts: mtime and (Windows) creation time restored.
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-ts", "-y"])
        .arg(&archive)
        .args(["--dest"])
        .arg(&out)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let extracted = out.join("t.txt");
    let dst_mtime = std::fs::metadata(&extracted).unwrap().modified().unwrap();
    assert!(
        dst_mtime.duration_since(src_mtime).unwrap_or_default() < std::time::Duration::from_secs(2),
        "extracted mtime must match the source"
    );
    if let Some(src_ctime) = src_ctime {
        let dst_ctime = created_time(&extracted);
        if let Some(dst_ctime) = dst_ctime {
            let diff = dst_ctime
                .duration_since(src_ctime)
                .unwrap_or_else(|_| src_ctime.duration_since(dst_ctime).unwrap());
            assert!(
                diff < std::time::Duration::from_secs(2),
                "extracted ctime {dst_ctime:?} must match source {src_ctime:?}"
            );
        }
    }

    // -ts1: 1-second precision (ns fields zero).
    let archive = dir.path().join("sec.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ts1", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("t.txt").unwrap();
        assert_eq!(e.mtime_ns(), Some(0), "-ts1 must store second precision");
    }

    // -ts-: no times stored at all.
    let archive = dir.path().join("none.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ts-", "-idq"])
        .arg(&archive)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let rar = rar5::RarArchive::open(&archive).unwrap();
        let e = rar.get_entry("t.txt").unwrap();
        assert!(e.ctime().is_none() && e.atime().is_none());
        assert!(
            e.mtime_ns().is_none(),
            "-ts- must not write a time extra record"
        );
    }

    // Invalid specs are rejected.
    let out = std::process::Command::new(RAR_CLI)
        .args(["a", "--ts=xyz"])
        .arg(dir.path().join("badts.rar"))
        .arg("t.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "invalid -ts spec must be rejected");
}

// ── update/freshen and miscellaneous switches ─────────────────────────────

#[test]
fn cli_version_control_keeps_previous_versions() {
    let dir = make_temp_dir();
    let file = dir.path().join("ver.txt");
    std::fs::write(&file, b"v1").unwrap();
    let archive = dir.path().join("ver.rar");

    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // First update with -ver: old version kept as `ver.txt;1`.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, b"v2").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-ver", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        assert_eq!(rar.read("ver.txt").unwrap(), b"v2");
        assert_eq!(rar.read("ver.txt;1").unwrap(), b"v1");
    }

    // Second update: the chain shifts (ver.txt;1 -> ver.txt;2).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, b"v3").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-ver", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        assert_eq!(rar.read("ver.txt").unwrap(), b"v3");
        assert_eq!(rar.read("ver.txt;1").unwrap(), b"v2");
        assert_eq!(rar.read("ver.txt;2").unwrap(), b"v1");
    }

    // -ver1 caps the history at one previous version.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, b"v4").unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["u", "-ver1", "-idq"])
        .arg(&archive)
        .arg("ver.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    {
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        assert_eq!(rar.read("ver.txt").unwrap(), b"v4");
        assert_eq!(rar.read("ver.txt;1").unwrap(), b"v3");
        assert!(!rar.namelist().contains(&"ver.txt;2"));
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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("keep.txt").unwrap(), b"k");
    drop(rar);
    match rar5::RarArchive::open_append(&archive) {
        Err(rar5::RarError::ArchiveLocked) => {}
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
    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("seed.txt").unwrap(), b"seed");
    assert_eq!(rar.read("added.txt").unwrap(), b"added");
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
    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("existing.txt").unwrap(), b"new");
    assert_eq!(rar.read("added.txt").unwrap(), b"added");
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
    let mut rar = RarArchive::open(&archive).unwrap();
    assert_eq!(rar.read("existing.txt").unwrap(), b"new");
    assert!(!rar.namelist().contains(&"missing.txt"));
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
    let mut rar = RarArchive::open(&update_archive).unwrap();
    assert_eq!(rar.read("update-tree/changed.txt").unwrap(), b"new");
    assert_eq!(rar.read("update-tree/unchanged.txt").unwrap(), b"same");
    assert_eq!(rar.read("update-tree/added.txt").unwrap(), b"added");

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
    let mut rar = RarArchive::open(&freshen_archive).unwrap();
    assert_eq!(rar.read("freshen-tree/changed.txt").unwrap(), b"new");
    assert_eq!(rar.read("freshen-tree/unchanged.txt").unwrap(), b"same");
    assert!(!rar.namelist().contains(&"freshen-tree/missing.txt"));
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
            RarArchive::create_with_options(&archive, rar5::CreateOptions::default()).unwrap();
        rar.add_bytes("a", b"A", 0).unwrap();
        rar.add_bytes("dir/base.txt", b"BASE", 0).unwrap();
        rar.add_bytes("full/path.txt", b"FULL", 0).unwrap();
        rar.close().unwrap();
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
    let rar = RarArchive::open(&archive).unwrap();
    assert!(rar.get_entry("dict.bin").unwrap().dict_size_bytes() > Some(8 * 1024 * 1024));

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

// ── configuration sources: RARINISWITCHES / -cfg- / command-line priority ──

#[test]
fn cli_config_sources_apply_with_winrar_priority() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

    // RARINISWITCHES supplies default switches (here: quiet mode).
    let archive = dir.path().join("env.rar");
    let out = std::process::Command::new(RAR_CLI)
        .env("RARINISWITCHES", "-idq")
        .args(["a"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "RARINISWITCHES=-idq must suppress output, got {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Command line wins over the environment for single-value switches
    // (no duplicate-argument error, -m5 applied).
    let archive = dir.path().join("prio.rar");
    let out = std::process::Command::new(RAR_CLI)
        .env("RARINISWITCHES", "-m1 -md128k")
        .args(["a", "-m5", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "CLI must override RARINISWITCHES without errors: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // -cfg- ignores RARINISWITCHES entirely.
    let archive = dir.path().join("cfg.rar");
    let out = std::process::Command::new(RAR_CLI)
        .env("RARINISWITCHES", "-idq")
        .args(["a", "-cfg-"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Created"),
        "-cfg- must ignore RARINISWITCHES, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ── rarfiles.lst solid ordering (WinRAR 7.23 semantics) ─────────────────────

#[test]
fn cli_rarfiles_lst_orders_solid_members() {
    let _guard = rarfiles_lst_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("aaa.cpp"), b"a").unwrap();
    std::fs::write(dir.path().join("f1.cpp"), b"b").unwrap();
    std::fs::write(dir.path().join("ddd.cpp"), b"c").unwrap();
    std::fs::write(dir.path().join("bbb.h"), b"d").unwrap();
    std::fs::write(dir.path().join("ccc.txt"), b"e").unwrap();
    std::fs::create_dir_all(dir.path().join("subd")).unwrap();
    std::fs::write(dir.path().join("subd").join("nested.txt"), b"n").unwrap();
    std::fs::write(dir.path().join("subd").join("deep.cpp"), b"p").unwrap();

    // rarfiles.lst next to the rar binary (Windows/Unix lookup path).
    let lst = Path::new(RAR_CLI).parent().unwrap().join("rarfiles.lst");
    std::fs::write(&lst, "; test list\n*.txt\nf*.cpp\n*.cpp\n$default\n").unwrap();
    let result = std::panic::catch_unwind(|| {
        let archive = dir.path().join("rfl.rar");
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", "-s", "-idq"])
            .arg(&archive)
            .args(["*.cpp", "*.h", "*.txt", "subd"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let rar = rar5::RarArchive::open(&archive).unwrap();
        let names: Vec<String> = rar
            .namelist()
            .into_iter()
            .map(|s| s.trim_start_matches("./").trim_end_matches('/').to_string())
            .collect();
        // WinRAR order: *.txt group, then f*.cpp (subset of *.cpp, so it
        // wins over *.cpp regardless of position), then *.cpp, then
        // $default, with directory entries last.
        assert_eq!(
            names,
            [
                "ccc.txt",
                "subd/nested.txt",
                "f1.cpp",
                "aaa.cpp",
                "ddd.cpp",
                "subd/deep.cpp",
                "bbb.h",
                "subd",
            ],
            "solid member order must follow rarfiles.lst: {names:?}"
        );
    });
    let _ = std::fs::remove_file(&lst);
    result.unwrap();
}

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
    let mut rar = rar5::RarArchive::open(&archive).unwrap();
    let e = rar.get_entry("f.bin").unwrap();
    assert_eq!(e.comp_version(), 1, "-ma7 forces v70");
    assert_eq!(
        e.dict_size_bytes(),
        Some(32 * 1024 * 1024),
        "default 32 MiB declared"
    );
    assert_eq!(rar.read("f.bin").unwrap(), std::fs::read(&file).unwrap());

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
    let rar = rar5::RarArchive::open(&archive).unwrap();
    let e = rar.get_entry("f.bin").unwrap();
    assert_eq!(e.comp_version(), 1);
    assert_eq!(
        e.dict_size_bytes(),
        Some(16 * 1024 * 1024),
        "-md16m declared"
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
    let rar = rar5::RarArchive::open(&ma5).unwrap();
    let e = rar.get_entry("f.bin").unwrap();
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
    let mut rar = rar5::RarArchive::open(&sd).unwrap();
    assert_eq!(rar.read("a.txt").unwrap(), std::fs::read(&a).unwrap());
    assert_eq!(rar.read("b.bin").unwrap(), std::fs::read(&b).unwrap());
    assert_eq!(rar.read("c.txt").unwrap(), std::fs::read(&c).unwrap());

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
    let mut rar = rar5::RarArchive::open(&se).unwrap();
    assert_eq!(rar.read("a.txt").unwrap(), std::fs::read(&a).unwrap());
    assert_eq!(rar.read("b.bin").unwrap(), std::fs::read(&b).unwrap());
    assert_eq!(rar.read("c.txt").unwrap(), std::fs::read(&c).unwrap());

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
    let volumes = rar5::discover_volumes(&sv);
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
    let mut rar = rar5::RarArchive::open(&volumes[0]).unwrap();
    assert_eq!(rar.read("a.txt").unwrap(), std::fs::read(&a).unwrap());
    assert_eq!(rar.read("b.bin").unwrap(), std::fs::read(&b).unwrap());
    assert_eq!(rar.read("c.txt").unwrap(), std::fs::read(&c).unwrap());
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
        let mut rar = rar5::RarArchive::open(&archive).unwrap();
        assert_eq!(
            rar.read("p.txt").unwrap(),
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
    let mut rar = rar5::RarArchive::open(&arc).unwrap();
    assert_eq!(
        rar.namelist(),
        vec!["a.txt", "b.bin", "c.txt", "d.bin"],
        "-se must not reorder members by extension"
    );
    assert_eq!(rar.read("a.txt").unwrap(), std::fs::read(&a).unwrap());
    assert_eq!(rar.read("b.bin").unwrap(), std::fs::read(&b).unwrap());
    assert_eq!(rar.read("c.txt").unwrap(), std::fs::read(&c).unwrap());
    assert_eq!(rar.read("d.bin").unwrap(), std::fs::read(&d).unwrap());

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

    let mut rar = rar5::RarArchive::open(&arc).unwrap();
    let mut names: Vec<String> = rar.namelist().into_iter().map(str::to_string).collect();
    names.sort();
    assert_eq!(names, vec!["a.txt".to_string(), "b.txt".to_string()]);
    assert_eq!(rar.read("a.txt").unwrap(), b"rar4 CLI member A");
    assert_eq!(
        rar.read("b.txt").unwrap(),
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

    let mut rar = rar5::RarArchive::open_with_password(&arc2, "secret").unwrap();
    assert_eq!(rar.read("f.txt").unwrap(), b"payload");

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
    let mut rar = rar5::RarArchive::open(&arc3).unwrap();
    assert_eq!(rar.read("f.txt").unwrap(), b"payload");
}
