#![allow(deprecated)] // legacy constructor family; use create_with_options
//! RAR4 containers are now accepted and decoded.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::RarArchive;

#[test]
fn synthetic_rar4_with_bogus_header_is_refused_with_clear_error() {
    let dir = make_temp_dir();
    let path = dir.path().join("rar4_synthetic.rar");
    // Valid signature + a marker-block header with head_size=4 (below
    // minimum 7), which must fail with a Format error.
    let mut data = b"Rar!\x1a\x07\x00".to_vec();
    data.extend_from_slice(&[0x72, 0x04, 0x00, 0x00, 0x00]);
    std::fs::write(&path, &data).unwrap();

    let err = match RarArchive::open(&path) {
        Ok(_) => panic!("expected synthetic RAR4 with broken header to fail"),
        Err(e) => e,
    };
    match err {
        rar_rs::RarError::Format(msg) => assert!(
            msg.contains("too small") || msg.contains("truncated"),
            "expected format-level error, got: {msg}"
        ),
        other => panic!("expected Format error for broken RAR4 header, got {other:?}"),
    }
}
