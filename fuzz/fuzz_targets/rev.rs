//! Recovery-volume fuzz target: streaming `repair_archive_path`, fabricated
//! REV5 sets rebuilt through `rebuild_missing_volumes` (header CRC
//! recomputed after mutation), and rev3 sets built with the public API and
//! mutated at the trailer (CRC32 recomputed). Reaches the shard math and
//! the `.rev` naming/layout discovery that raw bytes never reach.

#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| rar_rs_fuzz::rev(data));

#[cfg(not(fuzzing))]
fn main() {
    // Real file I/O per iteration, like the write-side targets.
    rar_rs_fuzz::standalone_with("rev", rar_rs_fuzz::CORPUS_ALL, rar_rs_fuzz::rev, 20_000);
}
