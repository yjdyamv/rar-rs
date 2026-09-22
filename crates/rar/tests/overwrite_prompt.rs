//! The interactive overwrite prompt: `ExtractOptions::prompt_overwrite`
//! together with `ArchiveReader::set_overwrite_prompt`. The library never
//! reads the terminal itself, so these tests drive the seam directly.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rar_rs::{
    ArchiveReader, ArchiveWriter, EntryWriteOptions, ExtractOptions, OverwriteChoice, RarError,
};

fn write_archive(path: &std::path::Path, members: &[(&str, &[u8])]) {
    let mut writer = ArchiveWriter::create(path).expect("create archive");
    for (name, data) in members {
        writer
            .add_bytes(name, data, EntryWriteOptions::new())
            .expect("add member");
    }
    writer.finish().expect("finish archive");
}

/// The options the CLI emits when it owns the overwrite decision: the prompt
/// is on and the non-interactive skip default is off.
fn prompt_options() -> ExtractOptions {
    ExtractOptions {
        prompt_overwrite: true,
        skip_existing: false,
        ..ExtractOptions::default()
    }
}

#[test]
fn the_prompt_decides_whether_an_existing_file_is_replaced() {
    let dir = make_temp_dir();
    let archive = dir.path().join("a.rar");
    write_archive(&archive, &[("a.txt", b"archived")]);
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("a.txt"), b"on disk").unwrap();

    // Skip: the on-disk file wins and is reported as skipped.
    let mut rar = ArchiveReader::open(&archive).unwrap();
    rar.set_overwrite_prompt(Some(Arc::new(|_| OverwriteChoice::Skip)));
    let report = rar
        .extract_all_with_options(&dest, prompt_options())
        .unwrap();
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"on disk");
    assert!(report.written().is_empty());
    assert_eq!(report.skipped().len(), 1);

    // Overwrite: the archived bytes win.
    let mut rar = ArchiveReader::open(&archive).unwrap();
    rar.set_overwrite_prompt(Some(Arc::new(|_| OverwriteChoice::Overwrite)));
    rar.extract_all_with_options(&dest, prompt_options())
        .unwrap();
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"archived");
}

#[test]
fn rename_keeps_the_original_and_writes_a_numbered_copy() {
    let dir = make_temp_dir();
    let archive = dir.path().join("b.rar");
    write_archive(&archive, &[("a.txt", b"archived"), ("b.txt", b"archived")]);
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    // Only a.txt exists on disk; b.txt must extract without a prompt.
    std::fs::write(dest.join("a.txt"), b"on disk").unwrap();

    let mut rar = ArchiveReader::open(&archive).unwrap();
    rar.set_overwrite_prompt(Some(Arc::new(|_| OverwriteChoice::Rename)));
    rar.extract_all_with_options(&dest, prompt_options())
        .unwrap();

    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"on disk");
    assert_eq!(std::fs::read(dest.join("a(1).txt")).unwrap(), b"archived");
    assert_eq!(std::fs::read(dest.join("b.txt")).unwrap(), b"archived");
}

#[test]
fn quit_aborts_with_cancelled() {
    let dir = make_temp_dir();
    let archive = dir.path().join("c.rar");
    write_archive(&archive, &[("a.txt", b"archived")]);
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("a.txt"), b"on disk").unwrap();

    let mut rar = ArchiveReader::open(&archive).unwrap();
    rar.set_overwrite_prompt(Some(Arc::new(|_| OverwriteChoice::Quit)));
    let error = rar
        .extract_all_with_options(&dest, prompt_options())
        .unwrap_err();
    assert!(matches!(error, RarError::Cancelled), "got {error:?}");
}

#[test]
fn without_a_prompt_the_flag_falls_back_to_skipping() {
    let dir = make_temp_dir();
    let archive = dir.path().join("d.rar");
    write_archive(&archive, &[("a.txt", b"archived")]);
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("a.txt"), b"on disk").unwrap();

    // `prompt_overwrite` with no callback installed must never overwrite.
    let mut rar = ArchiveReader::open(&archive).unwrap();
    rar.extract_all_with_options(&dest, prompt_options())
        .unwrap();
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"on disk");
}

#[test]
fn the_prompt_is_asked_only_for_existing_destinations() {
    let dir = make_temp_dir();
    let archive = dir.path().join("e.rar");
    write_archive(&archive, &[("a.txt", b"x"), ("b.txt", b"y")]);
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("a.txt"), b"on disk").unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let mut rar = ArchiveReader::open(&archive).unwrap();
    rar.set_overwrite_prompt(Some(Arc::new(move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
        OverwriteChoice::Skip
    })));
    rar.extract_all_with_options(&dest, prompt_options())
        .unwrap();

    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "only the existing a.txt may prompt"
    );
}
