//! Cancellation flag tests: `RarArchive::set_cancel_flag` must abort
//! create/extract at the next per-member or per-chunk check point with
//! `RarError::Cancelled`.

#![allow(deprecated)] // cancellation applies to the legacy facade too

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rar_rs::RarError;

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// A member payload that compresses (repeating pattern), several MiB so
/// chunked/parallel paths engage.
fn payload() -> Vec<u8> {
    (0..4_000_000u32).map(|i| (i % 251) as u8).collect()
}

#[test]
fn create_aborts_immediately_when_flag_already_set() {
    let dir = temp_dir();
    let path = dir.path().join("cancel.rar");
    let flag = Arc::new(AtomicBool::new(true));

    let mut rar = rar_rs::RarArchive::create_with_options(&path, rar_rs::CreateOptions::default())
        .expect("create");
    rar.set_cancel_flag(Some(flag.clone()));
    let res = rar.add_bytes("a.bin", &payload(), 3);
    assert!(
        matches!(res, Err(RarError::Cancelled)),
        "expected Cancelled, got {res:?}"
    );
    // The staged file must be cleaned up on drop, not committed.
    drop(rar);
    assert!(!path.exists());
}

#[test]
fn extract_aborts_immediately_when_flag_already_set() {
    let dir = temp_dir();
    let path = dir.path().join("cancel.rar");
    {
        let mut rar =
            rar_rs::RarArchive::create_with_options(&path, rar_rs::CreateOptions::default())
                .expect("create");
        rar.add_bytes("a.bin", &payload(), 3).expect("add");
        rar.close().expect("close");
    }
    let flag = Arc::new(AtomicBool::new(true));
    let mut rar = rar_rs::RarArchive::open(&path).expect("open");
    rar.set_cancel_flag(Some(flag.clone()));
    let res =
        rar.extract_all_with_options(dir.path().join("out"), rar_rs::ExtractOptions::default());
    assert!(
        matches!(res, Err(RarError::Cancelled)),
        "expected Cancelled, got {res:?}"
    );
    assert!(!dir.path().join("out/a.bin").exists());
}

#[test]
fn batch_aborts_midway_when_flag_flips_during_first_member() {
    let dir = temp_dir();
    let path = dir.path().join("cancel.rar");
    let flag = Arc::new(AtomicBool::new(false));
    let flag_for_cb = flag.clone();
    let member: Vec<u8> = payload();

    let mut rar = rar_rs::RarArchive::create_with_options(&path, rar_rs::CreateOptions::default())
        .expect("create");
    rar.set_cancel_flag(Some(flag.clone()));
    // Flip the flag on the first progress event (fired when the first
    // member's encoding starts): the next member's check point must abort.
    rar.set_progress_callback(Some(Box::new(move |done, total| {
        if done > 0 && total > 0 {
            flag_for_cb.store(true, Ordering::Relaxed);
        }
    })));

    let batch = [
        rar_rs::BatchEntry::Bytes {
            name: "a.bin",
            data: &member,
            level: 3,
        },
        rar_rs::BatchEntry::Bytes {
            name: "b.bin",
            data: &member,
            level: 3,
        },
        rar_rs::BatchEntry::Bytes {
            name: "c.bin",
            data: &member,
            level: 3,
        },
    ];
    let res = rar.add_batch(&batch);
    assert!(
        matches!(res, Err(RarError::Cancelled)),
        "expected Cancelled, got {res:?}"
    );
    drop(rar);
}

#[test]
fn unset_flag_allows_completion() {
    let dir = temp_dir();
    let path = dir.path().join("cancel.rar");
    let mut rar = rar_rs::RarArchive::create_with_options(&path, rar_rs::CreateOptions::default())
        .expect("create");
    rar.set_cancel_flag(Some(Arc::new(AtomicBool::new(false))));
    rar.add_bytes("a.bin", &payload(), 3).expect("add");
    rar.close().expect("close");

    let mut rar = rar_rs::RarArchive::open(&path).expect("open");
    rar.set_cancel_flag(Some(Arc::new(AtomicBool::new(false))));
    rar.extract_all_with_options(dir.path().join("out"), rar_rs::ExtractOptions::default())
        .expect("extract");
    assert_eq!(
        std::fs::read(dir.path().join("out/a.bin")).expect("read"),
        payload()
    );
}
