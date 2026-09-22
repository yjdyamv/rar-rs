//! Structured recovery-record mutations.
//!
//! Raw-byte robustness is covered by `robustness.rs`; this file targets the
//! checksum gates that random bytes never pass. It shares the mutation
//! generators with the fuzz targets (`support/structured.rs`, included via
//! `#[path]`) so the deterministic sequences exercised here are the ones the
//! fuzzer runs:
//!
//! * valid `{RB}` inline records built with `rar_rs::wire`, then plan /
//!   geometry / shard-state / parity mutations with the CRC64-XZ recomputed,
//! * REV5 `.rev` headers (CRC32 recomputed) driven through
//!   `rebuild_missing_volumes`,
//! * rev3 `.rev` sets built with the public API, trailer mutations with the
//!   CRC32 recomputed, then `rar rc` over the removed-volume set,
//! * RAR4/RAR13 block envelopes with the 16-bit header CRC / rolling member
//!   checksum recomputed, run through the full legacy read path.
//!
//! Every library call must come back as a classified `RarError` (or a
//! success); a panic is a defect.

#![allow(dead_code)] // the shared mutator module exposes more than this test drives

#[path = "support/structured.rs"]
mod structured;

use std::panic::{AssertUnwindSafe, catch_unwind};

/// Same xorshift64* PRNG as `fuzz/src/lib.rs`; the shared module resolves
/// `crate::Rng` to this type.
pub struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for byte in buf.iter_mut() {
            *byte = self.next_u64() as u8;
        }
    }
}

fn winrar5() -> &'static [u8] {
    include_bytes!("fixtures/rar50/winrar5_multiple_files.rar")
}

fn legacy_seeds() -> [&'static [u8]; 5] {
    [
        include_bytes!("fixtures/rar13/MULTIFIL.RAR"),
        include_bytes!("fixtures/rar40/rar2/rar20.rar"),
        include_bytes!("fixtures/rar40/winrar591_store_m0.rar"),
        include_bytes!("fixtures/rar40/rar300/compressed_text_rar300.rar"),
        include_bytes!("fixtures/rar40/repair/rar250_protect_head_rr1.rar"),
    ]
}

/// Run `repair_archive` on every structured inline-RR mutation; a return
/// value (success or classified error) passes, a panic fails. Hostile shard
/// geometry (reversed ranges included) is part of the generated cases now
/// that the defect is fixed; the deterministic repro remains below.
#[test]
fn structured_inline_rr_mutations_are_classified() {
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for round in 0..18usize {
        let mut rng = Rng::new(0x5EED_1000 + round as u64);
        let len = (96 + round * 113).min(winrar5().len());
        let prefix = &winrar5()[..len];
        let cases = structured::inline_rr_cases(prefix, 1 + round as u64 % 20, &mut rng);
        // Prove the structured seeds really reach the repair engine: the
        // intact record is idempotent and a single damaged prefix byte is
        // either restored byte-for-byte or rejected with a classified error.
        let intact = cases
            .iter()
            .find(|(_, label)| *label == "intact record")
            .map(|(case, _)| case.clone())
            .expect("generator emits the intact case");
        assert_eq!(
            rar_rs::repair_archive(&intact).expect("intact record repairs"),
            intact
        );
        if let Some((damaged, _)) = cases.iter().find(|(_, label)| *label == "damaged prefix")
            && let Ok(repaired) = rar_rs::repair_archive(damaged)
        {
            assert_eq!(
                repaired, intact,
                "a repaired prefix must equal the original"
            );
        }
        for (case, label) in cases {
            checked += 1;
            let outcome = catch_unwind(AssertUnwindSafe(|| rar_rs::repair_archive(&case)));
            match outcome {
                Ok(Ok(_)) | Ok(Err(_)) => {}
                Err(_) => failures.push(format!("round {round}: {label}")),
            }
        }
    }
    assert!(
        checked >= 200,
        "expected a few hundred structured mutations, got {checked}"
    );
    assert!(
        failures.is_empty(),
        "repair_archive panicked on structured mutations: {failures:?}"
    );
}

/// Deterministic full repair cycle over the complete embedded archive: the
/// record is built over real bytes, one data byte is corrupted, and the
/// engine must restore the original (relocation parse + RS solve + final
/// state verification).
#[test]
fn structured_inline_rr_full_repair_cycle() {
    let prefix = winrar5();
    let chunk = rar_rs::wire::build_structural_inline_recovery_data(prefix, 5).expect("build RR");
    let mut archive = prefix.to_vec();
    archive.extend_from_slice(&chunk);
    assert_eq!(
        rar_rs::repair_archive(&archive).expect("intact record repairs"),
        archive
    );

    let mut damaged = archive.clone();
    damaged[prefix.len() / 2] ^= 0x5a;
    assert_eq!(
        rar_rs::repair_archive(&damaged).expect("one damaged shard repairs"),
        archive
    );
}

/// Regression for the reversed-shard-range defect the structured mutator
/// found: `data_shards = 4`, `group_count = 2` over a 5-byte prefix used to
/// make the last shard's range start at offset 6, so `&prefix[6..5]` was a
/// reversed range and `repair_inline_recovery_prefix` panicked. The record
/// is parser-valid (matching CRC64-XZ, consistent cross-fields); the repair
/// must now classify it instead of panicking.
#[test]
fn reversed_shard_range_geometry_must_not_panic() {
    let prefix = b"Rar!\x1a";
    let mut rng = Rng::new(0x5EED_0001);
    let chunk = structured::synthetic_rb_chunk(prefix, 4, 1, 2, 0, Some(0), &mut rng);
    let mut input = prefix.to_vec();
    input.extend_from_slice(&chunk);

    let outcome = catch_unwind(AssertUnwindSafe(|| rar_rs::repair_archive(&input)));
    match outcome {
        Ok(Ok(_)) | Ok(Err(_)) => {}
        Err(_) => panic!(
            "repair_archive panicked on a parser-valid reversed shard range \
             (data_shards=4, group_count=2, prefix_len=5)"
        ),
    }
}

/// REV5 `.rev` header/volume-entry/payload mutations (header CRC recomputed)
/// plus truncations, each feeding `rebuild_missing_volumes` with a removed
/// volume.
#[test]
fn structured_rev5_mutations_are_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = "set";
    let data_count = 2usize;
    let size = 64usize;
    let mut rng = Rng::new(0x5EED_2001);
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for round in 0..6usize {
        let zeroed = round % 2 == 0;
        let mut sizes = Vec::with_capacity(data_count);
        let mut crcs = Vec::with_capacity(data_count);
        for index in 0..data_count {
            let mut bytes = vec![0u8; size];
            if !zeroed {
                rng.fill(&mut bytes);
            }
            crcs.push(structured::crc32_ieee(&bytes));
            sizes.push(bytes.len() as u64);
            std::fs::write(
                dir.path().join(format!("{base}.part{}.rar", index + 1)),
                &bytes,
            )
            .expect("write volume");
        }
        let mut payload = vec![0u8; size];
        if !zeroed {
            rng.fill(&mut payload);
        }

        let built = rar_rs::wire::build_recovery_volume_file(0, 1, &sizes, &crcs, &payload);
        // Prove the fabricated set is real: with zero-filled volumes and
        // zero parity the removed volume must be rebuilt and written back.
        if zeroed {
            let first = dir.path().join(format!("{base}.part1.rar"));
            let missing = dir.path().join(format!("{base}.part2.rar"));
            std::fs::write(dir.path().join(format!("{base}.part1.rev")), &built)
                .expect("write rev");
            let _ = std::fs::remove_file(&missing);
            let rebuilt = rar_rs::rebuild_missing_volumes(&first).expect("zeroed set rebuilds");
            assert_eq!(rebuilt, vec![missing.clone()]);
            std::fs::write(&missing, vec![0u8; size]).expect("restore volume");
        }
        let mut cases = structured::rev5_mutations(&built, &mut rng);
        cases.push((built, "rev5 as built"));
        for (case, label) in cases {
            checked += 1;
            let rev = dir.path().join(format!("{base}.part1.rev"));
            std::fs::write(&rev, &case).expect("write rev");
            let missing = dir.path().join(format!("{base}.part2.rar"));
            let _ = std::fs::remove_file(&missing);
            let first = dir.path().join(format!("{base}.part1.rar"));
            let outcome =
                catch_unwind(AssertUnwindSafe(|| rar_rs::rebuild_missing_volumes(&first)));
            match outcome {
                Ok(Ok(_)) | Ok(Err(_)) => {}
                Err(_) => failures.push(format!("rev5 round {round}: {label}")),
            }
            // Restore the removed volume so every case starts from the same
            // completed set; the bytes only need to exist.
            std::fs::write(&missing, vec![0u8; size]).expect("restore volume");
        }
    }
    assert!(checked >= 60, "expected REV5 mutations, got {checked}");
    assert!(
        failures.is_empty(),
        "rebuild_missing_volumes panicked on REV5 mutations: {failures:?}"
    );
}

/// rev3 `.rev` sets (trailer and legacy name-encoded layouts, `.partN.rar`
/// and `.rar`/`.r00` naming) with trailer mutations and a removed volume.
#[test]
fn structured_rev3_mutations_are_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = "set";
    let size = 64usize;
    let mut rng = Rng::new(0x5EED_2002);
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    // (new_naming, trailer_layout)
    for (round, (new_naming, trailer)) in
        [(true, true), (true, false), (false, true), (false, false)]
            .into_iter()
            .enumerate()
    {
        // One directory per round: `collect_recovery_volumes` scans by base
        // name, and stale `.rev` files of a different layout would otherwise
        // abort every later round with a mixed-layout error.
        let dir = dir.path().join(format!("round{round}"));
        std::fs::create_dir_all(&dir).expect("round dir");
        let mut paths = Vec::new();
        for index in 0..2usize {
            let path = if new_naming {
                dir.join(format!("{base}.part{}.rar", index + 1))
            } else if index == 0 {
                dir.join(format!("{base}.rar"))
            } else {
                dir.join(format!("{base}.r00"))
            };
            let mut bytes = vec![0u8; size];
            if index == 0 {
                // The legacy codec is dispatched on the first volume's
                // RAR4 signature.
                bytes[..7].copy_from_slice(b"Rar!\x1a\x07\x00");
            }
            bytes[7..size - 7].iter_mut().for_each(|byte| {
                *byte = rng.next_u64() as u8;
            });
            if !trailer {
                // Non-zero tail selects the legacy name-encoded layout.
                bytes[size - 1] = 0x5a;
            }
            std::fs::write(&path, &bytes).expect("write volume");
            paths.push(path);
        }
        // Force `endarc_end` to bail out on the last volume (its first
        // walked block claims `head_size < 7`), so the rebuilt bytes are
        // compared without a synthetic ENDARC truncation.
        let last = paths.len() - 1;
        let mut last_bytes = std::fs::read(&paths[last]).expect("read volume");
        last_bytes[12] = 0;
        last_bytes[13] = 0;
        std::fs::write(&paths[last], &last_bytes).expect("rewrite volume");

        let revs = rar_rs::build_recovery_volumes_for_set(&paths, 1).expect("build rv");
        let rev = revs.first().expect("rev path");

        // Prove the fabricated set is real: the removed volume must be
        // rebuilt from the generated parity byte-identically.
        let original = std::fs::read(&paths[last]).expect("read volume");
        let _ = std::fs::remove_file(&paths[last]);
        let rebuilt = rar_rs::rebuild_missing_volumes(&paths[0]).expect("rebuild volume");
        assert_eq!(rebuilt, vec![paths[last].clone()]);
        assert_eq!(
            std::fs::read(&paths[last]).expect("rebuilt volume"),
            original,
            "rev3 round {round}: rebuilt volume bytes"
        );

        let bytes = std::fs::read(rev).expect("read rev");
        let mut cases = structured::rev3_trailer_mutations(&bytes, &mut rng);
        cases.push((bytes, "rev3 as built"));
        for (case, label) in cases {
            checked += 1;
            std::fs::write(rev, &case).expect("write rev");
            let _ = std::fs::remove_file(&paths[1]);
            let entry = if round % 2 == 0 {
                rev.clone() // any `.rev` of the set is a valid entry point
            } else {
                paths[0].clone()
            };
            let outcome =
                catch_unwind(AssertUnwindSafe(|| rar_rs::rebuild_missing_volumes(&entry)));
            match outcome {
                Ok(Ok(_)) | Ok(Err(_)) => {}
                Err(_) => failures.push(format!("rev3 round {round}: {label}")),
            }
            std::fs::write(&paths[1], vec![0u8; size]).expect("restore volume");
        }
    }
    assert!(checked >= 20, "expected rev3 mutations, got {checked}");
    assert!(
        failures.is_empty(),
        "rebuild_missing_volumes panicked on rev3 mutations: {failures:?}"
    );
}

/// RAR4/RAR13 block envelope mutations (header CRC / member checksum
/// recomputed) through the reader and the legacy recovery-record scan.
#[test]
fn structured_legacy_block_mutations_are_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("in.rar");
    let fixed = dir.path().join("fixed.rar");
    let opts = rar_rs::ExtractOptions {
        safe_paths: true,
        max_unpacked_bytes: Some(8 * 1024 * 1024),
        max_total_unpacked_bytes: Some(16 * 1024 * 1024),
        ..Default::default()
    };
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for seed in legacy_seeds() {
        let mut rng = Rng::new(0x5EED_3000 + seed.len() as u64);
        for (case, label) in structured::legacy_block_cases(seed, &mut rng) {
            checked += 1;
            std::fs::write(&path, &case).expect("write archive");
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                if let Ok(mut reader) = rar_rs::ArchiveReader::open(&path) {
                    let names: Vec<String> = reader
                        .entries()
                        .map(|entry| entry.name().to_string())
                        .collect();
                    for name in names.iter().take(4) {
                        if let Some(id) = reader.entries_named(name).next().map(|entry| entry.id())
                        {
                            let _ = reader.read_entry_with_options(id, opts.clone());
                        }
                    }
                }
                if seed.starts_with(b"Rar!\x1a\x07\x00") {
                    let _ = rar_rs::repair_legacy_archive_path(&path, &fixed);
                }
            }));
            if outcome.is_err() {
                failures.push(format!("legacy seed {}: {label}", seed.len()));
            }
        }
    }
    assert!(checked >= 100, "expected legacy mutations, got {checked}");
    assert!(
        failures.is_empty(),
        "legacy block mutations panicked: {failures:?}"
    );
}
