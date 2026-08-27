//! Parse fuzz target: random bytes as an archive, exercising the full
//! RAR5/RAR7 read surface — block envelope, vints, headers, extra
//! records, solid chains, encryption-header scans, extraction.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar5_fuzz::parse(data));

#[cfg(not(fuzzing))]
fn main() {
    rar5_fuzz::standalone("parse", rar5_fuzz::CORPUS_PARSE, rar5_fuzz::parse);
}
