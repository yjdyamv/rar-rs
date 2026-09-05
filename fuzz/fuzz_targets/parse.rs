//! Parse fuzz target: random bytes as an archive, exercising the full
//! RAR5/RAR7 read surface — block envelope, vints, headers, extra
//! records, solid chains, encryption-header scans, extraction.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar_rs_fuzz::parse(data));

#[cfg(not(fuzzing))]
fn main() {
    rar_rs_fuzz::standalone("parse", rar_rs_fuzz::CORPUS_PARSE, rar_rs_fuzz::parse);
}
