use std::path::Path;
use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, ExtractOptions,
    WriterOptions,
};

use crate::support::{created_time, rar_bin, run, temp_dir, unrar_bin};

/// `-ts` timestamps interoperate in both directions: WinRAR restores our
/// stored ctime/atime on `x -ts`, and we parse + restore WinRAR's.
#[test]
fn ts_file_times_interop_with_winrar() {
    let dir = temp_dir();
    let src = dir.path().join("ts.bin");
    std::fs::write(&src, b"timestamp interop payload ".repeat(100)).unwrap();
    let src_ctime = created_time(&src);

    // Ours -> WinRAR: WinRAR's `x -ts` must restore mtime and (Windows)
    // creation time from our FILE_TIME extra record.
    let ours = dir.path().join("ours_ts.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &ours,
            WriterOptions::default().save_ctime(true).save_atime(true),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let ar = ArchiveReader::open(&ours).unwrap();
    let entry = ar.entry(ar.unique_entry("ts.bin").unwrap()).unwrap();
    assert!(entry.ctime().is_some(), "our -ts archive must store ctime");
    assert!(entry.atime().is_some(), "our -ts archive must store atime");
    if let Some(unrar) = unrar_bin() {
        let win = dir.path().join("win_ours_ts");
        std::fs::create_dir_all(&win).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-ts", "-y", "-idq"])
            .arg(&ours)
            .arg(&win));
        assert!(ok, "WinRAR x -ts failed on our archive:\n{out}");
        let extracted = win.join("ts.bin");
        let win_ctime = created_time(&extracted);
        if let (Some(a), Some(b)) = (src_ctime, win_ctime) {
            let diff = a
                .duration_since(b)
                .unwrap_or_else(|_| b.duration_since(a).unwrap());
            assert!(
                diff < std::time::Duration::from_secs(3),
                "WinRAR restored ctime {b:?}, source {a:?}"
            );
        }
    }

    // WinRAR -> ours: parse its FILE_TIME record and restore on extract.
    if let Some(rar) = rar_bin() {
        let theirs = dir.path().join("theirs_ts.rar");
        let (ok, out) = run(Command::new(&rar)
            .args(["a", "-ts", "-idq"])
            .arg(&theirs)
            .arg(&src));
        assert!(ok, "WinRAR -ts failed:\n{out}");
        let ar = ArchiveReader::open(&theirs).unwrap();
        let name = ar.entries().next().unwrap().name().to_string();
        let entry = ar.entry(ar.unique_entry(&name).unwrap()).unwrap();
        assert!(
            entry.ctime().is_some() && entry.atime().is_some(),
            "WinRAR -ts archive must carry ctime and atime"
        );
        let out_dir = dir.path().join("ours_from_winrar_ts");
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut ar = ArchiveReader::open(&theirs).unwrap();
        ar.extract_all_with_options(
            &out_dir,
            ExtractOptions {
                set_creation_time: true,
                set_access_time: true,
                ..Default::default()
            },
        )
        .unwrap();
        let extracted = out_dir.join(Path::new(&name).file_name().unwrap());
        let ours_ctime = created_time(&extracted);
        if let (Some(a), Some(b)) = (src_ctime, ours_ctime) {
            let diff = a
                .duration_since(b)
                .unwrap_or_else(|_| b.duration_since(a).unwrap());
            assert!(
                diff < std::time::Duration::from_secs(3),
                "we restored ctime {b:?}, source {a:?}"
            );
        }
    }
}
