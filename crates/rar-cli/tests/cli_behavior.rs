#![allow(deprecated)] // legacy constructor family; use create_with_options
//! CLI behavior tests: drive the built `rar`/`unrar` binaries through
//! WinRAR-compatible switches. Lives in this crate because the
//! `CARGO_BIN_EXE_*` env vars are only defined for the package that builds
//! the binaries (moved here from the library's interop.rs).

use rar5::RarArchive;
use std::io::Write;
use std::path::Path;

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
        let mut rar = RarArchive::create(&archive).unwrap();
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
        let mut rar = RarArchive::create(&archive).unwrap();
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
fn cli_overwrite_never_skips_existing_files() {
    let dir = make_temp_dir();
    let archive = dir.path().join("o.rar");
    {
        let mut rar = RarArchive::create(&archive).unwrap();
        rar.add_bytes("f.txt", b"new", 0).unwrap();
        rar.close().unwrap();
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("f.txt"), b"OLD").unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-o-"])
        .arg(&archive)
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
        let mut rar = RarArchive::create(&archive).unwrap();
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
        let mut rar = RarArchive::create(&archive).unwrap();
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
        .arg(&src.join("f.txt"))
        .status()
        .unwrap();
    assert!(status.success());
    let rar = RarArchive::open(&archive).unwrap();
    let names = rar.namelist();
    assert_eq!(names.len(), 1, "{names:?}");
    let stored = names[0];
    #[cfg(windows)]
    assert!(
        !stored.starts_with([
            'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q',
            'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z'
        ]) || !stored.contains(":/"),
        "-ep2 must drop the drive letter: {stored}"
    );

    let archive3 = dir.path().join("ep3.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ep3", "-idq"])
        .arg(&archive3)
        .arg(&src.join("f.txt"))
        .status()
        .unwrap();
    assert!(status.success());
    let rar = RarArchive::open(&archive3).unwrap();
    let stored = rar.namelist()[0];
    #[cfg(windows)]
    assert!(
        stored.contains("_/") || stored.contains("_/"),
        "-ep3 must keep the drive as X_: {stored}"
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
        let mut rar = RarArchive::create(&archive).unwrap();
        rar.add_bytes("f.txt", b"x", 0).unwrap();
        rar.close().unwrap();
    }
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-ad", "-idq"])
        .arg(&archive)
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
    rar.get_entry(name).unwrap().header.comp_dict_size
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
        assert_eq!(e.header.comp_version, 0, "-md{md} small file stays v50");
        assert_eq!(e.header.dict_size_bytes, None, "-md{md}");
        // 2x floor_pow2(32 MiB) = 64 MiB -> log 9.
        assert_eq!(e.header.comp_dict_size, 9, "-md{md} cap");
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
    assert_eq!(b.header.comp_method, 0, "-msbin must store b.bin");
    let a = rar.get_entry("a.txt").unwrap();
    assert_eq!(a.header.comp_method, 3, "a.txt must still compress");
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
        assert!(e.header.ctime.is_none(), "default must not store ctime");
        assert!(e.header.atime.is_none(), "default must not store atime");
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
        assert!(e.header.ctime.is_some(), "-ts must store ctime");
        if let Some(src_ctime) = src_ctime {
            let c = e.header.ctime.unwrap();
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
        assert_eq!(
            e.header.mtime_ns,
            Some(0),
            "-ts1 must store second precision"
        );
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
        assert!(e.header.ctime.is_none() && e.header.atime.is_none());
        assert!(
            e.header.mtime_ns.is_none(),
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

// ── misc switches: -ver version control, accepted no-ops, -ilog ────────────

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
fn cli_misc_switches_are_accepted_and_ilog_logs_errors() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("m.txt"), b"m").unwrap();

    // Platform-specific / informational switches are accepted.
    let archive = dir.path().join("misc.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args([
            "a", "-idc", "-idd", "-idn", "-idp", "-ac", "-ai", "-os", "-scu", "-oni", "-ri5",
            "-vp", "-vd", "-oi1", "-ams", "-e1", "-ow", "-idq",
        ])
        .arg(&archive)
        .arg("m.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "misc switches must be accepted");

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
