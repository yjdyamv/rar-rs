//! Rewrite fuzz target: create a base archive, then apply surgical
//! mutations (delete, rename, append, comment, lock) driven by the
//! input, verifying the surviving members byte-for-byte after every
//! step.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar5_fuzz::rewrite(data));

#[cfg(not(fuzzing))]
fn main() {
    // Write-side targets do real file I/O per iteration (and Windows file
    // churn is slow), so they default lower than the read targets.
    rar5_fuzz::standalone_with("rewrite", rar5_fuzz::CORPUS_ALL, rar5_fuzz::rewrite, 20_000);
}
