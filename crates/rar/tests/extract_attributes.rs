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
