//! Official-tool regression for RR-bearing edits on RAR 1.5–2.9 archives
//! whose archive comment is embedded in the main header.
//!
//! The fixture (`fixtures/rar40/rar2/comment_nopsw.rar`) is WinRAR 2.02's
//! own output and carries three embedded comments. Official UnRAR reports
//! one silent error per comment-bearing header and exits 3 even on the
//! untouched file (see the fixture README), while every member still tests
//! `OK`. The check here is therefore: after our `rar rr` edit the members
//! still test `OK` with the same unrar exit code and no new corruption
//! message — before the fix the main header was truncated to 13 bytes and
//! every following block parsed 38 bytes late.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rar40/rar2/comment_nopsw.rar")
}

/// `SA_OFFICIAL_UNRAR`, else the project's WinRAR cache (6.23 first: the
/// last RAR4-producing release named in the bug report). `None` skips the
/// official check.
fn official_unrar() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SA_OFFICIAL_UNRAR") {
        return Some(PathBuf::from(path));
    }
    let exe = if cfg!(windows) { "UnRAR.exe" } else { "unrar" };
    [
        "../../.cache/winrar/6-23",
        "../.cache/winrar/6-23",
        ".cache/winrar/6-23",
        "../../.cache/winrar/7-23",
        "../.cache/winrar/7-23",
        ".cache/winrar/7-23",
    ]
    .iter()
    .map(|dir| Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(exe))
    .find(|bin| bin.exists())
}

/// `unrar t` result: exit code and the merged stdout+stderr text.
fn unrar_test(unrar: &Path, archive: &Path) -> (i32, String) {
    let output = Command::new(unrar)
        .arg("t")
        .arg(archive)
        .output()
        .expect("spawn official unrar");
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), combined)
}

/// Standard (IEEE, reflected) CRC-32, used to hand-build the 7-byte
/// ENDARC block the fixture lacks.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// A plain ENDARC_HEAD (0x7b, flags 0, head_size 7).
fn endarc_block() -> [u8; 7] {
    let mut block = [0u8; 7];
    block[2] = 0x7b;
    block[5..7].copy_from_slice(&7u16.to_le_bytes());
    let crc = (crc32(&block[2..]) & 0xffff) as u16;
    block[0..2].copy_from_slice(&crc.to_le_bytes());
    block
}

/// The edited copy must keep every member testable with official UnRAR:
/// same exit code as the untouched fixture copy and an `OK` per member.
#[test]
fn rr_edit_keeps_official_unrar_members_ok() {
    let Some(unrar) = official_unrar() else {
        eprintln!("SKIP: official UnRAR not found (set SA_OFFICIAL_UNRAR)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();

    // The fixture predates the optional end-of-archive marker; our editor
    // requires one. Both the baseline and the edited copy carry it.
    let mut bytes = std::fs::read(fixture()).unwrap();
    bytes.extend_from_slice(&endarc_block());
    let baseline = dir.path().join("baseline.rar");
    std::fs::write(&baseline, &bytes).unwrap();

    let (baseline_exit, baseline_out) = unrar_test(&unrar, &baseline);
    assert!(
        baseline_out.contains("FILE1.TXT") && baseline_out.contains("FILE2.TXT"),
        "precondition: unrar must see both members:\n{baseline_out}"
    );

    // `rar rr` on the embedded-comment archive.
    let edited = dir.path().join("edited.rar");
    std::fs::copy(&baseline, &edited).unwrap();
    {
        let mut editor = rar_rs::ArchiveEditor::open(&edited).unwrap();
        editor
            .apply(rar_rs::EditPlan::new().set_recovery(5))
            .unwrap();
    }
    // Our own reader agrees the members extract.
    {
        let mut reader = rar_rs::ArchiveReader::open(&edited).unwrap();
        let file1 = reader.unique_entry("FILE1.TXT").unwrap();
        assert_eq!(reader.read_entry(file1).unwrap(), b"file1\r\n");
        let file2 = reader.unique_entry("FILE2.TXT").unwrap();
        assert_eq!(reader.read_entry(file2).unwrap(), b"file2\r\n");
    }

    let (edited_exit, edited_out) = unrar_test(&unrar, &edited);
    eprintln!("unrar baseline exit={baseline_exit}:\n{baseline_out}");
    eprintln!("unrar edited exit={edited_exit}:\n{edited_out}");
    assert!(
        edited_out.contains("FILE1.TXT") && edited_out.contains("FILE2.TXT"),
        "the edited archive must still list both members:\n{edited_out}"
    );
    assert!(
        edited_out.matches("OK").count() >= 2,
        "the edited archive must still test both members OK:\n{edited_out}"
    );
    assert!(
        !edited_out.contains("Unexpected end of archive")
            && !edited_out.contains("is not RAR archive"),
        "the edit introduced unrar-visible corruption:\n{edited_out}"
    );
    assert_eq!(
        edited_exit, baseline_exit,
        "the edit must not change unrar's verdict (the fixture itself exits {baseline_exit} \
         on its embedded comments):\n{edited_out}"
    );
}
