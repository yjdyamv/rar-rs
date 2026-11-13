//! Multi-volume RAR4 header edits must honour the archive's own header
//! encryption: on a *plain* set the edit password is irrelevant and the
//! rebuilt headers must stay plaintext (a latched `hp` of `None`). The
//! multi-volume rewrite used to encrypt every rebuilt header whenever a
//! password was configured, leaving a plain set whose headers were
//! unreadable — and it replaced the originals before noticing.

use crate::archive::RarArchive;
use crate::archive::discover_volumes;
use crate::archive::editor::{ArchiveEditor, EditPlan};
use crate::format::rar4::MHD_PASSWORD;
use crate::version::ArchiveVersion;

fn patterned(len: usize, modulo: usize) -> Vec<u8> {
    (0..len).map(|index| (index % modulo) as u8).collect()
}

/// Build a plain (`-p`-less, `-hp`-less) v29 volume set with two stored
/// members big enough to split across volumes.
fn build_set(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let base = dir.join("plain-mv.rar");
    let mut archive = RarArchive::create_with_options(
        &base,
        crate::options::CreateOptions {
            compression: ArchiveVersion::V29,
            volume_size: Some(64 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    archive
        .add_bytes("a.bin", &patterned(150_000, 251), 0)
        .unwrap();
    archive
        .add_bytes("b.bin", &patterned(120_000, 253), 0)
        .unwrap();
    archive.close().unwrap();
    let volumes = discover_volumes(&base);
    assert!(
        volumes.len() > 1,
        "precondition: multi-volume set: {volumes:?}"
    );
    volumes
}

/// Incompressible bytes with a deterministic stream, so a compressed member
/// really has to span volumes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// Entry options for [`crate::archive::ArchiveWriter::add_bytes`].
fn entry_options(level: u8) -> crate::EntryWriteOptions {
    crate::EntryWriteOptions::new()
        .compression_level(crate::CompressionLevel::try_from(level).unwrap())
}

fn main_flags(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[10], bytes[11]])
}

fn snapshot(volumes: &[std::path::PathBuf]) -> Vec<Vec<u8>> {
    volumes
        .iter()
        .map(|volume| std::fs::read(volume).unwrap())
        .collect()
}

/// Renaming (and commenting) a plain multi-volume set with an irrelevant
/// password must never encrypt the rebuilt headers: either the edit is
/// refused and the set stays byte-identical, or it succeeds and the set is
/// still readable without any password. Before the fix `emit_block` was
/// handed the password instead of the latched per-volume `hp`, so the
/// committed set failed its own CRC re-scan.
#[test]
fn plain_multivolume_edit_with_irrelevant_password_never_corrupts() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = build_set(dir.path());
    let before = snapshot(&volumes);

    let mut editor = ArchiveEditor::open_with_password(&volumes[0], "irrelevant-pw").unwrap();
    let a = editor.unique_entry("a.bin").unwrap();
    let result = editor.apply(
        EditPlan::new()
            .rename(a, "renamed.bin")
            .set_comment(b"edited with an irrelevant password".to_vec()),
    );
    drop(editor);

    if result.is_err() {
        assert_eq!(
            snapshot(&volumes),
            before,
            "a refused edit must leave every volume byte-identical"
        );
        return;
    }
    assert_eq!(result.unwrap().renamed(), 1);

    // The set stayed plain: no volume's main header gained MHD_PASSWORD.
    let after_volumes = discover_volumes(&volumes[0]);
    assert!(
        after_volumes.len() > 1,
        "the edited set is still multi-volume: {after_volumes:?}"
    );
    for volume in &after_volumes {
        let bytes = std::fs::read(volume).unwrap();
        assert_eq!(
            main_flags(&bytes) & MHD_PASSWORD,
            0,
            "{} must not become header-encrypted",
            volume.display()
        );
    }

    // Readable without a password, comment included, and both members still
    // extract across the volume boundaries.
    let mut archive = RarArchive::open(&after_volumes[0]).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["renamed.bin", "b.bin"]
    );
    assert_eq!(
        archive
            .read_with_options("renamed.bin", Default::default())
            .unwrap(),
        patterned(150_000, 251)
    );
    assert_eq!(
        archive
            .read_with_options("b.bin", Default::default())
            .unwrap(),
        patterned(120_000, 253)
    );
    assert_eq!(
        archive.get_comment().unwrap(),
        Some(b"edited with an irrelevant password".to_vec())
    );
}

/// Deleting a member from a multi-volume set repacks the whole set: the
/// surviving members must extract, the new set must keep the volume ceiling,
/// and no stale volume may be left behind.
#[test]
fn multivolume_delete_repacks_the_set() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = build_set(dir.path());
    let limit = volumes
        .iter()
        .map(|volume| std::fs::metadata(volume).unwrap().len())
        .max()
        .unwrap();
    let first = volumes[0].clone();

    let mut editor = ArchiveEditor::open(&first).unwrap();
    let b = editor.unique_entry("b.bin").unwrap();
    let report = editor.delete_entries(&[b]).unwrap();
    assert_eq!(report, 1);
    drop(editor);

    let after = discover_volumes(&first);
    assert!(!after.is_empty());
    for volume in &after {
        let len = std::fs::metadata(volume).unwrap().len();
        assert!(
            len <= limit.max(1),
            "{} grew past the original volume ceiling ({len} > {limit})",
            volume.display()
        );
    }
    let mut archive = RarArchive::open(&after[0]).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin"]
    );
    assert_eq!(
        archive
            .read_with_options("a.bin", Default::default())
            .unwrap(),
        patterned(150_000, 251)
    );
    // The set must still be a volume set, not a silently truncated part 1.
    assert!(
        archive.volume_paths.len() > 1,
        "the repacked set lost its volumes: {:?}",
        archive.volume_paths
    );
}

/// Compressed (non-solid) members decode through the regular reader during
/// the repack; the survivors keep their payloads byte for byte.
#[test]
fn multivolume_delete_recompresses_compressed_members() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("c-mv.rar");
    let p1 = noise(150_000, 0xA1);
    let p2 = noise(120_000, 0xB2);
    let p3 = noise(90_000, 0xC3);
    {
        let mut archive = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                compression: ArchiveVersion::V29,
                volume_size: Some(64 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        archive.add_bytes("a.bin", &p1, 3).unwrap();
        archive.add_bytes("b.bin", &p2, 3).unwrap();
        archive.add_bytes("c.bin", &p3, 3).unwrap();
        archive.close().unwrap();
    }
    let volumes = discover_volumes(&base);
    assert!(volumes.len() > 1);
    let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
    let b = editor.unique_entry("b.bin").unwrap();
    assert_eq!(editor.delete_entries(&[b]).unwrap(), 1);
    drop(editor);

    let after = discover_volumes(&base);
    let mut archive = RarArchive::open(&after[0]).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "c.bin"]
    );
    assert_eq!(
        archive
            .read_with_options("a.bin", Default::default())
            .unwrap(),
        p1
    );
    assert_eq!(
        archive
            .read_with_options("c.bin", Default::default())
            .unwrap(),
        p3
    );
}

/// Solid chains span volumes: the repack decodes the chain through the set.
#[test]
fn multivolume_solid_delete_repacks_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("s-mv.rar");
    let p1 = noise(150_000, 0xD1);
    let p2 = noise(120_000, 0xE2);
    let p3 = noise(90_000, 0xF3);
    {
        let mut archive = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                compression: ArchiveVersion::V29,
                solid: true,
                volume_size: Some(32 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        archive.add_bytes("a.bin", &p1, 1).unwrap();
        archive.add_bytes("b.bin", &p2, 1).unwrap();
        archive.add_bytes("c.bin", &p3, 1).unwrap();
        archive.close().unwrap();
    }
    let volumes = discover_volumes(&base);
    assert!(volumes.len() > 1, "precondition: {volumes:?}");
    let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
    let b = editor.unique_entry("b.bin").unwrap();
    assert_eq!(editor.delete_entries(&[b]).unwrap(), 1);
    drop(editor);

    let after = discover_volumes(&base);
    let mut archive = RarArchive::open(&after[0]).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "c.bin"]
    );
    assert_eq!(
        archive
            .read_with_options("a.bin", Default::default())
            .unwrap(),
        p1
    );
    assert_eq!(
        archive
            .read_with_options("c.bin", Default::default())
            .unwrap(),
        p3
    );
}

/// A delete composes with the archive comment and the per-volume recovery
/// records: both survive the whole-set repack.
#[test]
fn multivolume_delete_keeps_comment_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("rr-mv.rar");
    {
        let mut archive = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                compression: ArchiveVersion::V29,
                volume_size: Some(32 * 1024),
                recovery_percent: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        archive.set_rar4_writer_comment(Some(b"set comment".to_vec()));
        archive
            .add_bytes("a.bin", &patterned(120_000, 251), 0)
            .unwrap();
        archive
            .add_bytes("b.bin", &patterned(100_000, 253), 0)
            .unwrap();
        archive.close().unwrap();
    }
    let volumes = discover_volumes(&base);
    assert!(volumes.len() > 1);
    let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
    let b = editor.unique_entry("b.bin").unwrap();
    assert_eq!(editor.delete_entries(&[b]).unwrap(), 1);
    drop(editor);

    let after = discover_volumes(&base);
    let mut archive = RarArchive::open(&after[0]).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin"]
    );
    assert_eq!(
        archive.get_comment().unwrap(),
        Some(b"set comment".to_vec())
    );
    assert_eq!(
        archive
            .read_with_options("a.bin", Default::default())
            .unwrap(),
        patterned(120_000, 251)
    );
    for volume in &after {
        let protect = crate::recovery::legacy_rr::scan_protect_file(volume, None)
            .unwrap()
            .protect;
        assert!(
            protect.is_some(),
            "{} lost its recovery record",
            volume.display()
        );
    }
}

/// Appending to a stored set repacks the whole set: the new member and every
/// old member survive, and no volume exceeds the original ceiling.
#[test]
fn multivolume_append_stored_members() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = build_set(dir.path());
    let first = volumes[0].clone();
    let base = dir.path().join("plain-mv.rar");
    let limit = volumes
        .iter()
        .map(|volume| std::fs::metadata(volume).unwrap().len())
        .max()
        .unwrap();
    let added = patterned(200_000, 241);
    {
        let mut writer = crate::archive::ArchiveWriter::append(&first).unwrap();
        writer.add_bytes("c.bin", &added, entry_options(0)).unwrap();
        writer.finish().unwrap();
    }
    let after = discover_volumes(&base);
    assert!(after.len() > 1, "the appended set lost its volumes");
    for volume in &after {
        assert!(
            std::fs::metadata(volume).unwrap().len() <= limit.max(1),
            "{} grew past the original volume ceiling",
            volume.display()
        );
    }
    let mut archive = RarArchive::open(&after[0]).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "b.bin", "c.bin"]
    );
    assert_eq!(
        archive
            .read_with_options("a.bin", Default::default())
            .unwrap(),
        patterned(150_000, 251)
    );
    assert_eq!(
        archive
            .read_with_options("c.bin", Default::default())
            .unwrap(),
        added
    );
}

/// Compressed (non-solid) members decode through the regular reader during an
/// append repack; old and new payloads stay byte-identical.
#[test]
fn multivolume_append_recompresses_compressed_members() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("c-append.rar");
    let p1 = noise(150_000, 0xB1);
    let p2 = noise(120_000, 0xC2);
    let added = noise(90_000, 0xD3);
    {
        let mut archive = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                compression: ArchiveVersion::V29,
                volume_size: Some(64 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        archive.add_bytes("a.bin", &p1, 3).unwrap();
        archive.add_bytes("b.bin", &p2, 3).unwrap();
        archive.close().unwrap();
    }
    let first = discover_volumes(&base)[0].clone();
    {
        let mut writer = crate::archive::ArchiveWriter::append(&first).unwrap();
        writer.add_bytes("c.bin", &added, entry_options(3)).unwrap();
        writer.finish().unwrap();
    }
    let after = discover_volumes(&base);
    let mut archive = RarArchive::open(&after[0]).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "b.bin", "c.bin"]
    );
    for (name, expected) in [("a.bin", &p1), ("b.bin", &p2), ("c.bin", &added)] {
        assert_eq!(
            &archive.read_with_options(name, Default::default()).unwrap(),
            expected,
            "{name}"
        );
    }
}

/// A solid chain spans volumes, so an append decodes and re-encodes the whole
/// chain; every member must still read back.
#[test]
fn multivolume_solid_append_repacks_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("s-append.rar");
    let p1 = noise(150_000, 0xE1);
    let p2 = noise(120_000, 0xF2);
    let added = noise(90_000, 0x13);
    {
        let mut archive = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                compression: ArchiveVersion::V29,
                solid: true,
                volume_size: Some(32 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        archive.add_bytes("a.bin", &p1, 1).unwrap();
        archive.add_bytes("b.bin", &p2, 1).unwrap();
        archive.close().unwrap();
    }
    let first = discover_volumes(&base)[0].clone();
    {
        let mut writer = crate::archive::ArchiveWriter::append(&first).unwrap();
        writer.add_bytes("c.bin", &added, entry_options(1)).unwrap();
        writer.finish().unwrap();
    }
    let after = discover_volumes(&base);
    let mut archive = RarArchive::open(&after[0]).unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "b.bin", "c.bin"]
    );
    for (name, expected) in [("a.bin", &p1), ("b.bin", &p2), ("c.bin", &added)] {
        assert_eq!(
            &archive.read_with_options(name, Default::default()).unwrap(),
            expected,
            "{name}"
        );
    }
}

/// Header-encrypted sets append with the set password; the rebuilt volumes
/// stay `-hp` and every member reads back with the password.
#[test]
fn multivolume_append_keeps_header_encryption() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("hp-append.rar");
    let p1 = patterned(120_000, 251);
    let p2 = patterned(100_000, 253);
    let added = patterned(90_000, 249);
    {
        let mut archive = RarArchive::create_with_options(
            &base,
            crate::options::CreateOptions {
                compression: ArchiveVersion::V29,
                volume_size: Some(32 * 1024),
                password: Some("secret".into()),
                encrypt_headers: true,
                ..Default::default()
            },
        )
        .unwrap();
        archive.add_bytes("a.bin", &p1, 0).unwrap();
        archive.add_bytes("b.bin", &p2, 0).unwrap();
        archive.close().unwrap();
    }
    let first = discover_volumes(&base)[0].clone();
    {
        let mut writer = crate::archive::ArchiveWriter::append_with(
            &first,
            crate::AppendOptions::new().password("secret"),
        )
        .unwrap();
        writer.add_bytes("c.bin", &added, entry_options(0)).unwrap();
        writer.finish().unwrap();
    }
    let after = discover_volumes(&base);
    let mut archive = RarArchive::open_with_password(&after[0], "secret").unwrap();
    assert_eq!(
        archive.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        ["a.bin", "b.bin", "c.bin"]
    );
    for (name, expected) in [("a.bin", &p1), ("b.bin", &p2), ("c.bin", &added)] {
        assert_eq!(
            &archive.read_with_options(name, Default::default()).unwrap(),
            expected,
            "{name}"
        );
    }
}
