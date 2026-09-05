//! Write fuzz target: create archives from fuzzed options and member
//! bytes (single/multi-volume, solid, encrypted, header-encrypted,
//! quick-open, BLAKE2sp, inline recovery record, create-time .rev),
//! verify the round trip byte-for-byte, and exercise the rv/rc paths —
//! build .rev for an existing set, delete a volume, rebuild, compare.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar_rs_fuzz::write_roundtrip(data));

#[cfg(not(fuzzing))]
fn main() {
    // Write-side targets do real file I/O per iteration (and Windows file
    // churn is slow), so they default lower than the read targets.
    rar_rs_fuzz::standalone_with(
        "write",
        rar_rs_fuzz::CORPUS_ALL,
        rar_rs_fuzz::write_roundtrip,
        20_000,
    );
}
