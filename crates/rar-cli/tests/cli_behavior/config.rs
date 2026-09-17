use std::path::Path;

use crate::support::{RAR_CLI, make_temp_dir, status_retrying_busy};
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
    // `rarfiles.lst` is looked up next to the executable — a location shared
    // by every test in this binary (and every parallel test thread). Run a
    // private copy of the rar binary so this test never drops a stray list
    // into `target/debug/`, where the other solid-order tests would pick it
    // up and flake.
    let bin_dir = make_temp_dir();
    let bin = bin_dir.path().join(
        Path::new(RAR_CLI)
            .file_name()
            .expect("rar binary file name"),
    );
    std::fs::copy(RAR_CLI, &bin).expect("copy the rar binary");
    std::fs::write(dir.path().join("aaa.cpp"), b"a").unwrap();
    std::fs::write(dir.path().join("f1.cpp"), b"b").unwrap();
    std::fs::write(dir.path().join("ddd.cpp"), b"c").unwrap();
    std::fs::write(dir.path().join("bbb.h"), b"d").unwrap();
    std::fs::write(dir.path().join("ccc.txt"), b"e").unwrap();
    std::fs::create_dir_all(dir.path().join("subd")).unwrap();
    std::fs::write(dir.path().join("subd").join("nested.txt"), b"n").unwrap();
    std::fs::write(dir.path().join("subd").join("deep.cpp"), b"p").unwrap();

    // rarfiles.lst next to the (private copy of the) rar binary.
    let lst = bin_dir.path().join("rarfiles.lst");
    std::fs::write(&lst, "; test list\n*.txt\nf*.cpp\n*.cpp\n$default\n").unwrap();
    let result = std::panic::catch_unwind(|| {
        let archive = dir.path().join("rfl.rar");
        let status = status_retrying_busy(
            std::process::Command::new(&bin)
                .args(["a", "-s", "-idq"])
                .arg(&archive)
                .args(["*.cpp", "*.h", "*.txt", "subd"])
                .current_dir(dir.path()),
        );
        assert!(status.success());

        let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let names: Vec<String> = rar
            .entries()
            .map(|e| e.name().to_string())
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
