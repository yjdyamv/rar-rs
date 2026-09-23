//! Byte-level container assertions: locator records, quick-open caches and FILE_TIME extra records.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{CompressionLevel, EntryWriteOptions};

#[test]
fn quick_open_record_written_with_correct_relative_locator() {
    let dir = make_temp_dir();
    let path = dir.path().join("qo.rar");
    let payload = b"quick open payload ".repeat(1000);
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().quick_open(true),
        )
        .unwrap();
        rar.add_bytes(
            "f1.bin",
            &payload,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.add_bytes(
            "f2.bin",
            &vec![7u8; 4096],
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }

    let mut rar = rar_rs::ArchiveReader::open(&path).unwrap();
    let id1 = rar.unique_entry("f1.bin").unwrap();
    assert_eq!(rar.read_entry(id1).unwrap(), payload);
    let id2 = rar.unique_entry("f2.bin").unwrap();
    assert_eq!(rar.read_entry(id2).unwrap(), vec![7u8; 4096]);

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
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default().recovery_percent(10),
        )
        .unwrap();
        rar.add_bytes(
            "a.bin",
            &b"recovery test payload ".repeat(1000),
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
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
        let mut rar = rar_rs::ArchiveWriter::create(&path).unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    // The writer emits the FILE_TIME extra record in the running platform's
    // official form: flags 0x13 + Unix seconds + nanoseconds on Unix, or
    // flags 0x02 + a Windows FILETIME on Windows (where the header's 4-byte
    // Unix mtime is absent and the record is the only time carrier).
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
            let mut expected = vec![0x0a, 0x03];
            if cfg!(windows) {
                expected.push(0x02);
                // Seconds from 1601-01-01 to 1970-01-01, in 100 ns ticks.
                const EPOCH_DELTA_SECS: u64 = 11_644_473_600;
                let filetime = (disk_secs.as_secs() + EPOCH_DELTA_SECS) * 10_000_000
                    + u64::from(disk_ns / 100);
                expected.extend_from_slice(&filetime.to_le_bytes());
            } else {
                expected.push(0x13);
                expected.extend_from_slice(&(disk_secs.as_secs() as u32).to_le_bytes());
                expected.extend_from_slice(&disk_ns.to_le_bytes());
            }
            assert_eq!(
                extra,
                &expected[..],
                "FILE_TIME record must match the official format"
            );
            // The record is the member's only time carrier: the header must
            // not also set FILE_FLAG_TIME_UNIX (no double write).
            assert_eq!(
                file_flags & 0x0002,
                0,
                "a FILE_TIME record replaces the header mtime"
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
        let mut rar = rar_rs::ArchiveReader::open(&path).unwrap();
        let id = rar.unique_entry("ns.bin").unwrap();
        assert_eq!(rar.entry(id).unwrap().mtime_ns(), Some(disk_ns));
        rar.extract_entry(id, &out).unwrap();
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

/// A caller-supplied size estimate reserves the locator offset fields at the
/// width WinRAR uses for that size, so the main header matches its byte
/// layout; without one the default 5-byte fields are used.
#[test]
fn size_estimate_selects_the_locator_offset_width() {
    let dir = make_temp_dir();
    let build = |name: &str, options: rar_rs::WriterOptions| -> u64 {
        let path = dir.path().join(name);
        let mut rar = rar_rs::ArchiveWriter::create_with(&path, options).unwrap();
        rar.add_bytes("a.txt", b"x", EntryWriteOptions::new())
            .unwrap();
        rar.finish().unwrap();
        std::fs::metadata(&path).unwrap().len()
    };

    let w3 = build(
        "w3.rar",
        rar_rs::WriterOptions::default().estimated_size(400),
    );
    let w4 = build(
        "w4.rar",
        rar_rs::WriterOptions::default().estimated_size(60_000),
    );
    let w5 = build(
        "w5.rar",
        rar_rs::WriterOptions::default().estimated_size(4_000_000),
    );
    let w6 = build(
        "w6.rar",
        rar_rs::WriterOptions::default().estimated_size(20_000_000),
    );
    assert_eq!(w4 - w3, 1, "the 512-byte bucket adds one offset byte");
    assert_eq!(w5 - w4, 1);
    assert_eq!(w6 - w5, 1);

    // A width-5 estimate is byte-identical to the default (no estimate).
    let default = build("default.rar", rar_rs::WriterOptions::default());
    assert_eq!(default, w5, "the default is a 5-byte reservation");
}

/// The locator size estimate is a RAR5 option and must not be silently
/// dropped by a legacy writer.
#[test]
fn size_estimate_is_rar5_only_and_nonzero() {
    let dir = make_temp_dir();
    for (name, options) in [
        (
            "legacy.rar",
            rar_rs::WriterOptions::default()
                .compression(rar_rs::ArchiveVersion::V29)
                .estimated_size(1000),
        ),
        (
            "zero.rar",
            rar_rs::WriterOptions::default().estimated_size(0),
        ),
    ] {
        let err = rar_rs::ArchiveWriter::create_with(dir.path().join(name), options).unwrap_err();
        assert!(
            matches!(err, rar_rs::RarError::InvalidOption(_)),
            "{name}: unexpected {err:?}"
        );
    }
}

/// `(file_flags, extra area)` of the file-header block whose stored name is
/// `want`; `None` for any other block.
fn file_header_flags_and_extra(body: &[u8], want: &[u8]) -> Option<(u64, Vec<u8>)> {
    let (_, mut q) = read_vint(body, 0); // block type
    let (flags, n) = read_vint(body, q);
    q = n;
    if flags & 0x0001 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    if flags & 0x0002 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    let (file_flags, n) = read_vint(body, q);
    q = n;
    for _ in 0..2 {
        // unpacked size, attributes
        let (_, n) = read_vint(body, q);
        q = n;
    }
    if file_flags & 0x0002 != 0 {
        q += 4; // header mtime
    }
    if file_flags & 0x0004 != 0 {
        q += 4; // CRC32
    }
    for _ in 0..2 {
        // comp info, host OS
        let (_, n) = read_vint(body, q);
        q = n;
    }
    let (nl, n) = read_vint(body, q);
    q = n;
    if &body[q..q + nl as usize] != want {
        return None;
    }
    q += nl as usize;
    Some((file_flags, body[q..].to_vec()))
}

/// A whole-second modification time rides the header's 4-byte field on Unix
/// with no FILE_TIME record; on Windows the record is always the carrier and
/// the header field stays clear. Either way the member carries exactly one
/// time, matching WinRAR 7.23.
#[test]
fn whole_second_mtime_has_no_file_time_record_on_unix() {
    let dir = make_temp_dir();
    let src = dir.path().join("sec.bin");
    std::fs::write(&src, b"sec test").unwrap();
    let target = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 0);
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(target))
        .unwrap();

    let path = dir.path().join("sec.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create(&path).unwrap();
        rar.add_path(&src, EntryWriteOptions::new()).unwrap();
        rar.finish().unwrap();
    }

    let bytes = std::fs::read(&path).unwrap();
    let mut seen = false;
    for block in scan_blocks(&bytes) {
        if block.block_type != 0x02 {
            continue;
        }
        let Some((file_flags, extra)) = file_header_flags_and_extra(&block.body, b"sec.bin") else {
            continue;
        };
        seen = true;
        if cfg!(unix) {
            assert_ne!(
                file_flags & 0x0002,
                0,
                "Unix keeps a whole-second mtime in the header field"
            );
            assert!(
                extra.is_empty(),
                "a whole-second mtime needs no FILE_TIME record on Unix, got {extra:02x?}"
            );
        } else {
            assert_eq!(
                file_flags & 0x0002,
                0,
                "Windows never uses the header time field"
            );
            assert!(
                !extra.is_empty(),
                "Windows keeps the time in the FILE_TIME record"
            );
        }
    }
    assert!(seen, "the sec.bin file header must be present");
}
