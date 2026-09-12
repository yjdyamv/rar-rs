use rar_rs::{CompressionLevel, EntryWriteOptions};

use crate::support::{RAR_CLI, UNRAR_CLI, cli_names, create_duplicate_archive, make_temp_dir};
// ── WinRAR CLI parity: quiet mode, ch/p commands, -o-, -z, list variants ──

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
            rar_rs::ArchiveWriter::create_with(&archive, rar_rs::WriterOptions::default()).unwrap();
        rar.add_path(
            dir.path().join("MiXeD.TXT"),
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    assert_eq!(cli_names(&archive), ["MiXeD.TXT"]);
    let status = std::process::Command::new(RAR_CLI)
        .args(["ch", "-cl"])
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(cli_names(&archive), ["mixed.txt"]);
    let mut rar = rar_rs::ArchiveReader::open(&archive).unwrap();
    let mixed_id = rar.unique_entry("mixed.txt").unwrap();
    assert_eq!(rar.read_entry(mixed_id).unwrap(), b"x");
}

#[test]
fn cli_print_writes_member_to_stdout() {
    let dir = make_temp_dir();
    let archive = dir.path().join("p.rar");
    {
        let mut rar =
            rar_rs::ArchiveWriter::create_with(&archive, rar_rs::WriterOptions::default()).unwrap();
        rar.add_bytes(
            "a.txt",
            b"hello p",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
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
    let mut rar =
        rar_rs::ArchiveWriter::create_with(&exact_archive, rar_rs::WriterOptions::default())
            .unwrap();
    rar.add_bytes(
        "same.bin",
        b"exact",
        EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
    )
    .unwrap();
    rar.add_bytes(
        "dir/same.bin",
        b"basename only",
        EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
    )
    .unwrap();
    rar.finish().unwrap();
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
            rar_rs::ArchiveWriter::create_with(&archive, rar_rs::WriterOptions::default()).unwrap();
        rar.add_bytes(
            "f.txt",
            b"new",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
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
            rar_rs::ArchiveWriter::create_with(&archive, rar_rs::WriterOptions::default()).unwrap();
        rar.add_bytes(
            "f.txt",
            b"x",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
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
            rar_rs::ArchiveWriter::create_with(&archive, rar_rs::WriterOptions::default()).unwrap();
        rar.add_bytes(
            "a.txt",
            b"aaa",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.add_bytes(
            "b.bin",
            b"bbbb",
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
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
