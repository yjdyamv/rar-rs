//! Legacy block-envelope fuzz target: small genuine RAR 1.4 / RAR 2.0 /
//! RAR 3.0 / RAR4 archives with mutated block headers (16-bit header CRC
//! recomputed where the reader verifies it, RAR13 rolling member checksum
//! refreshed after payload edits), run through the full read path and the
//! legacy recovery-record scan.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar_rs_fuzz::legacy(data));

#[cfg(not(fuzzing))]
fn main() {
    // Real file I/O per iteration, like the write-side targets.
    rar_rs_fuzz::standalone_with("legacy", rar_rs_fuzz::CORPUS_ALL, rar_rs_fuzz::legacy, 20_000);
}
