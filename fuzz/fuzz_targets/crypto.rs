//! Crypto fuzz target: key derivation (bounded strength), encryption
//! parameter parsing and AES-256-CBC round trips with random key
//! material. The strength byte is masked so the KDF stays cheap.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar5_fuzz::crypto(data));

#[cfg(not(fuzzing))]
fn main() {
    rar5_fuzz::standalone("crypto", rar5_fuzz::CORPUS_ALL, rar5_fuzz::crypto);
}
