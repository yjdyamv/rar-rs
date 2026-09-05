//! Byte-level container assertions: locator records, quick-open caches and FILE_TIME extra records.

#![allow(deprecated)] // legacy facade used for control archives

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::RarArchive;

#[test]
fn quick_open_record_written_with_correct_relative_locator() {
    let dir = make_temp_dir();
    let path = dir.path().join("qo.rar");
    let payload = b"quick open payload ".repeat(1000);
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &path,
            rar_rs::CreateOptions {
                quick_open: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("f1.bin", &payload, 3).unwrap();
        rar.add_bytes("f2.bin", &vec![7u8; 4096], 0).unwrap();
        rar.close().unwrap();
    }

    let mut rar = rar_rs::RarArchive::open(&path).unwrap();
    assert_eq!(rar.read("f1.bin").unwrap(), payload);
    assert_eq!(rar.read("f2.bin").unwrap(), vec![7u8; 4096]);

    let bytes = std::fs::read(&path).unwrap();
    let qo_pos = service_offset(&bytes, "QO");
    let (_, qo, rr) = main_header_locator(&bytes);
    assert!(rr.is_none(), "no recovery locator expected");
    assert_eq!(
        qo.unwrap(),
        qo_pos as u64 - 8,
        "QO offset must be relative to archive start"
    );
}

#[test]
fn recovery_locator_offset_is_relative_to_archive_start() {
    let dir = make_temp_dir();
    let path = dir.path().join("rr.rar");
    {
        let mut rar = rar_rs::RarArchive::create_with_options(
            &path,
            rar_rs::CreateOptions {
                recovery_percent: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("a.bin", &b"recovery test payload ".repeat(1000), 3)
            .unwrap();
        rar.close().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let rr_pos = service_offset(&bytes, "RR");
    let (_, _, rr) = main_header_locator(&bytes);
    assert_eq!(
        rr.unwrap(),
        rr_pos as u64 - 8,
        "RR offset must be relative to archive start"
    );
}

#[test]
fn nanosecond_mtime_roundtrip() {
    let dir = make_temp_dir();
    let src = dir.path().join("ns.bin");
    std::fs::write(&src, b"ns test").unwrap();
    // A file with sub-second mtime precision. NTFS stores 100 ns units, so
    // the value read back from disk is platform-quantized; everything below
    // compares against the *actual* on-disk timestamp instead of the
    // requested one, keeping the format check exact on every platform.
    let target = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
    let times = std::fs::FileTimes::new().set_modified(target);
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(times)
        .unwrap();
    let disk_mtime = std::fs::metadata(&src).unwrap().modified().unwrap();
    let disk_secs = disk_mtime.duration_since(std::time::UNIX_EPOCH).unwrap();
    let disk_ns = disk_secs.subsec_nanos();

    let path = dir.path().join("ns.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&path, rar_rs::CreateOptions::default()).unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    // The writer emits the FILE_TIME extra record (byte-identical to the
    // official `rar` format: flags 0x13 + seconds + nanoseconds).
    let bytes = std::fs::read(&path).unwrap();
    for block in scan_blocks(&bytes) {
        if block.block_type == 0x02 {
            let (_, mut q) = read_vint(&block.body, 0);
            let (flags, n) = read_vint(&block.body, q);
            q = n;
            if flags & 0x0001 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            if flags & 0x0002 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            let (file_flags, n) = read_vint(&block.body, q);
            q = n;
            for _ in 0..2 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            if file_flags & 0x0002 != 0 {
                q += 4;
            }
            if file_flags & 0x0004 != 0 {
                q += 4;
            }
            for _ in 0..2 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            let (nl, n) = read_vint(&block.body, q);
            q = n;
            let name = &block.body[q..q + nl as usize];
            assert_eq!(name, b"ns.bin");
            let extra = &block.body[q + nl as usize..];
            let mut expected = vec![0x0a, 0x03, 0x13];
            expected.extend_from_slice(&(disk_secs.as_secs() as u32).to_le_bytes());
            expected.extend_from_slice(&disk_ns.to_le_bytes());
            assert_eq!(
                extra,
                &expected[..],
                "FILE_TIME record must match the official format"
            );
            #[cfg(unix)]
            assert_eq!(
                disk_secs.as_secs(),
                1_700_000_000,
                "ext4 keeps the exact requested timestamp"
            );
            #[cfg(unix)]
            assert_eq!(disk_ns, 123_456_789, "ext4 keeps exact nanoseconds");
        }
    }

    // Reading it back restores the nanosecond mtime on extraction.
    let out = dir.path().join("out");
    {
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(rar.get_entry("ns.bin").unwrap().mtime_ns(), Some(disk_ns));
        rar.extract("ns.bin", &out).unwrap();
    }
    let extracted = std::fs::metadata(out.join("ns.bin")).unwrap();
    let restored = extracted
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    assert_eq!(restored.as_secs(), disk_secs.as_secs());
    assert_eq!(restored.subsec_nanos(), disk_ns);
}
