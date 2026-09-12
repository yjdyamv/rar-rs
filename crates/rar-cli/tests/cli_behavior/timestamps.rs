use crate::support::{RAR_CLI, UNRAR_CLI, created_time, make_temp_dir};
// ── -ts file time save/restore (WinRAR 7.23 aligned) ───────────────────────

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
        let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let e = rar.entry(rar.unique_entry("t.txt").unwrap()).unwrap();
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
        let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let e = rar.entry(rar.unique_entry("t.txt").unwrap()).unwrap();
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
        let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let e = rar.entry(rar.unique_entry("t.txt").unwrap()).unwrap();
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
        let rar = rar_rs::ArchiveReader::open(&archive).unwrap();
        let e = rar.entry(rar.unique_entry("t.txt").unwrap()).unwrap();
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
