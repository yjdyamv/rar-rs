//! Regressions for four reported CLI bugs: `-ep`/`-ep1` basename loss,
//! local `-ta`/`-tb` dates, member-selector masks/directories and
//! case-folded `-x`/`-n` masks.
use crate::support::{RAR_CLI, UNRAR_CLI, cli_names, make_temp_dir, make_tree};

/// `-ep` / `-ep1` must keep every source whose stored basename collides:
/// RAR allows duplicate member names, and a name-keyed dedup silently
/// dropped the second file (exit 0, wrong member list).
#[test]
fn cli_ep_keeps_colliding_basenames() {
    let dir = make_temp_dir();
    for sub in ["a", "b"] {
        std::fs::create_dir_all(dir.path().join(sub)).unwrap();
    }
    std::fs::write(dir.path().join("a/f.txt"), b"first").unwrap();
    std::fs::write(dir.path().join("b/f.txt"), b"second").unwrap();

    for (switch, archive_name) in [("-ep", "ep.rar"), ("-ep1", "ep1.rar")] {
        let archive = dir.path().join(archive_name);
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", switch, "-idq"])
            .arg(&archive)
            .args(["a/f.txt", "b/f.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{switch}");
        assert_eq!(cli_names(&archive), ["f.txt", "f.txt"], "{switch}");

        let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let ids: Vec<_> = rar
            .entries()
            .filter(|entry| entry.name() == "f.txt")
            .map(|entry| entry.id())
            .collect();
        let mut payloads: Vec<Vec<u8>> =
            ids.iter().map(|id| rar.read_entry(*id).unwrap()).collect();
        payloads.sort();
        assert_eq!(payloads, [b"first".to_vec(), b"second".to_vec()]);

        let out = std::process::Command::new(RAR_CLI)
            .arg("lb")
            .arg(&archive)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|line| line.trim() == "f.txt")
                .count(),
            2,
            "{switch}: lb must list both members"
        );
    }
}

/// `-ta`/`-tb` compare against *local* civil dates, like `-tk`. The anchor
/// archive pins the exact instant of a local midnight, so the assertions
/// hold in every timezone.
#[test]
fn cli_absolute_time_filters_use_local_dates() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("seed.txt"), b"seed").unwrap();
    let anchor = dir.path().join("anchor.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tk2020-01-01", "-idq"])
        .arg(&anchor)
        .arg("seed.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    // The `-tk` archive mtime is the local civil time 2020-01-01 00:00:00.
    let local_midnight = std::fs::metadata(&anchor).unwrap().modified().unwrap();

    let file = dir.path().join("t.txt");
    std::fs::write(&file, b"body").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(local_midnight + std::time::Duration::from_secs(3 * 3600)),
        )
        .unwrap();

    // 03:00 into the local day: `-ta` (after local midnight) includes it...
    let after = dir.path().join("after.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ta2020-01-01", "-idq"])
        .arg(&after)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-ta must include 03:00 of its local day");
    assert_eq!(cli_names(&after), ["t.txt"]);

    // ...`-tb` (before local midnight) rejects it, with WinRAR's exit 10...
    let before = dir.path().join("before.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tb2020-01-01", "-idq"])
        .arg(&before)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(10), "-tb must use the local midnight");
    assert!(!before.exists());

    // ...and the next local day's midnight does include it.
    let next = dir.path().join("next.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-tb2020-01-02", "-idq"])
        .arg(&next)
        .arg("t.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "-tb of the next day must include it");
    assert_eq!(cli_names(&next), ["t.txt"]);
}

/// `-ta19700101` must not fail when the local civil date precedes the Unix
/// epoch (east-of-UTC zones): the filter saturates to 0 and a current file
/// is included in every timezone.
#[test]
fn cli_epoch_time_filter_works_in_east_of_utc_zones() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"body").unwrap();

    let archive = dir.path().join("epoch.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ta19700101", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "-ta19700101 must accept local dates before the Unix epoch"
    );
    assert_eq!(cli_names(&archive), ["f.txt"]);
}

/// Member selectors are masks and directory prefixes: a directory name
/// selects its subtree, a `*.txt` mask selects by name, and names fold
/// ASCII case on Windows.
#[test]
fn cli_member_selectors_match_masks_and_directories() {
    let dir = make_temp_dir();
    make_tree(dir.path());
    let archive = dir.path().join("sel.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(&archive)
        .args(["f1.txt", "f2.tmp", "sub"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // `rar x archive sub out\` extracts the whole `sub/` subtree.
    let out = dir.path().join("out-dir");
    std::fs::create_dir_all(&out).unwrap();
    let trailing = format!("{}{}", out.display(), std::path::MAIN_SEPARATOR);
    let status = std::process::Command::new(RAR_CLI)
        .args(["x", "-idq"])
        .arg(&archive)
        .arg("sub")
        .arg(&trailing)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "directory selector must extract");
    assert!(out.join("sub").is_dir());
    assert!(out.join("sub/f3.txt").exists());
    assert!(out.join("sub/f4.bin").exists());
    assert!(!out.join("f1.txt").exists(), "unselected member extracted");
    assert!(!out.join("f2.tmp").exists(), "unselected member extracted");

    // A wildcard selector matches by name (`*.txt` across directories).
    let out = dir.path().join("out-mask");
    std::fs::create_dir_all(&out).unwrap();
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["x", "-idq", "--dest"])
        .arg(&out)
        .arg(&archive)
        .arg("*.txt")
        .status()
        .unwrap();
    assert!(status.success(), "mask selector must extract");
    assert!(out.join("f1.txt").exists());
    assert!(out.join("sub/f3.txt").exists());
    assert!(!out.join("f2.tmp").exists(), "mask mismatch extracted");
    assert!(!out.join("sub/f4.bin").exists(), "mask mismatch extracted");

    // `p` shares the same selector, so the mask prints the matching members.
    let out = std::process::Command::new(RAR_CLI)
        .arg("p")
        .arg(&archive)
        .arg("*.txt")
        .output()
        .unwrap();
    assert!(out.status.success(), "mask selector must print");
    assert_eq!(out.stdout, b"onethree");

    // Selector case folds on Windows, like the filesystem and WinRAR.
    #[cfg(windows)]
    {
        let out = dir.path().join("out-case");
        std::fs::create_dir_all(&out).unwrap();
        let status = std::process::Command::new(UNRAR_CLI)
            .args(["x", "-idq", "--dest"])
            .arg(&out)
            .arg(&archive)
            .arg("SUB")
            .status()
            .unwrap();
        assert!(status.success(), "case-folded directory selector");
        assert!(out.join("sub/f3.txt").exists());
        assert!(out.join("sub/f4.bin").exists());
        let status = std::process::Command::new(UNRAR_CLI)
            .args(["x", "-idq", "--dest"])
            .arg(&out)
            .arg(&archive)
            .arg("F1.TXT")
            .status()
            .unwrap();
        assert!(status.success(), "case-folded mask selector");
        assert!(out.join("f1.txt").exists());
    }
}

/// `-x` / `-n` masks (and `-x@` list files) fold ASCII case on Windows,
/// like WinRAR and the `-ms` store-type masks.
#[cfg(windows)]
#[test]
fn cli_exclude_and_include_masks_fold_case() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("lower.txt"), b"lower").unwrap();
    std::fs::write(dir.path().join("keep.bin"), b"keep").unwrap();

    // -x<mask>: the uppercase mask excludes the lowercase file.
    let archive = dir.path().join("x.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-x*.TXT", "-idq"])
        .arg(&archive)
        .args(["lower.txt", "keep.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["keep.bin"]);

    // -n<mask>: only the matching file is stored.
    let archive = dir.path().join("n.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-n*.TXT", "-idq"])
        .arg(&archive)
        .args(["lower.txt", "keep.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["lower.txt"]);

    // -x@<listfile>: masks read from a file fold case too.
    std::fs::write(dir.path().join("masks.lst"), b"*.TXT\n").unwrap();
    let archive = dir.path().join("xat.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .arg(format!("-x@{}", dir.path().join("masks.lst").display()))
        .arg(&archive)
        .args(["lower.txt", "keep.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["keep.bin"]);
}
