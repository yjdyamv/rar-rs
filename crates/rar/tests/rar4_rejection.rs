#![allow(deprecated)] // legacy constructor family; use create_with_options
//! RAR4 containers are rejected with a clear unsupported error.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar5::RarArchive;

#[test]
fn rar4_archives_are_rejected_with_clear_error() {
    let dir = make_temp_dir();
    let path = dir.path().join("rar4.rar");
    // RAR4 signature plus a marker-block header; rar-rs is RAR5-only and
    // must refuse with an actionable error (7-Zip handles RAR4).
    let mut data = b"Rar!\x1a\x07\x00".to_vec();
    data.extend_from_slice(&[0x72, 0x04, 0x00]);
    std::fs::write(&path, &data).unwrap();

    match RarArchive::open(&path) {
        Err(rar5::RarError::Unsupported(msg)) => assert!(
            msg.contains("RAR4"),
            "expected a RAR4-specific message, got: {msg}"
        ),
        Err(e) => panic!("expected Unsupported(RAR4), got {e:?}"),
        Ok(_) => panic!("expected RAR4 archive to be rejected"),
    }
}
