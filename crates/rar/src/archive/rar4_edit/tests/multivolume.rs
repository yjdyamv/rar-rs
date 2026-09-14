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
