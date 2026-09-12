use std::path::Path;

use crate::support::{RAR_CLI, make_temp_dir, rarfiles_lst_lock};
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
