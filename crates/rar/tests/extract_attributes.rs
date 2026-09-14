//! Extracted members must restore their stored attributes: Unix permission
//! bits (`chmod`) for Unix-host members, DOS attributes
//! (`SetFileAttributesW`) for Windows-host members. Both paths are the same
//! shared destination code, so STORE and compressed members behave alike.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use rar_rs::{ArchiveReader, ArchiveWriter, EntryWriteOptions};
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
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
            if meta.block_type == 0x02 && support::file_header_name(&meta.raw.header_data) == name {
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
