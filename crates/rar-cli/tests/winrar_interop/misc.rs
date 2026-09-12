#![cfg(unix)]
use rar_rs::{ArchiveReader, ExtractOptions};

use crate::support::{temp_dir, unrar_bin, unrar_test};

/// Symlink members (`-ol`) round-trip with rar-rs and decode through WinRAR
/// as redirects (no data). Unix-only source symlinks; Windows runs WinRAR
/// to confirm the redirect archive validates (the symlink target is not
/// recreated by WinRAR, but the member must test cleanly with an empty
/// data stream).
#[cfg(unix)]
#[test]
fn symlink_member_roundtrips_and_winrar_reads_redirect() {
    let dir = temp_dir();
    let src = dir.path().join("lnk");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("target.txt"), b"target").unwrap();
    std::os::unix::fs::symlink("target.txt", src.join("lnk.txt")).unwrap();

    let arc = dir.path().join("ol.rar");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ol", "-idq"])
        .arg(&arc)
        .arg("lnk")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // rar-rs restores the symlink on extract.
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let mut rar = ArchiveReader::open(&arc).unwrap();
    rar.extract_all_with_options(&out, ExtractOptions::default())
        .unwrap();
    assert_eq!(
        std::fs::read_link(out.join("lnk/lnk.txt")).unwrap(),
        std::path::Path::new("target.txt")
    );

    // WinRAR must test the redirect archive (empty data stream).
    if let Some(_unrar) = unrar_bin() {
        let (ok, out_log) = unrar_test(&arc, None);
        assert!(ok, "UnRAR rejected our -ol symlink archive:\n{out_log}");
    }
}
