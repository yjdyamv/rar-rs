use crate::support::{RAR_CLI, cli_names, make_temp_dir, make_tree};
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
        None => {
            assert!(
                std::env::var_os("SA_REQUIRE_OFFICIAL").is_none(),
                "SA_OFFICIAL_UNRAR is required (SA_REQUIRE_OFFICIAL is set)"
            );
            eprintln!("SKIP: SA_OFFICIAL_UNRAR not set");
            return;
        }
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
