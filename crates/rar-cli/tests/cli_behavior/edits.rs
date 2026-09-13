use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir};
/// RAR4 multi-volume sets support the header-level edits official `rar`
/// supports: rename (and lock) rewrite each volume in place, while delete is
/// refused exactly like WinRAR's "Cannot modify volume".
#[test]
fn cli_rar4_multivolume_rename_and_lock() {
    let dir = make_temp_dir();
    let mut content = Vec::new();
    let mut seed = 7u32;
    while content.len() < 90_000 {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        content.push((seed >> 16) as u8);
    }
    std::fs::write(dir.path().join("a.bin"), &content).unwrap();
    std::fs::write(dir.path().join("b.bin"), vec![0x5Au8; 30_000]).unwrap();

    // Our RAR4 writer names the set `.rar`/`.r00`/... .
    let first = dir.path().join("mv.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-v20k", "-idq"])
        .arg(&first)
        .args(["a.bin", "b.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "create a RAR4 volume set");
    assert!(
        dir.path().join("mv.r00").exists(),
        "expected a second volume"
    );

    // Rename: the set must stay readable under the new name.
    let status = std::process::Command::new(RAR_CLI)
        .args(["rn", "-idq"])
        .arg(&first)
        .args(["a.bin", "renamed.bin"])
        .status()
        .unwrap();
    assert!(status.success(), "rename in a RAR4 volume set");

    let mut reader = rar_rs::ArchiveReader::open(&first).unwrap();
    let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "renamed.bin"), "{names:?}");
    let id = reader.unique_entry("renamed.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), content);

    // Delete is refused (official parity), not silently ignored.
    let delete = std::process::Command::new(RAR_CLI)
        .args(["d", "-idq"])
        .arg(&first)
        .arg("renamed.bin")
        .output()
        .unwrap();
    assert!(
        !delete.status.success(),
        "delete on a RAR4 volume set must fail"
    );

    // Lock patches the first volume's main header; a later edit is refused.
    let status = std::process::Command::new(RAR_CLI)
        .args(["k", "-idq"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success(), "lock a RAR4 volume set");
    let rename_after_lock = std::process::Command::new(RAR_CLI)
        .args(["rn", "-idq"])
        .arg(&first)
        .args(["b.bin", "c.bin"])
        .output()
        .unwrap();
    assert!(
        !rename_after_lock.status.success(),
        "a locked RAR4 volume set must refuse edits"
    );
}

/// Archive comments are header-level too: official `rar c` puts the `CMT`
/// block after the first volume's main header (and no other volume changes);
/// set/replace/clear must keep the set valid.
#[test]
fn cli_rar4_multivolume_archive_comment_roundtrips() {
    let dir = make_temp_dir();
    for i in 1u8..=3 {
        std::fs::write(dir.path().join(format!("t{i}.txt")), vec![b'a' + i; 9000]).unwrap();
    }
    let first = dir.path().join("cmt.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-v20k", "-idq"])
        .arg(&first)
        .args(["t1.txt", "t2.txt", "t3.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "create a RAR4 volume set");
    assert!(
        dir.path().join("cmt.r00").exists(),
        "expected a second volume"
    );

    let comment = dir.path().join("comment.txt");
    let set_comment = |body: &[u8]| {
        std::fs::write(&comment, body).unwrap();
        std::process::Command::new(RAR_CLI)
            .args(["c", "-idq"])
            .arg(format!("-z{}", comment.display()))
            .arg(&first)
            .status()
            .unwrap()
            .success()
    };
    let read_comment = || {
        let out = std::process::Command::new(RAR_CLI)
            .args(["cw"])
            .arg(&first)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    assert!(set_comment(b"multi-volume comment\n"));
    assert_eq!(read_comment(), "multi-volume comment");
    assert!(set_comment(b"replaced\n"));
    assert_eq!(read_comment(), "replaced");
    assert!(set_comment(b""));
    assert_eq!(read_comment(), "");

    let reader = rar_rs::ArchiveReader::open(&first).unwrap();
    assert_eq!(reader.entries().count(), 3);
}

/// Header-encrypted RAR5 archives refuse rename and archive-comment edits
/// (the transaction cannot re-encrypt rewritten headers) and the archive is
/// left byte-identical; delete and recovery-record edits keep working, as
/// does the RAR4 `-hp` comment path.
#[test]
fn cli_header_encrypted_rar5_edits_are_refused() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("g.txt"), b"two").unwrap();
    std::fs::write(dir.path().join("note.txt"), b"note").unwrap();
    let archive = dir.path().join("hp.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-hpsecret", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .arg("g.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let before = std::fs::read(&archive).unwrap();

    for args in [
        vec!["c", "-psecret", "-znote.txt"],
        vec!["rn", "-psecret", "-idq"],
    ] {
        let mut command = std::process::Command::new(RAR_CLI);
        command.args(&args).arg(&archive);
        if args[0] == "rn" {
            command.arg("f.txt").arg("zz.txt");
        }
        let output = command.current_dir(dir.path()).output().unwrap();
        assert!(
            !output.status.success(),
            "{args:?} must be refused on an -hp archive"
        );
        assert_eq!(
            std::fs::read(&archive).unwrap(),
            before,
            "{args:?} must not touch the archive"
        );
    }

    // Create-time `-z`/`-k` are rejected before anything is written.
    for (name, extra) in [("nz", "-znote.txt"), ("nk", "-k")] {
        let target = dir.path().join(format!("{name}.rar"));
        let status = std::process::Command::new(RAR_CLI)
            .args(["a", "-hpsecret", "-idq", extra])
            .arg(&target)
            .arg("f.txt")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(!status.success(), "a -hp {extra} must be rejected");
        assert!(!target.exists(), "{name}.rar must not be created");
    }

    // Delete still works on the encrypted archive.
    let status = std::process::Command::new(RAR_CLI)
        .args(["d", "-psecret", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-psecret", "-idq"])
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
}
