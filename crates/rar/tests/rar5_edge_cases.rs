//! RAR5/RAR7 edge cases: erase errors, the REV5 entry route, quick-open
//! with NTFS streams, empty-member integrity, extracted attributes and the
//! wire model API.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

mod rar5_erase_volume_errors {
    //! RAR5 erase-everything regressions: deleting the last member must remove
    //! every volume of the set — including `.rev` recovery volumes — and must
    //! report an error instead of success when a file cannot be removed.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveEditor, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions,
    };

    fn stored() -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
    }

    fn patterned(len: usize, modulo: usize) -> Vec<u8> {
        (0..len).map(|index| (index % modulo) as u8).collect()
    }

    /// Build a small multi-volume RAR5 set with `.rev` recovery volumes.
    fn create_set(path: &std::path::Path) {
        let mut writer = ArchiveWriter::create_with(
            path,
            WriterOptions::new()
                .volume_size(30_000)
                .recovery_volumes_percent(50),
        )
        .unwrap();
        for index in 0..3 {
            let data = patterned(28_000, 251 + index);
            writer
                .add_bytes(&format!("m{index}.bin"), &data, stored())
                .unwrap();
        }
        writer.finish().unwrap();
    }

    fn set_files(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".rar") || name.ends_with(".rev"))
            .collect();
        names.sort();
        names
    }

    fn staging_leftovers(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.contains("rar5tmp") || name.contains("rar5bak") || name.contains("rar5commit")
            })
            .collect()
    }

    fn entry_ids(editor: &ArchiveEditor) -> Vec<rar_rs::EntryId> {
        editor.entries().map(|entry| entry.id()).collect()
    }

    #[test]
    fn erase_everything_removes_all_volumes_and_recovery_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("erase.rar");
        create_set(&path);

        let volumes = rar_rs::discover_volumes(&path);
        assert!(volumes.len() > 1, "precondition: multi-volume set");
        assert!(
            set_files(dir.path())
                .iter()
                .any(|name| name.ends_with(".rev")),
            "precondition: .rev files present"
        );

        let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
        let ids = entry_ids(&editor);
        assert_eq!(ids.len(), 3);
        assert_eq!(editor.delete_entries(&ids).unwrap(), 3);
        drop(editor);

        assert_eq!(
            set_files(dir.path()),
            Vec::<String>::new(),
            "every volume and .rev file must be erased"
        );
        assert!(staging_leftovers(dir.path()).is_empty());
    }

    /// A recovery file that cannot be removed (here: a directory occupying a
    /// `.rev` name, which `remove_file` refuses on every platform) must surface
    /// as an error and must not be silently skipped while reporting success.
    #[test]
    fn erase_surfaces_a_recovery_file_that_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stuck.rar");
        create_set(&path);

        let volumes = rar_rs::discover_volumes(&path);
        assert!(volumes.len() > 1, "precondition: multi-volume set");
        let stuck = dir.path().join("stuck.part01.rev");
        std::fs::create_dir(&stuck).unwrap();

        let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
        let ids = entry_ids(&editor);
        match editor.delete_entries(&ids) {
            Err(rar_rs::RarError::Io(_)) => {}
            other => {
                panic!("an unremovable recovery file must surface an I/O error, got {other:?}")
            }
        }
        assert!(stuck.is_dir(), "the unremovable path must remain");
    }

    /// Windows lock regression: a volume held open without sharing must surface
    /// an error (the removal used to be ignored and the erase reported success).
    #[cfg(windows)]
    #[test]
    fn erase_reports_a_locked_volume_instead_of_success() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.rar");
        create_set(&path);

        let volumes = rar_rs::discover_volumes(&path);
        assert!(volumes.len() > 1, "precondition: multi-volume set");

        // Collect the catalog before locking a later volume (the open scan
        // itself reads every volume).
        let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
        let ids = entry_ids(&editor);

        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&volumes[1])
            .unwrap();

        match editor.delete_entries(&ids) {
            Err(rar_rs::RarError::Io(_)) => {}
            other => panic!("a locked volume must surface an I/O error, got {other:?}"),
        }
        assert!(volumes[1].exists(), "the locked volume must not disappear");
        drop(locked);
    }
}

mod rev5_rev_entry {
    //! A REV5 `.rev` path passed to the rebuild entry point must route to the
    //! RAR5 recovery codec, not the legacy RAR 1.5–4.x one. Regression for the
    //! 8-byte `REV5_SIGNATURE` comparison that was truncated to 7 bytes and
    //! misrouted every REV5 set into `rev3` (`no recovery volumes found`).

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use std::fs;
    use std::path::PathBuf;

    use rar_rs::ArchiveWriter;

    #[test]
    fn rebuild_through_a_rev5_rev_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rcv.rar");
        let payload_a: Vec<u8> = (0..120_000u32).map(|i| (i % 251) as u8).collect();
        let payload_b: Vec<u8> = (0..60_000u32).map(|i| (i % 253) as u8).collect();
        {
            let mut rar = ArchiveWriter::create_with(
                &path,
                rar_rs::WriterOptions::default()
                    .volume_size(60_000)
                    .recovery_volume_count(2),
            )
            .unwrap();
            let opts0 = rar_rs::EntryWriteOptions::new()
                .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
            rar.add_bytes("a.bin", &payload_a, opts0).unwrap();
            rar.add_bytes("b.bin", &payload_b, opts0).unwrap();
            rar.finish().unwrap();
        }
        let volumes = rar_rs::discover_volumes(&path);
        assert!(volumes.len() > 1, "precondition: multi-volume set");
        let rev = dir.path().join("rcv.part1.rev");
        assert!(rev.exists(), "precondition: .rev files present");

        // Delete a middle volume and rebuild it through the `.rev` entry point
        // (like `rar rc set.part1.rev`).
        let victim: PathBuf = volumes[1].clone();
        fs::remove_file(&victim).unwrap();
        let rebuilt = rar_rs::rebuild_missing_volumes(&rev)
            .expect("a REV5 .rev path must rebuild through the RAR5 codec");
        assert!(rebuilt.contains(&victim), "the missing volume is rebuilt");

        let volumes = rar_rs::discover_volumes(&path);
        let mut rar = rar_rs::ArchiveReader::open(&volumes[0]).unwrap();
        assert_eq!(
            rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
            payload_a
        );
        assert_eq!(
            rar.read_entry(rar.unique_entry("b.bin").unwrap()).unwrap(),
            payload_b
        );
    }
}

mod quick_open_streams {
    //! Quick-open fast path vs. NTFS alternate data streams: the QO record
    //! caches file headers only, so extraction from a quick-open catalog must
    //! still discover the "STM" service records and restore the streams.

    #![cfg(windows)]

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveReader, ArchiveWriter, EntryWriteOptions, ExtractOptions, OpenOptions, ScanStrategy,
        WriterOptions,
    };

    /// Index of the volume holding the first "STM" service block (block type
    /// 0x03 with `BLOCK_FLAG_DEPENDS_PREV` 0x20), if any.
    fn stream_record_volume(volumes: &[std::path::PathBuf]) -> Option<usize> {
        use std::io::{Seek, SeekFrom};

        for (index, volume) in volumes.iter().enumerate() {
            let Ok(mut file) = std::fs::File::open(volume) else {
                continue;
            };
            // Every volume carries its own 8-byte RAR5 signature.
            file.seek(SeekFrom::Start(8)).ok()?;
            while let Ok(Some(meta)) = rar_rs::wire::read_block(&mut file, None) {
                if meta.block_type == 0x03 && meta.flags & 0x20 != 0 {
                    return Some(index);
                }
                // `read_block` stops at the data area; skip it explicitly.
                file.seek(SeekFrom::Start(meta.data_end)).ok()?;
            }
        }
        None
    }

    /// A multi-volume `-os` set can place the "STM" block in a later volume
    /// (the writer rolls to the next volume before emitting it). Extraction
    /// must read the record from the volume that holds it; the old reader
    /// always sought the primary volume and silently dropped the stream.
    #[test]
    fn multivolume_extraction_restores_ntfs_streams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("mkdir");
        let src = src_dir.join("owner.bin");
        // A compressible head plus an incompressible tail keeps the packed
        // member spanning several 64 KiB volumes (the random tail dominates the
        // packed size while the head keeps compression a net win).
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut payload = vec![0u8; 128 * 1024];
        payload.extend((0..300_000).map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
        }));
        std::fs::write(&src, &payload).expect("write member");
        let stream = b"alternate stream payload for a multi-volume set".repeat(20);
        std::fs::write(format!("{}{}", src.display(), ":ads"), &stream).expect("write stream");

        let archive = dir.path().join("streams-mv.rar");
        {
            let mut rar = ArchiveWriter::create_with(
                &archive,
                WriterOptions::default()
                    .save_streams(true)
                    .volume_size(64 * 1024),
            )
            .expect("create");
            rar.add_path_as(&src, "owner.bin", EntryWriteOptions::new())
                .expect("add");
            rar.finish().expect("close");
        }

        let volumes = rar_rs::discover_volumes(&archive);
        assert!(volumes.len() >= 2, "expected a split set");
        assert!(
            matches!(stream_record_volume(&volumes), Some(volume) if volume > 0),
            "the STM record must sit in a later volume for this regression test"
        );

        let out = dir.path().join("out");
        let mut reader = ArchiveReader::open(&archive).expect("open");
        reader
            .extract_all_with_options(&out, ExtractOptions::default())
            .expect("extract multi-volume");
        assert_eq!(std::fs::read(out.join("owner.bin")).unwrap(), payload);
        assert_eq!(
            std::fs::read(format!("{}{}", out.join("owner.bin").display(), ":ads"))
                .expect("restored stream"),
            stream,
            "the stream must be restored from its own volume"
        );
    }

    #[test]
    fn quick_open_extraction_restores_ntfs_streams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("mkdir");
        let src = src_dir.join("owner.bin");
        let payload = b"main stream data".repeat(8);
        std::fs::write(&src, &payload).expect("write member");
        let stream = b"alternate stream payload".repeat(4);
        std::fs::write(format!("{}{}", src.display(), ":ads"), &stream).expect("write stream");

        let archive = dir.path().join("streams.rar");
        {
            let mut rar = ArchiveWriter::create_with(
                &archive,
                WriterOptions::default().quick_open(true).save_streams(true),
            )
            .expect("create");
            rar.add_path_as(&src, "owner.bin", EntryWriteOptions::new())
                .expect("add");
            rar.finish().expect("close");
        }

        // Full-scan extraction is the control: it must restore the stream.
        let full_out = dir.path().join("out_full");
        {
            let mut full = ArchiveReader::open(&archive).expect("open");
            full.extract_all_with_options(&full_out, ExtractOptions::default())
                .expect("extract full scan");
        }
        assert_eq!(
            std::fs::read(format!(
                "{}{}",
                full_out.join("owner.bin").display(),
                ":ads"
            ))
            .unwrap(),
            stream,
            "full scan must restore the stream"
        );

        // Quick-open extraction must restore the same stream.
        let quick_out = dir.path().join("out_quick");
        {
            let mut quick = ArchiveReader::open_with(
                &archive,
                OpenOptions::new().scan_strategy(ScanStrategy::PreferQuickOpen),
            )
            .expect("open quick");
            assert_eq!(quick.entries().count(), 1);
            quick
                .extract_all_with_options(&quick_out, ExtractOptions::default())
                .expect("extract quick-open");
        }
        assert_eq!(
            std::fs::read(quick_out.join("owner.bin")).unwrap(),
            payload,
            "quick-open must extract the member data"
        );
        assert_eq!(
            std::fs::read(format!(
                "{}{}",
                quick_out.join("owner.bin").display(),
                ":ads"
            ))
            .unwrap(),
            stream,
            "quick-open must restore the stream"
        );
    }

    /// The multi-volume re-split rewrite must carry surviving "STM" records
    /// over to the rebuilt set: a delete of a stream-free member used to drop
    /// the surviving member's streams (then briefly refused the edit).
    #[test]
    fn multivolume_delete_keeps_ntfs_streams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("mkdir");
        let src = src_dir.join("owner.bin");
        let payload = vec![0x4Cu8; 100 * 1024];
        std::fs::write(&src, &payload).expect("write member");
        std::fs::write(format!("{}{}", src.display(), ":ads"), b"stream").expect("write stream");
        let plain = src_dir.join("plain.bin");
        std::fs::write(&plain, b"no streams here").expect("write plain member");

        let archive = dir.path().join("streams-edit.rar");
        {
            let mut rar = ArchiveWriter::create_with(
                &archive,
                WriterOptions::default()
                    .save_streams(true)
                    .volume_size(32 * 1024),
            )
            .expect("create");
            let stored =
                || EntryWriteOptions::new().compression_level(rar_rs::CompressionLevel::STORE);
            rar.add_path_as(&src, "owner.bin", stored())
                .expect("add owner");
            rar.add_path_as(&plain, "plain.bin", stored())
                .expect("add plain");
            rar.finish().expect("close");
        }
        let volumes = rar_rs::discover_volumes(&archive);
        assert!(volumes.len() > 1, "precondition: multi-volume set");

        let mut editor = rar_rs::ArchiveEditor::open(&volumes[0]).expect("editor");
        let id = editor.unique_entry("plain.bin").expect("member");
        editor.delete_entries(&[id]).expect("delete");
        drop(editor);

        // The stream survives with its owner and extraction restores it.
        let out = dir.path().join("out");
        let mut reader = ArchiveReader::open(&archive).expect("reopen");
        let names: Vec<String> = reader
            .entries()
            .map(|entry| entry.name().to_owned())
            .collect();
        assert_eq!(names, ["owner.bin"], "plain.bin is gone");
        reader
            .extract_all_with_options(&out, ExtractOptions::default())
            .expect("extract");
        assert_eq!(std::fs::read(out.join("owner.bin")).unwrap(), payload);
        assert_eq!(
            std::fs::read(format!("{}{}", out.join("owner.bin").display(), ":ads"))
                .expect("restored stream"),
            b"stream",
            "the surviving member's stream must be re-emitted"
        );
    }

    /// `-p` archives store each "STM" payload with its own ENCR record; the
    /// rewrite must re-encrypt surviving streams (and only those) so the set
    /// still decodes with the password.
    #[test]
    fn multivolume_delete_keeps_encrypted_ntfs_streams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("mkdir");
        let src = src_dir.join("owner.bin");
        let payload = vec![0x5Du8; 100 * 1024];
        std::fs::write(&src, &payload).expect("write member");
        std::fs::write(format!("{}{}", src.display(), ":ads"), b"secret stream")
            .expect("write stream");
        let plain = src_dir.join("plain.bin");
        std::fs::write(&plain, b"no streams here").expect("write plain member");

        let archive = dir.path().join("streams-enc.rar");
        {
            let mut rar = ArchiveWriter::create_with(
                &archive,
                WriterOptions::default()
                    .password("pw")
                    .save_streams(true)
                    .volume_size(32 * 1024),
            )
            .expect("create");
            let stored =
                || EntryWriteOptions::new().compression_level(rar_rs::CompressionLevel::STORE);
            rar.add_path_as(&src, "owner.bin", stored())
                .expect("add owner");
            rar.add_path_as(&plain, "plain.bin", stored())
                .expect("add plain");
            rar.finish().expect("close");
        }
        let volumes = rar_rs::discover_volumes(&archive);
        assert!(volumes.len() > 1, "precondition: multi-volume set");

        let mut editor =
            rar_rs::ArchiveEditor::open_with_password(&volumes[0], "pw").expect("editor");
        let id = editor.unique_entry("plain.bin").expect("member");
        editor.delete_entries(&[id]).expect("delete");
        drop(editor);

        let out = dir.path().join("out");
        let mut reader = ArchiveReader::open_with(&archive, OpenOptions::new().password("pw"))
            .expect("reopen with password");
        reader
            .extract_all_with_options(&out, ExtractOptions::default())
            .expect("extract");
        assert_eq!(std::fs::read(out.join("owner.bin")).unwrap(), payload);
        assert_eq!(
            std::fs::read(format!("{}{}", out.join("owner.bin").display(), ":ads"))
                .expect("restored stream"),
            b"secret stream",
            "the re-encrypted stream must decode with the archive password"
        );
    }
}

mod empty_member_integrity {
    //! Zero-size members must still pass stored CRC32/BLAKE2sp verification:
    //! a crafted empty member whose stored checksum was tampered with must be
    //! reported as corrupt by `read`, `copy`, `test` and extraction.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, RarError, WriterOptions,
    };

    fn opts(level: u8) -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::try_from(level).unwrap())
    }

    /// On-disk `[CRC32][size vint][body]` header of the file block named `name`,
    /// as `(block_start, header_bytes)`.
    fn locate_file_header(bytes: &[u8], name: &str) -> (usize, Vec<u8>) {
        let mut cursor = std::io::Cursor::new(bytes);
        cursor.set_position(8);
        while let Ok(Some(meta)) = rar_rs::wire::read_block(&mut cursor, None) {
            if meta.block_type == 0x02 && file_header_name(&meta.raw.header_data) == name {
                return (meta.block_start as usize, meta.header_bytes);
            }
            cursor.set_position(meta.data_end);
        }
        panic!("file block {name} not found");
    }

    /// Apply `patch` to the plaintext header body and repair the block header
    /// CRC, so the patched archive still opens.
    fn patch_header(bytes: &mut [u8], name: &str, patch: impl FnOnce(&mut [u8])) {
        let (start, mut header) = locate_file_header(bytes, name);
        let (_, body_start) = read_vint(&header, 4);
        patch(&mut header[body_start..]);
        let crc = crc32fast::hash(&header[4..]);
        header[..4].copy_from_slice(&crc.to_le_bytes());
        bytes[start..start + header.len()].copy_from_slice(&header);
    }

    /// Rewrite the stored CRC32 of `name` (header CRC repaired).
    fn set_stored_crc(bytes: &mut [u8], name: &str, crc: u32) {
        patch_header(bytes, name, |body| {
            let mut off = 0;
            let (_, n) = read_vint(body, off);
            off = n; // block type
            let (block_flags, n) = read_vint(body, off);
            off = n;
            if block_flags & 0x0001 != 0 {
                let (_, n) = read_vint(body, off);
                off = n; // extra area size
            }
            if block_flags & 0x0002 != 0 {
                let (_, n) = read_vint(body, off);
                off = n; // data area size
            }
            let (file_flags, n) = read_vint(body, off);
            off = n;
            let (_, n) = read_vint(body, off);
            off = n; // unpacked size
            let (_, n) = read_vint(body, off);
            off = n; // attributes
            if file_flags & 0x0002 != 0 {
                off += 4; // unix mtime
            }
            assert!(
                file_flags & 0x0004 != 0,
                "member {name} has no stored CRC32"
            );
            body[off..off + 4].copy_from_slice(&crc.to_le_bytes());
        });
    }

    /// Flip the first byte of the stored BLAKE2sp hash of `name` (header CRC
    /// repaired).
    fn tamper_stored_hash(bytes: &mut [u8], name: &str) {
        patch_header(bytes, name, |body| {
            let mut off = 0;
            let (_, n) = read_vint(body, off);
            off = n; // block type
            let (block_flags, n) = read_vint(body, off);
            off = n;
            let mut extra_size = 0usize;
            if block_flags & 0x0001 != 0 {
                let (value, _) = read_vint(body, off);
                extra_size = value as usize;
            }
            let mut q = body.len() - extra_size;
            while q < body.len() {
                let (rec_size, n) = read_vint(body, q);
                let rec_end = n + rec_size as usize;
                let (rec_type, n) = read_vint(body, n);
                if rec_type == 0x02 {
                    let (_, n) = read_vint(body, n);
                    body[n] ^= 0xFF; // hash type, then the 32-byte value
                    return;
                }
                q = rec_end;
            }
            panic!("member {name} has no hash record");
        });
    }

    #[test]
    fn valid_empty_member_passes_verification() {
        let dir = make_temp_dir();
        let path = dir.path().join("valid-empty.rar");
        {
            let mut writer = ArchiveWriter::create(&path).unwrap();
            writer.add_bytes("empty.bin", b"", opts(0)).unwrap();
            writer.finish().unwrap();
        }

        let mut reader = ArchiveReader::open(&path).unwrap();
        let id = reader.unique_entry("empty.bin").unwrap();
        assert!(reader.read_entry(id).unwrap().is_empty());

        let out = dir.path().join("out");
        let extracted = reader.extract_entry(id, &out).unwrap();
        assert_eq!(std::fs::metadata(&extracted).unwrap().len(), 0);

        drop(reader);
        let mut archive = ArchiveReader::open(&path).unwrap();
        let report = archive.verify().unwrap();
        assert_eq!(
            (report.passed() + report.failed(), report.failed()),
            (1, 0),
            "a valid empty member must pass verification"
        );
    }

    #[test]
    fn empty_member_crc_mismatch_fails_read_test_and_extract() {
        let dir = make_temp_dir();
        let path = dir.path().join("bad-crc.rar");
        {
            let mut writer = ArchiveWriter::create(&path).unwrap();
            writer.add_bytes("empty.bin", b"", opts(0)).unwrap();
            writer.finish().unwrap();
        }

        let mut bytes = std::fs::read(&path).unwrap();
        set_stored_crc(&mut bytes, "empty.bin", 0xDEAD_BEEF);
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = ArchiveReader::open(&path).unwrap();
        let id = reader.unique_entry("empty.bin").unwrap();
        let err = reader.read_entry(id).unwrap_err();
        assert!(matches!(err, RarError::Crc { .. }), "read: {err}");

        let mut copy = Vec::new();
        let err = reader.copy_entry_to(id, &mut copy).unwrap_err();
        assert!(matches!(err, RarError::Crc { .. }), "copy: {err}");

        let out = dir.path().join("out");
        let err = reader.extract_entry(id, &out).unwrap_err();
        assert!(matches!(err, RarError::Crc { .. }), "extract: {err}");
        assert!(
            !out.join("empty.bin").exists(),
            "a failed extraction must not leave output behind"
        );

        drop(reader);
        let mut archive = ArchiveReader::open(&path).unwrap();
        let report = archive.verify().unwrap();
        assert_eq!(
            (report.passed() + report.failed(), report.failed()),
            (1, 1),
            "test must report the tampered empty member"
        );
    }

    /// The parallel whole-archive path must verify empty members too: its decode
    /// phase skips `decode_member` for a zero-size payload but must still run the
    /// integrity check, or a crafted zero-size header slips through.
    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_extraction_rejects_tampered_empty_member() {
        const BIG: usize = 17 * 1024 * 1024;
        let dir = make_temp_dir();
        let path = dir.path().join("parallel-empty.rar");
        {
            let mut writer = ArchiveWriter::create(&path).unwrap();
            for i in 0..4 {
                writer
                    .add_bytes(&format!("big{i}.bin"), &vec![0x5A; BIG], opts(0))
                    .unwrap();
            }
            writer.add_bytes("empty.bin", b"", opts(0)).unwrap();
            writer.finish().unwrap();
        }

        let mut bytes = std::fs::read(&path).unwrap();
        set_stored_crc(&mut bytes, "empty.bin", 0xDEAD_BEEF);
        std::fs::write(&path, &bytes).unwrap();

        let out = dir.path().join("out");
        let mut reader = ArchiveReader::open(&path).unwrap();
        let err = reader
            .extract_all_with_options(&out, rar_rs::ExtractOptions::default())
            .unwrap_err();
        assert!(matches!(err, RarError::Crc { .. }), "parallel: {err}");
        assert!(
            !out.join("empty.bin").exists(),
            "the tampered empty member must not land"
        );
        assert!(
            out.join("big0.bin").exists(),
            "members before the failure land"
        );
    }

    #[test]
    fn empty_member_hash_mismatch_fails_read_and_test() {
        let dir = make_temp_dir();
        let path = dir.path().join("bad-hash.rar");
        {
            let mut writer =
                ArchiveWriter::create_with(&path, WriterOptions::default().blake2(true)).unwrap();
            writer.add_bytes("empty.bin", b"", opts(0)).unwrap();
            writer.finish().unwrap();
        }

        // Positive control: the stored hash is the fixed BLAKE2sp of empty.
        let mut reader = ArchiveReader::open(&path).unwrap();
        let id = reader.unique_entry("empty.bin").unwrap();
        assert!(reader.read_entry(id).unwrap().is_empty());
        drop(reader);

        let mut bytes = std::fs::read(&path).unwrap();
        tamper_stored_hash(&mut bytes, "empty.bin");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = ArchiveReader::open(&path).unwrap();
        let id = reader.unique_entry("empty.bin").unwrap();
        let err = reader.read_entry(id).unwrap_err();
        assert!(matches!(err, RarError::HashMismatch { .. }), "read: {err}");

        drop(reader);
        let mut archive = ArchiveReader::open(&path).unwrap();
        let report = archive.verify().unwrap();
        assert_eq!(
            (report.passed() + report.failed(), report.failed()),
            (1, 1),
            "test must report the tampered empty-member hash"
        );
    }
}

mod extract_attributes {
    //! Extracted members must restore their stored attributes: Unix permission
    //! bits (`chmod`) for Unix-host members, DOS attributes
    //! (`SetFileAttributesW`) for Windows-host members. Both paths are the same
    //! shared destination code, so STORE and compressed members behave alike.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    #[cfg(unix)]
    mod unix {
        use super::*;
        use rar_rs::{ArchiveReader, ArchiveWriter, EntryWriteOptions};
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &std::path::Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
        }

        /// Whether the effective uid of this test process is 0. The production
        /// code uses `libc::geteuid` for the same test (UnRAR's `geteuid() != 0`
        /// rule); the integration test asks libc directly so the expectation and
        /// the implementation cannot drift apart.
        fn running_as_root() -> bool {
            // SAFETY: `geteuid` takes no arguments, has no side effects and
            // cannot fail.
            unsafe { libc::geteuid() == 0 }
        }

        /// The set-ID bits a hostile archive would try to install: `S_ISUID`
        /// and `S_ISGID` may together clear or set them; `0o6000` is the pair.
        const SET_ID_BITS: u32 = 0o6000;

        /// Whether this filesystem keeps set-ID bits at all. Overlay and some
        /// network filesystems silently drop them on `chmod`, which would make
        /// the round-trip assertion fail for an environment reason rather than
        /// a code one; such environments skip the set-ID cases.
        fn keeps_set_id_bits(path: &std::path::Path, mode: u32) -> bool {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            mode_of(path) & SET_ID_BITS == mode & SET_ID_BITS
        }

        #[test]
        fn executable_mode_round_trips() {
            let dir = make_temp_dir();
            let src = dir.path().join("script.sh");
            std::fs::write(&src, b"#!/bin/sh\necho hi\n").unwrap();
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();

            let archive = dir.path().join("mode.rar");
            let mut writer = ArchiveWriter::create(&archive).unwrap();
            writer.add_path(&src, EntryWriteOptions::new()).unwrap();
            writer.finish().unwrap();

            let out = dir.path().join("out");
            let mut reader = ArchiveReader::open(&archive).unwrap();
            reader.extract_all(&out).unwrap();

            assert_eq!(
                mode_of(&out.join("script.sh")),
                0o755,
                "the stored executable mode must be restored"
            );
        }

        #[test]
        fn directory_mode_round_trips() {
            let dir = make_temp_dir();
            let src = dir.path().join("sub");
            std::fs::create_dir(&src).unwrap();
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o750)).unwrap();

            let archive = dir.path().join("dirmode.rar");
            let mut writer = ArchiveWriter::create(&archive).unwrap();
            writer.add_directory(&src, "sub").unwrap();
            writer.finish().unwrap();

            let out = dir.path().join("out");
            let mut reader = ArchiveReader::open(&archive).unwrap();
            reader.extract_all(&out).unwrap();

            assert_eq!(
                mode_of(&out.join("sub")),
                0o750,
                "the stored directory mode must be restored"
            );
        }

        /// A non-root extractor must not install a setuid/setgid executable out
        /// of an untrusted archive; a root extractor keeps the stored mode.
        /// This mirrors official UnRAR, which strips `S_ISUID|S_ISGID` from
        /// ordinary files exactly when `geteuid() != 0`.
        #[test]
        fn file_set_id_bits_follow_the_unrar_root_rule() {
            let dir = make_temp_dir();
            let src = dir.path().join("tool");
            std::fs::write(&src, b"\x7fELF\x02\x01\x01").unwrap();
            if !keeps_set_id_bits(&src, 0o6755) {
                eprintln!("skipping: this filesystem does not keep set-ID bits");
                return;
            }

            let archive = dir.path().join("setid.rar");
            let mut writer = ArchiveWriter::create(&archive).unwrap();
            writer.add_path(&src, EntryWriteOptions::new()).unwrap();
            writer.finish().unwrap();

            let out = dir.path().join("out");
            let mut reader = ArchiveReader::open(&archive).unwrap();
            reader.extract_all(&out).unwrap();

            let extracted = mode_of(&out.join("tool"));
            let expected = if running_as_root() { 0o6755 } else { 0o755 };
            assert_eq!(
                extracted, expected,
                "set-ID file bits must be kept for root and stripped for a \
             standard user (official UnRAR's rule)"
            );
            assert_eq!(
                extracted & 0o111,
                0o111,
                "the ordinary permission bits must survive either way"
            );
        }

        /// Directories are not covered by UnRAR's strip rule: the set-group-ID
        /// bit there only selects the group of newly created entries and confers
        /// no privilege, so it round-trips for every extractor.
        #[test]
        fn directory_setgid_is_preserved() {
            let dir = make_temp_dir();
            let src = dir.path().join("shared");
            std::fs::create_dir(&src).unwrap();
            if !keeps_set_id_bits(&src, 0o2755) {
                eprintln!("skipping: this filesystem does not keep set-ID bits");
                return;
            }

            let archive = dir.path().join("dirsetgid.rar");
            let mut writer = ArchiveWriter::create(&archive).unwrap();
            writer.add_directory(&src, "shared").unwrap();
            writer.finish().unwrap();

            let out = dir.path().join("out");
            let mut reader = ArchiveReader::open(&archive).unwrap();
            reader.extract_all(&out).unwrap();

            assert_eq!(
                mode_of(&out.join("shared")),
                0o2755,
                "a directory's stored set-group-ID bit is restored like UnRAR"
            );
        }

        /// The sticky bit is not a set-ID bit and must survive extraction (it is
        /// the ordinary mode bit for `/tmp`-style directories).
        #[test]
        fn directory_sticky_bit_is_preserved() {
            let dir = make_temp_dir();
            let src = dir.path().join("scratch");
            std::fs::create_dir(&src).unwrap();
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o1755)).unwrap();
            if mode_of(&src) & 0o1000 == 0 {
                eprintln!("skipping: this filesystem does not keep the sticky bit");
                return;
            }

            let archive = dir.path().join("dirsticky.rar");
            let mut writer = ArchiveWriter::create(&archive).unwrap();
            writer.add_directory(&src, "scratch").unwrap();
            writer.finish().unwrap();

            let out = dir.path().join("out");
            let mut reader = ArchiveReader::open(&archive).unwrap();
            reader.extract_all(&out).unwrap();

            assert_eq!(
                mode_of(&out.join("scratch")),
                0o1755,
                "the sticky bit is not a set-ID bit and must be restored"
            );
        }
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use rar_rs::{ArchiveReader, ArchiveWriter, EntryWriteOptions};

        /// Rewrite the `attributes` and `host_os` vints of `name`'s file header
        /// in place (same-width vints) and repair the block header CRC. `0x4021`
        /// has the DOS read-only (0x1) and archive (0x20) bits set; host 0 is
        /// Windows.
        fn mark_read_only(bytes: &mut [u8], name: &str) {
            let mut cursor = std::io::Cursor::new(&bytes[..]);
            cursor.set_position(8);
            while let Ok(Some(meta)) = rar_rs::wire::read_block(&mut cursor, None) {
                if meta.block_type == 0x02
                    && support::file_header_name(&meta.raw.header_data) == name
                {
                    let mut header = meta.header_bytes.clone();
                    let (_, body_start) = support::read_vint(&header, 4); // size vint
                    let (_, n) = support::read_vint(&header, body_start); // block type
                    let mut off = n;
                    let (block_flags, n) = support::read_vint(&header, off);
                    off = n;
                    if block_flags & 0x0001 != 0 {
                        let (_, n) = support::read_vint(&header, off);
                        off = n;
                    }
                    if block_flags & 0x0002 != 0 {
                        let (_, n) = support::read_vint(&header, off);
                        off = n;
                    }
                    let (file_flags, n) = support::read_vint(&header, off);
                    off = n;
                    let (_, n) = support::read_vint(&header, off); // unpacked size
                    off = n;
                    let (_, after) = support::read_vint(&header, off); // attributes
                    let attrs = rar_rs::wire::vint::encode(0x4021);
                    assert_eq!(attrs.len(), after - off, "patch must keep the vint width");
                    header[off..after].copy_from_slice(&attrs);
                    off = after;
                    if file_flags & 0x0002 != 0 {
                        off += 4; // Unix mtime
                    }
                    if file_flags & 0x0004 != 0 {
                        off += 4; // CRC32
                    }
                    let (_, n) = support::read_vint(&header, off); // compression info
                    off = n;
                    let host_start = off;
                    let (_, after) = support::read_vint(&header, off); // host OS
                    assert_eq!(
                        after - host_start,
                        1,
                        "the writer emits a one-byte OS_UNIX vint"
                    );
                    header[host_start] = 0; // OS_WINDOWS
                    let crc = crc32fast::hash(&header[4..]);
                    header[..4].copy_from_slice(&crc.to_le_bytes());
                    let start = meta.block_start as usize;
                    bytes[start..start + header.len()].copy_from_slice(&header);
                    return;
                }
                cursor.set_position(meta.data_end);
            }
            panic!("member {name} not found");
        }

        #[test]
        // Clearing the read-only bit is only for temp-dir cleanup on Windows,
        // where this test runs; Unix permissions are set via `PermissionsExt`.
        #[allow(clippy::permissions_set_readonly_false)]
        fn read_only_attribute_round_trips() {
            let dir = make_temp_dir();
            let archive = dir.path().join("attrs.rar");
            {
                let mut writer = ArchiveWriter::create(&archive).unwrap();
                writer
                    .add_bytes("readonly.bin", b"content", EntryWriteOptions::new())
                    .unwrap();
                writer.finish().unwrap();
            }

            let mut bytes = std::fs::read(&archive).unwrap();
            mark_read_only(&mut bytes, "readonly.bin");
            std::fs::write(&archive, &bytes).unwrap();

            let out = dir.path().join("out");
            let mut reader = ArchiveReader::open(&archive).unwrap();
            reader.extract_all(&out).unwrap();

            let extracted = out.join("readonly.bin");
            assert!(
                std::fs::metadata(&extracted)
                    .unwrap()
                    .permissions()
                    .readonly(),
                "FILE_ATTRIBUTE_READONLY must be restored"
            );

            // Clear the bit so the temp directory can be removed on drop.
            let mut perms = std::fs::metadata(&extracted).unwrap().permissions();
            perms.set_readonly(false);
            std::fs::set_permissions(&extracted, perms).unwrap();
        }
    }
}

mod model_api_compat {
    //! Wire model API: the promoted `wire` surface exposes the archive model
    //! structs together with their parsing/serialization helpers (the former
    //! `rar40`/`rar50` alias paths were retired with the `raw` feature, ADR 0007).

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::wire::{DataChunk, FileHeader, RawBlock};

    #[test]
    fn wire_model_structs_expose_their_serialization_helpers() {
        let header = FileHeader::default();
        assert!(!header.to_bytes().is_empty());

        let _: fn(&RawBlock, u64) -> rar_rs::RarResult<FileHeader> = FileHeader::from_raw;

        let chunk = DataChunk {
            volume_index: 2,
            data_offset: 17,
            packed_size: 23,
            crc32_val: Some(42),
            is_final: true,
            extra_data: vec![1, 2, 3],
        };
        assert_eq!(chunk.volume_index, 2);
        assert_eq!(chunk.data_offset, 17);
        assert_eq!(chunk.packed_size, 23);
    }
}
