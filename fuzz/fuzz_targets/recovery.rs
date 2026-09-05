//! Recovery fuzz target: inline `{RB}` chunk parsing + repair, the
//! GF(2^16) parity encode/reconstruct paths, CRC64-XZ, and `.rev` file
//! serialization — all bounded by the input length.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar_rs_fuzz::recovery(data));

#[cfg(not(fuzzing))]
fn main() {
    rar_rs_fuzz::standalone("recovery", rar_rs_fuzz::CORPUS_ALL, rar_rs_fuzz::recovery);
}
