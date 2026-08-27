//! Recovery fuzz target: inline `{RB}` chunk parsing + repair, the
//! GF(2^16) parity encode/reconstruct paths, CRC64-XZ, and `.rev` file
//! serialization — all bounded by the input length.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar5_fuzz::recovery(data));

#[cfg(not(fuzzing))]
fn main() {
    rar5_fuzz::standalone("recovery", rar5_fuzz::CORPUS_ALL, rar5_fuzz::recovery);
}
