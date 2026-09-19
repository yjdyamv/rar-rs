use super::{commit_files, install_durable, read_write_create, replace_file};
use std::path::Path;

/// `Path::new("bare.rar").parent()` is `Some("")`: a bare relative
/// archive name must still yield a real directory to journal and fsync
/// in, or the Unix multi-volume commit rolls back after staging.
#[test]
fn parent_dir_normalizes_an_empty_parent_to_the_current_directory() {
    assert_eq!(super::parent_dir(Path::new("bare.rar")), Path::new("."));
    assert_eq!(super::parent_dir(Path::new("./bare.rar")), Path::new("."));
    assert_eq!(super::parent_dir(Path::new("dir")), Path::new("."));
    assert_eq!(
        super::parent_dir(Path::new("sub/bare.rar")),
        Path::new("sub")
    );
}

#[test]
fn staging_create_never_truncates_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stage.tmp");
    std::fs::write(&path, b"keep").unwrap();

    assert!(read_write_create(&path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"keep");
}

#[test]
fn failed_replace_preserves_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.tmp");
    let dest = dir.path().join("archive.rar");
    std::fs::write(&dest, b"original").unwrap();

    assert!(replace_file(&missing, &dest).is_err());
    assert_eq!(std::fs::read(dest).unwrap(), b"original");
}

#[test]
fn commit_files_restores_the_old_set_when_a_later_install_fails() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("set.part1.rar");
    let second = dir.path().join("set.part2.rar");
    std::fs::write(&first, b"old-1").unwrap();
    std::fs::write(&second, b"old-2").unwrap();
    let staged_first = dir.path().join(".stage-1");
    std::fs::write(&staged_first, b"new-1").unwrap();
    // The second staged file is deliberately missing so the install
    // fails after the first file already landed.
    let staged_second = dir.path().join(".stage-2");

    let install = vec![
        (staged_first.clone(), first.clone()),
        (staged_second, second.clone()),
    ];
    assert!(commit_files(dir.path(), "set", &install, &[]).is_err());

    assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
    assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
    // The installed file went back to its staged name for the caller's
    // cleanup instead of being lost or left at the final path.
    assert_eq!(std::fs::read(&staged_first).unwrap(), b"new-1");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5bak"))
        .collect();
    assert!(leftovers.is_empty(), "backup leftovers: {leftovers:?}");
}

#[test]
fn commit_files_replaces_and_retires_a_set() {
    let dir = tempfile::tempdir().unwrap();
    let keep = dir.path().join("set.part1.rar");
    let extra = dir.path().join("set.part2.rar");
    std::fs::write(&keep, b"old-1").unwrap();
    std::fs::write(&extra, b"old-2").unwrap();
    let staged = dir.path().join(".stage-1");
    std::fs::write(&staged, b"new-1").unwrap();

    commit_files(
        dir.path(),
        "set",
        &[(staged, keep.clone())],
        std::slice::from_ref(&extra),
    )
    .unwrap();
    assert_eq!(std::fs::read(&keep).unwrap(), b"new-1");
    assert!(!extra.exists());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5commit") || name.contains("rar5bak"))
        .collect();
    assert!(leftovers.is_empty(), "commit leftovers: {leftovers:?}");
}

#[test]
fn recovery_rolls_back_a_prepared_commit() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let final_path = parent.join("set.part1.rar");
    // The old file was parked and the new one installed, then the writer
    // was killed before the done marker was written.
    let backup = parent.join(".set.part1.rar.rar5bak-x");
    let staged = parent.join(".set.part1.rar.rar5tmp-x.part1.rar");
    std::fs::write(&backup, b"old").unwrap();
    std::fs::write(&final_path, b"new").unwrap();
    std::fs::write(
        super::journal_path(parent, "set"),
        format!(
            "rar5commit v1\nbackup\t{}\t{}\ninstall\t{}\t{}\n",
            backup.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
            staged.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
        ),
    )
    .unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert_eq!(std::fs::read(&final_path).unwrap(), b"old");
    assert!(!backup.exists());
    assert!(!super::journal_path(parent, "set").exists());
}

#[test]
fn recovery_keeps_the_new_set_when_committed() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let final_path = parent.join("set.part1.rar");
    let backup = parent.join(".set.part1.rar.rar5bak-y");
    let staged = parent.join(".set.part1.rar.rar5tmp-y.part1.rar");
    std::fs::write(&backup, b"old").unwrap();
    std::fs::write(&final_path, b"new").unwrap();
    std::fs::write(
        super::journal_path(parent, "set"),
        format!(
            "rar5commit v1\nbackup\t{}\t{}\ninstall\t{}\t{}\n",
            backup.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
            staged.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
        ),
    )
    .unwrap();
    std::fs::write(super::commit_done_path(parent, "set"), b"").unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert_eq!(std::fs::read(&final_path).unwrap(), b"new");
    assert!(!backup.exists());
    assert!(!super::commit_done_path(parent, "set").exists());
    assert!(!super::journal_path(parent, "set").exists());
}

/// A kill between the phase-1 parks leaves some finals parked (backup
/// file present) and later ones untouched (backup file missing even
/// though the journal planned one). Recovery must never delete the
/// untouched final: it is the only copy of the old data.
#[test]
fn recovery_leaves_an_unparked_final_alone_when_the_park_was_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let name = |path: &std::path::Path| path.file_name().unwrap().to_string_lossy().into_owned();
    let first = parent.join("set.part1.rar");
    let second = parent.join("set.part2.rar");
    let first_backup = parent.join(".set.part1.rar.rar5bak-x");
    // The second park never ran, so this planned backup does not exist.
    let planned_second_backup = parent.join(".set.part2.rar.rar5bak-x");
    let first_staged = parent.join(".set.part1.rar.rar5tmp-x");
    let second_staged = parent.join(".set.part2.rar.rar5tmp-x");
    std::fs::write(&first_backup, b"old-1").unwrap();
    std::fs::write(&second, b"old-2").unwrap();
    std::fs::write(&first_staged, b"new-1").unwrap();
    std::fs::write(&second_staged, b"new-2").unwrap();
    std::fs::write(
        super::journal_path(parent, "set"),
        format!(
            "rar5commit v1\n\
             backup\t{}\t{}\n\
             backup\t{}\t{}\n\
             install\t{}\t{}\n\
             install\t{}\t{}\n",
            name(&first_backup),
            name(&first),
            name(&planned_second_backup),
            name(&second),
            name(&first_staged),
            name(&first),
            name(&second_staged),
            name(&second),
        ),
    )
    .unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    // The parked original came back; the untouched final stayed put.
    assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
    assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
    assert!(!first_backup.exists());
    assert!(!first_staged.exists());
    assert!(!second_staged.exists());
    assert!(!super::journal_path(parent, "set").exists());
}

/// A kill after all parks but before any install: every final is
/// missing and every backup exists, so recovery restores the complete
/// old set and discards the staged files.
#[test]
fn recovery_restores_every_parked_final_when_no_install_ran() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let name = |path: &std::path::Path| path.file_name().unwrap().to_string_lossy().into_owned();
    let first = parent.join("set.part1.rar");
    let second = parent.join("set.part2.rar");
    let first_backup = parent.join(".set.part1.rar.rar5bak-x");
    let second_backup = parent.join(".set.part2.rar.rar5bak-x");
    let first_staged = parent.join(".set.part1.rar.rar5tmp-x");
    let second_staged = parent.join(".set.part2.rar.rar5tmp-x");
    std::fs::write(&first_backup, b"old-1").unwrap();
    std::fs::write(&second_backup, b"old-2").unwrap();
    std::fs::write(&first_staged, b"new-1").unwrap();
    std::fs::write(&second_staged, b"new-2").unwrap();
    std::fs::write(
        super::journal_path(parent, "set"),
        format!(
            "rar5commit v1\n\
             backup\t{}\t{}\n\
             backup\t{}\t{}\n\
             install\t{}\t{}\n\
             install\t{}\t{}\n",
            name(&first_backup),
            name(&first),
            name(&second_backup),
            name(&second),
            name(&first_staged),
            name(&first),
            name(&second_staged),
            name(&second),
        ),
    )
    .unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
    assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
    assert!(!first_backup.exists());
    assert!(!second_backup.exists());
    assert!(!first_staged.exists());
    assert!(!second_staged.exists());
    assert!(!super::journal_path(parent, "set").exists());
}

/// A file installed where nothing pre-existed (no backup record) is the
/// new set's own bytes and must still be removed on rollback.
#[test]
fn recovery_removes_an_installed_final_with_no_parked_original() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let final_path = parent.join("set.part1.rar");
    let staged = parent.join(".set.part1.rar.rar5tmp-x");
    std::fs::write(&final_path, b"new").unwrap();
    std::fs::write(
        super::journal_path(parent, "set"),
        format!(
            "rar5commit v1\ninstall\t{}\t{}\n",
            staged.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
        ),
    )
    .unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert!(!final_path.exists(), "a fresh install must be rolled back");
    assert!(!staged.exists());
    assert!(!super::journal_path(parent, "set").exists());
}

#[test]
fn replace_installs_the_staged_file() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("stage.tmp");
    let dest = dir.path().join("archive.rar");
    std::fs::write(&src, b"replacement").unwrap();
    std::fs::write(&dest, b"original").unwrap();

    replace_file(&src, &dest).unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), b"replacement");
    assert!(!src.exists());
}

#[test]
fn install_durable_replaces_the_destination_and_consumes_the_stage() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("stage.tmp");
    let dest = dir.path().join("archive.rar");
    std::fs::write(&src, b"durable").unwrap();
    std::fs::write(&dest, b"original").unwrap();

    install_durable(&src, &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"durable");
    assert!(!src.exists());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5tmp") || name.contains("rar5bak"))
        .collect();
    assert!(leftovers.is_empty(), "install leftovers: {leftovers:?}");
}

#[cfg(not(unix))]
#[test]
fn journal_field_escaping_round_trips_controls() {
    for name in [
        "plain",
        "tab\there",
        "line\nbreak",
        "carriage\rreturn",
        "back\\slash",
        "controls\x01\x1f\x7f",
        "all\t\n\\of them\r\x01",
    ] {
        let escaped = super::escape_journal_field(name);
        assert!(
            !escaped.chars().any(char::is_control),
            "escaped field still holds a control character: {escaped:?}"
        );
        assert_eq!(
            super::unescape_journal_field(&escaped).as_deref(),
            Some(name)
        );
    }
    assert!(super::unescape_journal_field("unknown\\x").is_none());
    assert!(super::unescape_journal_field("\\x0").is_none());
    assert!(super::unescape_journal_field("\\xzz").is_none());
    assert!(super::unescape_journal_field("raw\ttab").is_none());
    assert!(super::unescape_journal_field("trailing\\").is_none());
}

#[test]
fn journal_names_must_be_plain_siblings() {
    for bad in ["", ".", "..", "../x", "a/b", "nul\0"] {
        assert!(
            !super::plain_journal_name(std::ffi::OsStr::new(bad)),
            "{bad:?} accepted"
        );
    }
    assert!(super::plain_journal_name(std::ffi::OsStr::new(
        "set.part1.rar"
    )));
    assert!(super::plain_journal_name(std::ffi::OsStr::new(
        ".set.part1.rar.rar5bak-abc"
    )));
    // A backslash is an ordinary character on Unix and a separator on
    // Windows; `C:evil` is drive-relative only on Windows.
    #[cfg(unix)]
    assert!(super::plain_journal_name(std::ffi::OsStr::new("a\\b")));
    #[cfg(windows)]
    assert!(!super::plain_journal_name(std::ffi::OsStr::new("a\\b")));
    #[cfg(windows)]
    assert!(!super::plain_journal_name(std::ffi::OsStr::new("C:evil")));
}

#[test]
fn recovery_ignores_bad_records_and_keeps_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("archive");
    std::fs::create_dir(&parent).unwrap();
    let outside = dir.path().join("victim");
    std::fs::write(&outside, b"keep").unwrap();

    let final_path = parent.join("set.part1.rar");
    std::fs::write(&final_path, b"new").unwrap();
    let backup = parent.join(".set.part1.rar.rar5bak-x");
    std::fs::write(&backup, b"old").unwrap();

    // The forward-slash traversal stays rejected on every platform; the
    // escaped backslash traversal is only a traversal on Windows (on Unix
    // it is an ordinary, harmless sibling name). The over-long and
    // unknown-kind records are malformed everywhere.
    let escaped_traversal = "..\\\\..\\\\victim";
    std::fs::write(
        super::journal_path(&parent, "set"),
        format!(
            "rar5commit v2\n\
             install\t../../victim\t../../victim\n\
             install\t{escaped_traversal}\t{escaped_traversal}\n\
             install\thas\textra\tfields\n\
             bogus\tone\ttwo\n\
             backup\t{}\t{}\n",
            backup.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
        ),
    )
    .unwrap();

    super::recover_interrupted_commit(&parent, "set").unwrap();

    assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    // The valid backup record still rolled the parked original back.
    assert_eq!(std::fs::read(&final_path).unwrap(), b"old");
    assert!(!backup.exists());
    // Skipped records keep the journal for inspection instead of being
    // silently forgotten and deleted.
    assert!(super::journal_path(&parent, "set").exists());
}

#[test]
fn recovery_leaves_an_unknown_version_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let final_path = parent.join("set.part1.rar");
    std::fs::write(&final_path, b"new").unwrap();
    let journal = super::journal_path(parent, "set");

    for header in ["rar5commit v9", "rar5commit v1x", "not a journal"] {
        std::fs::write(
            &journal,
            format!("{header}\ninstall\tset.part1.rar\tset.part1.rar\n"),
        )
        .unwrap();
        super::recover_interrupted_commit(parent, "set").unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"new");
        assert!(journal.exists(), "journal with {header:?} must be kept");
    }
}

#[test]
fn write_commit_journal_escapes_control_characters() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let from = parent.join("bad\rname\x01");
    let to = parent.join("final.rar");

    super::write_commit_journal(parent, "set", &[(from, to)], &[], &[]).unwrap();
    let text = std::fs::read_to_string(super::journal_path(parent, "set")).unwrap();
    assert_eq!(
        text.lines().count(),
        2,
        "records must stay on one line: {text:?}"
    );
    assert!(text.contains("bad\\rname\\x01"), "{text:?}");
}

#[cfg(unix)]
#[test]
fn journal_round_trips_control_and_backslash_names_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let backup = parent.join(".set.part1.rar.rar5bak-\t\n\r\x01\\");
    let final_path = parent.join("set.part1.rar");
    let staged = parent.join(".set.part1.rar.rar5tmp-x");
    std::fs::write(&backup, b"old").unwrap();
    std::fs::write(&final_path, b"new").unwrap();

    let install = vec![(staged, final_path.clone())];
    super::write_commit_journal(
        parent,
        "set",
        &[(backup.clone(), final_path.clone())],
        &install,
        &[],
    )
    .unwrap();
    let text = std::fs::read_to_string(super::journal_path(parent, "set")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "records must stay on one line: {text:?}");
    assert!(
        lines
            .iter()
            .skip(1)
            .all(|line| line.split('\t').count() == 3),
        "malformed record: {text:?}"
    );
    assert_eq!(
        super::unescape_journal_name(lines[1].split('\t').nth(1).unwrap()).as_deref(),
        Some(backup.file_name().unwrap()),
        "escaped field must round-trip: {text:?}"
    );

    super::recover_interrupted_commit(parent, "set").unwrap();
    assert_eq!(std::fs::read(&final_path).unwrap(), b"old");
    assert!(!backup.exists());
    assert!(!super::journal_path(parent, "set").exists());
}

/// A name with invalid UTF-8 bytes is escaped byte-for-byte on Unix, so
/// recovery restores the original file instead of renaming to the
/// U+FFFD spelling (which could delete or overwrite a different file).
#[cfg(unix)]
#[test]
fn journal_round_trips_non_utf8_names_on_disk() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let final_path = parent.join(OsString::from_vec(b"caf\xE9.part1.rar".to_vec()));
    let backup = parent.join(OsString::from_vec(b".caf\xE9.part1.rar.rar5bak-x".to_vec()));
    let staged = parent.join(OsString::from_vec(b".caf\xE9.part1.rar.rar5tmp-x".to_vec()));
    // A different file at the lossy (U+FFFD) spelling: the old
    // journal encoded this name, so recovery must not touch it.
    let decoy = parent.join("caf\u{FFFD}.part1.rar");
    std::fs::write(&backup, b"old").unwrap();
    std::fs::write(&final_path, b"new").unwrap();
    std::fs::write(&decoy, b"decoy").unwrap();

    let install = vec![(staged.clone(), final_path.clone())];
    super::write_commit_journal(
        parent,
        "set",
        &[(backup.clone(), final_path.clone())],
        &install,
        &[],
    )
    .unwrap();
    // The journal stays valid UTF-8 and carries the raw byte escaped,
    // never the U+FFFD replacement character.
    let text = std::fs::read_to_string(super::journal_path(parent, "set")).unwrap();
    assert!(!text.contains('\u{FFFD}'), "{text:?}");
    assert!(text.contains("caf\\xe9.part1.rar"), "{text:?}");

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert_eq!(std::fs::read(&final_path).unwrap(), b"old");
    assert_eq!(
        std::fs::read(&decoy).unwrap(),
        b"decoy",
        "the U+FFFD spelling names a different file and must survive"
    );
    assert!(!backup.exists(), "the parked original must be consumed");
    assert!(!staged.exists());
    assert!(!super::journal_path(parent, "set").exists());
}

/// An uncommitted [`super::StagedFile`] removes its staged sibling on
/// drop and leaves the destination untouched.
#[test]
fn staged_file_drops_its_temp_when_uncommitted() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("archive.rar");
    std::fs::write(&dest, b"original").unwrap();

    {
        let (staged, mut file) = super::StagedFile::create(&dest).unwrap();
        file.write_all(b"new").unwrap();
        assert!(staged.path().exists());
    }

    assert_eq!(std::fs::read(&dest).unwrap(), b"original");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5tmp"))
        .collect();
    assert!(leftovers.is_empty(), "staged leftovers: {leftovers:?}");
}

/// A failed install (here: the destination is a directory) keeps the
/// staged file until drop, so a caller can retry or clean it.
#[test]
fn staged_file_keeps_the_stage_when_commit_fails() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("t.rar");
    std::fs::create_dir(&dest).unwrap();

    let (mut staged, mut file) = super::StagedFile::create(&dest).unwrap();
    file.write_all(b"new").unwrap();
    drop(file);
    let staged_path = staged.path().to_path_buf();

    assert!(staged.commit().is_err());
    assert!(staged_path.exists(), "the failed stage is kept until drop");
    drop(staged);
    assert!(!staged_path.exists());
    assert!(dest.is_dir(), "the destination must stay untouched");
}

#[test]
fn staged_file_commits_over_the_destination() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("archive.rar");
    std::fs::write(&dest, b"original").unwrap();

    let (mut staged, mut file) = super::StagedFile::create(&dest).unwrap();
    let staged_path = staged.path().to_path_buf();
    file.write_all(b"durable").unwrap();
    drop(file);
    staged.commit().unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"durable");
    assert!(!staged_path.exists());
}

/// `StagedCopy` copies the original, installs the copy over it on commit
/// and removes an uncommitted copy on drop.
#[test]
fn staged_copy_installs_the_copy_over_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("archive.rar");
    std::fs::write(&dest, b"original").unwrap();

    let staged_path = {
        let mut staged = super::StagedCopy::create(&dest).unwrap();
        assert_eq!(std::fs::read(staged.path()).unwrap(), b"original");
        std::fs::write(staged.path(), b"updated").unwrap();
        let staged_path = staged.path().to_path_buf();
        staged.commit().unwrap();
        staged_path
    };

    assert_eq!(std::fs::read(&dest).unwrap(), b"updated");
    assert!(!staged_path.exists());
}

/// A dropped, uncommitted copy leaves the original untouched and cleans
/// up its sibling.
#[test]
fn staged_copy_drops_uncommitted_copies() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("archive.rar");
    std::fs::write(&dest, b"original").unwrap();

    let staged_path = {
        let staged = super::StagedCopy::create(&dest).unwrap();
        let staged_path = staged.path().to_path_buf();
        std::fs::write(&staged_path, b"garbage").unwrap();
        staged_path
    };

    assert_eq!(std::fs::read(&dest).unwrap(), b"original");
    assert!(!staged_path.exists());
}

/// A failed commit keeps the staged copy until drop, so a caller can
/// retry or clean it, and never touches the original.
#[test]
fn staged_copy_keeps_the_stage_when_commit_fails() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("archive.rar");
    std::fs::write(&dest, b"original").unwrap();

    let mut staged = super::StagedCopy::create(&dest).unwrap();
    let staged_path = staged.path().to_path_buf();
    std::fs::remove_file(&dest).unwrap();
    std::fs::create_dir(&dest).unwrap();

    assert!(staged.commit().is_err());
    assert!(staged_path.exists(), "the failed stage is kept until drop");
    drop(staged);
    assert!(!staged_path.exists());
    assert!(dest.is_dir(), "the destination must stay untouched");
}

/// A failed copy removes the reserved sibling instead of leaking it.
#[test]
fn staged_copy_leaves_nothing_behind_when_the_source_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("missing.rar");

    let error = match super::StagedCopy::create(&dest) {
        Ok(_) => panic!("create must fail when the source is missing"),
        Err(error) => error,
    };
    assert!(
        matches!(error, crate::error::RarError::Io(_)),
        "got {error:?}"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no staged sibling remains"
    );
}

/// Write a staged sibling for `final_path` the way a lower-level builder
/// would, and hand it to a set with [`super::StagedSet::track`].
fn staged_sibling(final_path: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let staged = super::temp_sibling_path(final_path);
    std::fs::write(&staged, bytes).unwrap();
    staged
}

#[test]
fn staged_set_drops_staged_files_when_uncommitted() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("set.part1.rar");
    let second = dir.path().join("set.part2.rar");
    std::fs::write(&first, b"old-1").unwrap();
    std::fs::write(&second, b"old-2").unwrap();

    let (first_staged, second_staged) = {
        let mut set = super::StagedSet::new(dir.path(), "set").unwrap();
        let first_staged = staged_sibling(&first, b"new-1");
        set.track(first_staged.clone(), &first);
        let second_staged = staged_sibling(&second, b"new-2");
        set.track(second_staged.clone(), &second);
        (first_staged, second_staged)
    };

    assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
    assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
    assert!(!first_staged.exists() && !second_staged.exists());
}

#[test]
fn staged_set_commits_tracked_files() {
    let dir = tempfile::tempdir().unwrap();
    let keep = dir.path().join("set.part1.rar");
    std::fs::write(&keep, b"old-1").unwrap();

    let mut set = super::StagedSet::new(dir.path(), "set").unwrap();
    let staged = staged_sibling(&keep, b"new-1");
    set.track(staged, &keep);
    set.commit().unwrap();

    assert_eq!(std::fs::read(&keep).unwrap(), b"new-1");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5"))
        .collect();
    assert!(leftovers.is_empty(), "commit leftovers: {leftovers:?}");
}

/// A failed set commit rolls the finals back and the set's drop removes
/// every staged file: no partial set, no leftovers.
#[test]
fn staged_set_failed_commit_cleans_the_staged_files() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("set.part1.rar");
    let second = dir.path().join("set.part2.rar");
    std::fs::write(&first, b"old-1").unwrap();
    std::fs::write(&second, b"old-2").unwrap();

    let mut set = super::StagedSet::new(dir.path(), "set").unwrap();
    let first_staged = staged_sibling(&first, b"new-1");
    set.track(first_staged, &first);
    let second_staged = staged_sibling(&second, b"new-2");
    set.track(second_staged.clone(), &second);
    // Remove the second staged file so the install fails after the first
    // one already landed.
    std::fs::remove_file(&second_staged).unwrap();

    assert!(set.commit().is_err());
    // The failed set keeps its staged files until drop cleans them.
    drop(set);
    assert_eq!(std::fs::read(&first).unwrap(), b"old-1");
    assert_eq!(std::fs::read(&second).unwrap(), b"old-2");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5"))
        .collect();
    assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
}

/// A parked original stays at its parked name after a successful commit
/// while the rebuilt staged file lands at the final path.
#[test]
fn staged_set_commit_keeps_a_parked_original() {
    let dir = tempfile::tempdir().unwrap();
    let final_path = dir.path().join("set.part2.rar");
    let parked = final_path.with_extension("rar.bad");
    std::fs::write(&final_path, b"damaged").unwrap();

    let mut set = super::StagedSet::new(dir.path(), "set").unwrap();
    let staged = staged_sibling(&final_path, b"rebuilt");
    set.track(staged, &final_path);
    set.park(&final_path, parked.clone());
    set.commit().unwrap();

    assert_eq!(std::fs::read(&final_path).unwrap(), b"rebuilt");
    assert_eq!(std::fs::read(&parked).unwrap(), b"damaged");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5"))
        .collect();
    assert!(leftovers.is_empty(), "commit leftovers: {leftovers:?}");
}

/// A failed commit restores the parked original and the parked name is
/// gone: the damaged volume is never lost, and no `.bad` copy is left.
#[test]
fn staged_set_failed_commit_restores_a_parked_original() {
    let dir = tempfile::tempdir().unwrap();
    let final_path = dir.path().join("set.part2.rar");
    let parked = final_path.with_extension("rar.bad");
    std::fs::write(&final_path, b"damaged").unwrap();

    let mut set = super::StagedSet::new(dir.path(), "set").unwrap();
    let staged = staged_sibling(&final_path, b"rebuilt");
    set.track(staged.clone(), &final_path);
    set.park(&final_path, parked.clone());
    // Remove the staged file so the install fails after the park ran.
    std::fs::remove_file(&staged).unwrap();

    assert!(set.commit().is_err());
    drop(set);
    assert_eq!(std::fs::read(&final_path).unwrap(), b"damaged");
    assert!(!parked.exists(), "the parked copy must be moved back");
}

/// A kill between the journaled park and the install: the journal names
/// the park, so recovery removes the new install and returns the parked
/// original to its final name.
#[test]
fn recovery_restores_a_journaled_park() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let final_path = parent.join("set.part2.rar");
    let parked = parent.join("set.part2.rar.bad");
    let staged = parent.join(".set.part2.rar.rar5tmp-x");
    std::fs::write(&parked, b"damaged").unwrap();
    std::fs::write(&final_path, b"rebuilt").unwrap();
    std::fs::write(
        super::journal_path(parent, "set"),
        format!(
            "rar5commit v3\npark\t{}\t{}\ninstall\t{}\t{}\n",
            parked.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
            staged.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
        ),
    )
    .unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert_eq!(std::fs::read(&final_path).unwrap(), b"damaged");
    assert!(!parked.exists());
    assert!(!super::journal_path(parent, "set").exists());
}

/// With the done marker present the committed rebuild wins: the journaled
/// park stays at its parked name (it is the kept damaged original).
#[test]
fn recovery_keeps_a_journaled_park_when_committed() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let final_path = parent.join("set.part2.rar");
    let parked = parent.join("set.part2.rar.bad");
    let staged = parent.join(".set.part2.rar.rar5tmp-x");
    std::fs::write(&parked, b"damaged").unwrap();
    std::fs::write(&final_path, b"rebuilt").unwrap();
    std::fs::write(
        super::journal_path(parent, "set"),
        format!(
            "rar5commit v3\npark\t{}\t{}\ninstall\t{}\t{}\n",
            parked.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
            staged.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
        ),
    )
    .unwrap();
    std::fs::write(super::commit_done_path(parent, "set"), b"").unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert_eq!(std::fs::read(&final_path).unwrap(), b"rebuilt");
    assert_eq!(std::fs::read(&parked).unwrap(), b"damaged");
    assert!(!super::commit_done_path(parent, "set").exists());
    assert!(!super::journal_path(parent, "set").exists());
}

/// A kill between writing the journal and the park rename: the park file
/// is absent and the final still holds the original, so recovery must
/// leave it untouched.
#[test]
fn recovery_leaves_the_final_alone_when_the_park_never_ran() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    let final_path = parent.join("set.part2.rar");
    let parked = parent.join("set.part2.rar.bad");
    std::fs::write(&final_path, b"damaged").unwrap();
    std::fs::write(
        super::journal_path(parent, "set"),
        format!(
            "rar5commit v3\npark\t{}\t{}\n",
            parked.file_name().unwrap().to_string_lossy(),
            final_path.file_name().unwrap().to_string_lossy(),
        ),
    )
    .unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert_eq!(std::fs::read(&final_path).unwrap(), b"damaged");
    assert!(!parked.exists());
    assert!(!super::journal_path(parent, "set").exists());
}

/// A park whose final vanished before the commit makes the phase-1 rename
/// fail; parks that already ran must be rolled back.
#[test]
fn staged_set_park_rename_failure_restores_earlier_parks() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("set.part1.rar");
    let first_parked = dir.path().join("set.part1.rar.bad");
    let missing = dir.path().join("set.part2.rar");
    let missing_parked = dir.path().join("set.part2.rar.bad");
    std::fs::write(&first, b"damaged-1").unwrap();

    let mut set = super::StagedSet::new(dir.path(), "set").unwrap();
    set.park(&first, first_parked.clone());
    set.park(&missing, missing_parked.clone());

    assert!(set.commit().is_err());
    drop(set);
    assert_eq!(std::fs::read(&first).unwrap(), b"damaged-1");
    assert!(
        !first_parked.exists(),
        "the earlier park must be rolled back"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("rar5"))
        .collect();
    assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
}

/// A kill after the journal removal but before the marker removal leaves
/// a journal-less done marker; recovery clears it so a later commit
/// killed mid-write is not misread as finished.
#[test]
fn recovery_clears_a_stale_done_marker_without_a_journal() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path();
    std::fs::write(super::commit_done_path(parent, "set"), b"").unwrap();

    super::recover_interrupted_commit(parent, "set").unwrap();

    assert!(!super::commit_done_path(parent, "set").exists());
}
