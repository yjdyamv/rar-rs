//! Crypto fuzz target: key derivation (bounded strength), encryption
//! parameter parsing and AES-256-CBC round trips with random key
//! material. The strength byte is masked so the KDF stays cheap.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar_rs_fuzz::crypto(data));

#[cfg(not(fuzzing))]
fn main() {
    rar_rs_fuzz::standalone("crypto", rar_rs_fuzz::CORPUS_ALL, rar_rs_fuzz::crypto);
}
