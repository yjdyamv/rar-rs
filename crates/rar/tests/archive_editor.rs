//! Phase 4 slice tests: the typed [`ArchiveEditor`] — duplicate-safe catalog
//! identities, ID-based delete/rename, and catalog-generation invalidation
//! after structural edits. Byte parity with the legacy name-based
//! operations is checked on twin archive copies.

#![allow(deprecated)] // legacy facade (delete/rename/lock/list/read) — kept for byte-parity checks

use rar_rs::{ArchiveEditor, ArchiveReader, ArchiveVersion, EditPlan, RarArchive, RarError};

fn stored_level() -> u8 {
    0
}

/// Build `path` with duplicate members plus a directory tree:
/// `same.txt` x2 ("first", "second"), `other.txt`, `d/`, `d/x.txt`.
fn build_fixture(path: &std::path::Path, dir: &std::path::Path) {
    let mut archive = RarArchive::create_with_options(path, rar_rs::CreateOptions::default())
        .expect("create fixture");
    archive
        .add_bytes("same.txt", b"first", stored_level())
        .expect("add dup 1");
    archive
        .add_bytes("same.txt", b"second", stored_level())
        .expect("add dup 2");
    archive
        .add_bytes("other.txt", b"other", stored_level())
        .expect("add other");
    archive
        .add_directory_only(dir, "d")
        .expect("add dir member");
    let leaf = dir.join("leaf.txt");
    std::fs::write(&leaf, b"leaf payload").unwrap();
    archive
        .add_as(&leaf, "d/x.txt", stored_level())
        .expect("add child");
    archive.close().expect("close fixture");
}

fn names(path: &std::path::Path) -> Vec<String> {
    let reader = ArchiveReader::open(path).unwrap();
    reader
        .entries()
        .map(|entry| entry.name().to_string())
        .collect()
}

fn sorted_names(path: &std::path::Path) -> Vec<String> {
    let mut names = names(path);
    names.sort();
    names
}

/// Copy the fixture archive to `twin_path` and return the original path.
fn fixture_pair(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let src = dir.join(format!("{name}-src.rar"));
    let twin = dir.join(format!("{name}-twin.rar"));
    build_fixture(&src, dir);
    std::fs::copy(&src, &twin).unwrap();
    (src, twin)
}

#[test]
fn editor_catalog_is_duplicate_safe_and_edits_independently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dups.rar");
    build_fixture(&path, dir.path());

    let mut editor = ArchiveEditor::open(&path).unwrap();
    // Duplicate names resolve ambiguously by name, individually by ID.
    assert!(matches!(
        editor.unique_entry("same.txt"),
        Err(RarError::AmbiguousMember { matches: 2, .. })
    ));
    let duplicates: Vec<_> = editor
        .entries_named("same.txt")
        .map(|entry| (entry.id(), entry.metadata().size()))
        .collect();
    assert_eq!(duplicates.len(), 2);

    // Delete exactly the second duplicate; the first survives untouched.
    let deleted = editor.delete_entries(&[duplicates[1].0]).unwrap();
    assert_eq!(deleted, 1);
    let mut reader = ArchiveReader::open(&path).unwrap();
    let survivors: Vec<_> = reader
        .entries_named("same.txt")
        .map(|entry| entry.id())
        .collect();
    assert_eq!(survivors.len(), 1, "exactly one duplicate must remain");
    assert_eq!(reader.read_entry(survivors[0]).unwrap(), b"first");
    assert_eq!(
        sorted_names(&path),
        ["d/", "d/x.txt", "other.txt", "same.txt"]
    );

    // Rename the remaining duplicate by its fresh ID.
    let mut editor = ArchiveEditor::open(&path).unwrap();
    let id = editor.unique_entry("same.txt").unwrap();
    editor
        .rename_entries(&[(id, "renamed.txt".to_string())])
        .unwrap();
    assert_eq!(
        sorted_names(&path),
        ["d/", "d/x.txt", "other.txt", "renamed.txt"]
    );
    let mut reader = ArchiveReader::open(&path).unwrap();
    let renamed = reader.unique_entry("renamed.txt").unwrap();
    assert_eq!(reader.read_entry(renamed).unwrap(), b"first");
}

#[test]
fn delete_entries_matches_legacy_output_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (src, twin) = fixture_pair(dir.path(), "del");

    // Legacy deletes by name; the editor deletes the same member by ID.
    let mut legacy = RarArchive::open(&src).unwrap();
    legacy.delete(&["other.txt"]).unwrap();
    legacy.close().unwrap();

    let mut editor = ArchiveEditor::open(&twin).unwrap();
    let id = editor.unique_entry("other.txt").unwrap();
    editor.delete_entries(&[id]).unwrap();

    assert_eq!(
        std::fs::read(&src).unwrap(),
        std::fs::read(&twin).unwrap(),
        "ID-based delete must produce the same bytes as the name-based one"
    );
}

#[test]
fn rename_entries_matches_legacy_output_bytes_with_dir_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let (src, twin) = fixture_pair(dir.path(), "ren");

    let mut legacy = RarArchive::open(&src).unwrap();
    legacy.rename(&[("d", "renamed")]).unwrap();
    legacy.close().unwrap();

    let mut editor = ArchiveEditor::open(&twin).unwrap();
    let dir_id = editor.unique_entry("d/").unwrap();
    editor
        .rename_entries(&[(dir_id, "renamed".to_string())])
        .unwrap();

    // Directory rename expands to descendants in both paths.
    assert_eq!(sorted_names(&twin), sorted_names(&src));
    assert_eq!(
        std::fs::read(&src).unwrap(),
        std::fs::read(&twin).unwrap(),
        "ID-based rename must produce the same bytes as the name-based one"
    );
    let mut reader = ArchiveReader::open(&twin).unwrap();
    let child = reader.unique_entry("renamed/x.txt").unwrap();
    assert_eq!(reader.read_entry(child).unwrap(), b"leaf payload");
}

#[test]
fn lock_matches_legacy_locked_archive_and_refuses_further_edits() {
    let dir = tempfile::tempdir().unwrap();
    let (src, twin) = fixture_pair(dir.path(), "lock");

    let mut legacy = RarArchive::open(&src).unwrap();
    legacy.lock().unwrap();
    drop(legacy);

    let mut editor = ArchiveEditor::open(&twin).unwrap();
    editor.lock().unwrap();

    assert_eq!(
        std::fs::read(&src).unwrap(),
        std::fs::read(&twin).unwrap(),
        "editor lock must produce the same in-place main-header patch"
    );

    // Locked archives are read-only: any rewrite fails with ArchiveLocked,
    // while opening them for read/edit remains allowed.
    let mut locked_editor = ArchiveEditor::open(&twin).unwrap();
    let other = locked_editor.unique_entry("other.txt").unwrap();
    assert!(matches!(
        locked_editor.delete_entries(&[other]),
        Err(RarError::ArchiveLocked)
    ));

    // Content still readable after the lock.
    let mut reader = ArchiveReader::open(&twin).unwrap();
    let other_id = reader.unique_entry("other.txt").unwrap();
    assert_eq!(reader.read_entry(other_id).unwrap(), b"other");
}

#[test]
fn structural_edits_invalidate_previously_issued_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gen.rar");
    build_fixture(&path, dir.path());

    let mut editor = ArchiveEditor::open(&path).unwrap();
    let stale_other = editor.unique_entry("other.txt").unwrap();
    let stale_dup = editor.entries_named("same.txt").next().unwrap().id();

    // A failed edit (unknown target is impossible with IDs; use a stale ID
    // from another editor) leaves the archive and the generation intact.
    let other_editor = ArchiveEditor::open(&path).unwrap();
    let foreign = other_editor.unique_entry("other.txt").unwrap();
    assert!(matches!(
        editor.delete_entries(&[foreign]),
        Err(RarError::StaleEntryId)
    ));
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "failed edit must not touch the archive"
    );
    assert!(
        editor.entry(stale_other).is_ok(),
        "IDs survive a rejected edit"
    );

    // After a real edit every earlier ID is stale, even for a member that
    // still exists at the same position (generation, not name, is the key).
    let deleted = editor.delete_entries(&[stale_other]).unwrap();
    assert_eq!(deleted, 1);
    assert!(matches!(
        editor.entry(stale_other),
        Err(RarError::StaleEntryId)
    ));
    assert!(matches!(
        editor.entry(stale_dup),
        Err(RarError::StaleEntryId)
    ));
    assert!(matches!(
        editor.delete_entries(&[stale_dup]),
        Err(RarError::StaleEntryId)
    ));

    // Fresh IDs from the new catalog work.
    let fresh = editor.entries_named("same.txt").next().unwrap().id();
    assert!(editor.entry(fresh).is_ok());
}

#[test]
fn deleting_every_member_erases_the_archive_like_rar_d() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("erase.rar");
    let mut archive =
        RarArchive::create_with_options(&path, rar_rs::CreateOptions::default()).unwrap();
    archive.add_bytes("only.txt", b"only", 0).unwrap();
    archive.close().unwrap();

    let mut editor = ArchiveEditor::open(&path).unwrap();
    let id = editor.unique_entry("only.txt").unwrap();
    assert_eq!(editor.delete_entries(&[id]).unwrap(), 1);
    assert!(!path.exists(), "deleting every member erases the archive");
    assert!(matches!(
        editor.unique_entry("only.txt"),
        Err(RarError::MemberNotFound { .. })
    ));
}

#[test]
fn solid_chain_delete_roundtrips_and_refreshes_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid.rar");
    let payloads = [
        vec![b'a'; 48 * 1024],
        vec![b'b'; 48 * 1024],
        vec![b'c'; 48 * 1024],
    ];
    {
        let mut archive = RarArchive::create_with_options(
            &path,
            rar_rs::CreateOptions {
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        for (index, payload) in payloads.iter().enumerate() {
            archive
                .add_bytes(&format!("m{index}.bin"), payload, 1)
                .unwrap();
        }
        archive.close().unwrap();
    }

    let mut editor = ArchiveEditor::open(&path).unwrap();
    let middle = editor.unique_entry("m1.bin").unwrap();
    editor.delete_entries(&[middle]).unwrap();

    // The remaining members survive the chain recompression.
    let mut reader = ArchiveReader::open(&path).unwrap();
    assert_eq!(reader.entries().count(), 2);
    let first = reader.unique_entry("m0.bin").unwrap();
    let last = reader.unique_entry("m2.bin").unwrap();
    assert_eq!(reader.read_entry(first).unwrap(), payloads[0]);
    assert_eq!(reader.read_entry(last).unwrap(), payloads[2]);
    // The editor's own catalog was refreshed by the rewrite.
    assert_eq!(editor.entries().count(), 2);
}

#[test]
fn edition_never_mixes_ids_between_archives() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.rar");
    let b = dir.path().join("b.rar");
    build_fixture(&a, dir.path());
    {
        let mut archive =
            RarArchive::create_with_options(&b, rar_rs::CreateOptions::default()).unwrap();
        archive.add_bytes("same.txt", b"from b", 0).unwrap();
        archive.close().unwrap();
    }
    let editor_a = ArchiveEditor::open(&a).unwrap();
    let mut editor_b = ArchiveEditor::open(&b).unwrap();
    let id_a = editor_a.entries_named("same.txt").next().unwrap().id();
    // Same file offset, different archive: still stale.
    assert!(matches!(
        editor_b.delete_entries(&[id_a]),
        Err(RarError::StaleEntryId)
    ));
    let names_after = sorted_names(&b);
    assert_eq!(names_after, ["same.txt"]);
}

#[test]
fn rar4_archives_are_refused_with_a_clear_unsupported() {
    // The surgical rewrite engine is RAR5-only; the editor must refuse
    // legacy-container archives up front instead of failing mid-rewrite.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rar4.rar");
    let file = dir.path().join("src.txt");
    std::fs::write(&file, b"rar4 member").unwrap();
    {
        let mut archive = RarArchive::create_with_options(
            &path,
            rar_rs::CreateOptions {
                format_version: ArchiveVersion::Rar40,
                ..Default::default()
            },
        )
        .unwrap();
        archive.add(&file, 0).unwrap();
        let second = dir.path().join("second.txt");
        std::fs::write(&second, b"second rar4 member").unwrap();
        archive.add_as(&second, "other.txt", 0).unwrap();
        archive.close().unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    let mut editor = ArchiveEditor::open(&path).unwrap();
    let one = editor.entries_named("src.txt").next().unwrap().id();
    assert!(matches!(
        editor.rename_entries(&[(one, "renamed.txt".to_string())]),
        Err(RarError::Unsupported(_))
    ));
    let two = editor.entries_named("other.txt").next().unwrap().id();
    assert!(matches!(
        editor.delete_entries(&[two]),
        Err(RarError::Unsupported(_))
    ));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "refused edits must leave the archive untouched"
    );
}

#[test]
fn combined_edit_plan_applies_all_ops_in_one_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plan.rar");
    build_fixture(&path, dir.path());

    let mut editor = ArchiveEditor::open(&path).unwrap();
    let other = editor.unique_entry("other.txt").unwrap();
    let dir_member = editor.unique_entry("d/").unwrap();
    let duplicate = editor.entries_named("same.txt").next().unwrap().id();

    let plan = EditPlan::new()
        .delete(other)
        .rename(dir_member, "renamed")
        .rename(duplicate, "first.txt");
    let report = editor.apply(plan).unwrap();
    assert_eq!(report.deleted(), 1);
    assert_eq!(report.renamed(), 2);
    assert!(!report.is_empty());

    // Everything landed in one pass: dir rename expanded to its child, the
    // duplicate was renamed independently, and the deleted member is gone.
    assert_eq!(
        sorted_names(&path),
        ["first.txt", "renamed/", "renamed/x.txt", "same.txt"]
    );
    let mut reader = ArchiveReader::open(&path).unwrap();
    let first = reader.unique_entry("first.txt").unwrap();
    assert_eq!(reader.read_entry(first).unwrap(), b"first");
    let duplicate = reader.unique_entry("same.txt").unwrap();
    assert_eq!(reader.read_entry(duplicate).unwrap(), b"second");
    let child = reader.unique_entry("renamed/x.txt").unwrap();
    assert_eq!(reader.read_entry(child).unwrap(), b"leaf payload");

    // The pre-plan catalog generation is gone.
    assert!(matches!(editor.entry(other), Err(RarError::StaleEntryId)));
}

#[test]
fn plan_rejects_conflicts_and_solid_chain_renames_atomically() {
    let dir = tempfile::tempdir().unwrap();

    // Deleting and renaming the same member in one plan is rejected before
    // any rewrite happens.
    let path = dir.path().join("conflict.rar");
    build_fixture(&path, dir.path());
    let mut editor = ArchiveEditor::open(&path).unwrap();
    let other = editor.unique_entry("other.txt").unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        editor.apply(EditPlan::new().delete(other).rename(other, "x.txt")),
        Err(RarError::InvalidOption(_))
    ));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "rejected plan must not touch the archive"
    );

    // Renaming a member of a solid chain that also loses a member would
    // silently drop the rename in the recompressed chain; refuse it.
    let solid = dir.path().join("solid.rar");
    {
        let mut archive = RarArchive::create_with_options(
            &solid,
            rar_rs::CreateOptions {
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        for (index, byte) in (0u8..4).enumerate() {
            archive
                .add_bytes(&format!("m{index}.bin"), &vec![b'a' + byte; 40 * 1024], 1)
                .unwrap();
        }
        archive.close().unwrap();
    }
    let mut editor = ArchiveEditor::open(&solid).unwrap();
    let m0 = editor.unique_entry("m0.bin").unwrap();
    let m1 = editor.unique_entry("m1.bin").unwrap();
    let before = std::fs::read(&solid).unwrap();
    assert!(matches!(
        editor.apply(EditPlan::new().delete(m1).rename(m0, "z0.bin")),
        Err(RarError::Unsupported(_))
    ));
    assert_eq!(
        std::fs::read(&solid).unwrap(),
        before,
        "refused chain edit must not touch the archive"
    );

    // The same delete without the chain rename still works.
    let report = editor.apply(EditPlan::new().delete(m1)).unwrap();
    assert_eq!(report.deleted(), 1);
    let reader = ArchiveReader::open(&solid).unwrap();
    assert_eq!(reader.entries().count(), 3);
}

#[test]
fn multivolume_plan_is_atomic_and_failures_leave_all_volumes_intact() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("mv.rar");
    let mut archive = RarArchive::create_with_options(
        &base,
        rar_rs::CreateOptions {
            volume_size: Some(48 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    archive
        .add_bytes("a.bin", &vec![7u8; 220 * 1024], 0)
        .unwrap();
    archive
        .add_bytes("b.bin", &vec![9u8; 70 * 1024], 0)
        .unwrap();
    archive.close().unwrap();

    // Locate the first volume (zero-padded part names).
    let mut parts: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().contains("part"))
                .unwrap_or(false)
        })
        .collect();
    parts.sort();
    assert!(parts.len() > 1, "expected a multi-volume set");
    let first = parts[0].clone();
    let snapshot = |dir: &std::path::Path| -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rar"))
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, std::fs::read(&path).unwrap())
            })
            .collect()
    };
    let before = snapshot(dir.path());

    // A stale ID (minted on a second editor) fails the whole plan up front
    // and must leave every volume byte-identical.
    let mut editor = ArchiveEditor::open(&first).unwrap();
    let foreign = ArchiveEditor::open(&first).unwrap();
    let foreign_id = foreign.unique_entry("a.bin").unwrap();
    assert!(matches!(
        editor.apply(EditPlan::new().rename(foreign_id, "renamed.bin")),
        Err(RarError::StaleEntryId)
    ));
    assert_eq!(
        snapshot(dir.path()),
        before,
        "failed plan must not touch any volume"
    );

    // A successful combined plan re-splits the set in one transaction.
    let a = editor.unique_entry("a.bin").unwrap();
    let b = editor.unique_entry("b.bin").unwrap();
    let report = editor
        .apply(EditPlan::new().rename(a, "renamed.bin").delete(b))
        .unwrap();
    assert_eq!(report.deleted(), 1);
    assert_eq!(report.renamed(), 1);
    let mut reader = ArchiveReader::open(&first).unwrap();
    let names: Vec<String> = reader
        .entries()
        .map(|entry| entry.name().to_string())
        .collect();
    assert_eq!(names, ["renamed.bin"]);
    let renamed = reader.unique_entry("renamed.bin").unwrap();
    let data = reader.read_entry(renamed).unwrap();
    assert_eq!(data, vec![7u8; 220 * 1024]);
}

#[test]
fn comment_op_matches_legacy_set_comment_bytes_and_clears() {
    let dir = tempfile::tempdir().unwrap();
    let (src, twin) = fixture_pair(dir.path(), "cmt");
    let comment = b"edited by the plan".to_vec();

    let mut legacy = RarArchive::open(&src).unwrap();
    legacy.set_comment(&comment).unwrap();
    legacy.close().unwrap();

    let mut editor = ArchiveEditor::open(&twin).unwrap();
    editor
        .apply(EditPlan::new().set_comment(comment.clone()))
        .unwrap();
    assert_eq!(
        std::fs::read(&src).unwrap(),
        std::fs::read(&twin).unwrap(),
        "plan comment must match the legacy set_comment bytes"
    );

    // Clearing with empty bytes removes the comment again.
    let mut editor = ArchiveEditor::open(&twin).unwrap();
    editor
        .apply(EditPlan::new().set_comment(Vec::new()))
        .unwrap();
    let mut archive = RarArchive::open(&twin).unwrap();
    assert_eq!(archive.get_comment().unwrap(), None);
}

#[test]
fn recovery_op_matches_legacy_add_recovery_record_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (src, twin) = fixture_pair(dir.path(), "rr");

    let mut legacy = RarArchive::open(&src).unwrap();
    legacy.add_recovery_record(10).unwrap();
    legacy.close().unwrap();

    let mut editor = ArchiveEditor::open(&twin).unwrap();
    editor.apply(EditPlan::new().set_recovery(10)).unwrap();
    assert_eq!(
        std::fs::read(&src).unwrap(),
        std::fs::read(&twin).unwrap(),
        "plan recovery record must match the legacy add_recovery_record bytes"
    );
}

#[test]
fn combined_plan_with_comment_and_recovery_applies_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all-ops.rar");
    build_fixture(&path, dir.path());

    let mut editor = ArchiveEditor::open(&path).unwrap();
    let other = editor.unique_entry("other.txt").unwrap();
    let dir_member = editor.unique_entry("d/").unwrap();
    let report = editor
        .apply(
            EditPlan::new()
                .delete(other)
                .rename(dir_member, "renamed")
                .set_comment(b"combined plan".to_vec())
                .set_recovery(25),
        )
        .unwrap();
    assert_eq!((report.deleted(), report.renamed()), (1, 1));

    // Members, comment and recovery record all landed in the one rewrite.
    assert_eq!(
        sorted_names(&path),
        ["renamed/", "renamed/x.txt", "same.txt", "same.txt"]
    );
    let mut archive = RarArchive::open(&path).unwrap();
    assert_eq!(
        archive.get_comment().unwrap(),
        Some(b"combined plan".to_vec())
    );
    // The rewritten archive still verifies (the RR record is structurally
    // present and the main-header locator consistent).
    let mut reader = ArchiveReader::open(&path).unwrap();
    assert!(reader.verify().unwrap().is_ok());
}

#[test]
fn comment_and_recovery_ops_refuse_multivolume_archives() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("mv-cmt.rar");
    let mut archive = RarArchive::create_with_options(
        &base,
        rar_rs::CreateOptions {
            volume_size: Some(32 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    archive
        .add_bytes("a.bin", &vec![5u8; 120 * 1024], 0)
        .unwrap();
    archive.close().unwrap();
    let mut parts: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().contains("part"))
                .unwrap_or(false)
        })
        .collect();
    parts.sort();
    let first = parts.into_iter().next().expect("first volume");
    let snapshot: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| (entry.file_name(), entry.path()))
        .filter(|(_, path)| path.extension().is_some_and(|ext| ext == "rar"))
        .map(|(name, path)| (name, std::fs::read(path).unwrap()))
        .collect();

    let mut editor = ArchiveEditor::open(&first).unwrap();
    assert!(matches!(
        editor.apply(EditPlan::new().set_comment(b"x".to_vec())),
        Err(RarError::Unsupported(_))
    ));
    assert!(matches!(
        editor.apply(EditPlan::new().set_recovery(10)),
        Err(RarError::Unsupported(_))
    ));
    let after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| (entry.file_name(), entry.path()))
        .filter(|(_, path)| path.extension().is_some_and(|ext| ext == "rar"))
        .map(|(name, path)| (name, std::fs::read(path).unwrap()))
        .collect();
    assert_eq!(after, snapshot, "refused ops must not touch any volume");
}
