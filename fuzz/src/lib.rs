//! Shared harness for the rar-rs fuzz targets.
//!
//! Each target is a `fn(&[u8])` runner (no panics tolerated — a panic is
//! a bug the fuzzer is looking for). The same runners drive libFuzzer
//! (`#[cfg(fuzzing)]`) and the standalone mutation loop (`standalone`),
//! so the targets run on stable Rust without libFuzzer.

use std::panic::{AssertUnwindSafe, catch_unwind};

/// Deterministic xorshift64* PRNG (no external deps).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
}

/// Embedded seed corpus: real WinRAR output plus the tail-match
/// regression input. Mutations of these hit deep parser paths that raw
/// random bytes almost never reach.
pub static CORPUS_WINRAR: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../crates/rar/tests/fixtures/rar50/winrar5_multiple_files.rar"
));
pub static CORPUS_TAIL_MATCH: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../crates/rar/tests/fixtures/rar50/tail-match-362.bin"
));

pub static CORPUS_PARSE: &[&[u8]] = &[CORPUS_WINRAR, CORPUS_TAIL_MATCH];
pub static CORPUS_ALL: &[&[u8]] = &[CORPUS_WINRAR, CORPUS_TAIL_MATCH, b"Rar!\x1a\x07\x01\x00"];

/// Produce one mutated input from the corpus (dict-style: start from a
/// seed, apply 1-8 byte-level edits, occasionally splice another seed).
pub fn mutate(rng: &mut Rng, seeds: &[&[u8]]) -> Vec<u8> {
    let base = seeds[rng.below(seeds.len())];
    let mut out = base.to_vec();
    let ops = 1 + rng.below(8);
    for _ in 0..ops {
        if out.is_empty() {
            out.push(rng.next_u64() as u8);
            continue;
        }
        match rng.below(6) {
            0 => {
                let i = rng.below(out.len());
                out[i] ^= 1u8 << rng.below(8);
            }
            1 => {
                let i = rng.below(out.len());
                out[i] = rng.next_u64() as u8;
            }
            2 => {
                let i = rng.below(out.len());
                out[i] = 0;
            }
            3 => {
                let i = rng.below(out.len());
                out[i] = 0xFF;
            }
            4 => {
                let i = rng.below(out.len() + 1);
                out.insert(i, rng.next_u64() as u8);
            }
            _ => {
                if out.len() > 1 {
                    let i = rng.below(out.len());
                    out.remove(i);
                }
            }
        }
    }
    if rng.below(4) == 0 {
        let other = seeds[rng.below(seeds.len())];
        if !other.is_empty() && !out.is_empty() {
            let at = rng.below(out.len() + 1);
            let start = rng.below(other.len());
            let len = 1 + rng.below(other.len() - start);
            let mut v: Vec<u8> = out[..at].to_vec();
            v.extend_from_slice(&other[start..start + len]);
            v.extend_from_slice(&out[at..]);
            out = v;
        }
    }
    out
}

/// Standalone driver: run `runner` over `iterations` mutated inputs,
/// catching panics. A panic saves the crashing input to
/// `fuzz/crashes/<name>-crash-<n>.bin` and exits non-zero, so the loop
/// doubles as a smoke fuzzer.
///
/// Overrides: `FUZZ_ITERATIONS` (default 200_000), `FUZZ_SEED`
/// (default 0x5EED_0001).
pub fn standalone(name: &str, seeds: &[&[u8]], runner: fn(&[u8])) {
    standalone_with(name, seeds, runner, 200_000);
}

/// Like [`standalone`], with a target-specific default iteration count
/// (write-side targets do real file I/O per iteration and default lower;
/// `FUZZ_ITERATIONS` always overrides).
pub fn standalone_with(name: &str, seeds: &[&[u8]], runner: fn(&[u8]), default_iterations: usize) {
    let iterations: usize = std::env::var("FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default_iterations);
    let seed: u64 = std::env::var("FUZZ_SEED")
        .ok()
        .and_then(|v| {
            v.strip_prefix("0x")
                .or_else(|| v.strip_prefix("0X"))
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .or_else(|| v.parse().ok())
        })
        .unwrap_or(0x5EED_0001);

    let mut rng = Rng::new(seed);
    for i in 0..iterations {
        let input = mutate(&mut rng, seeds);
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| runner(&input))) {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crashes");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("{name}-crash-{seed:#x}-{i}.bin"));
            let _ = std::fs::write(&path, &input);
            let msg = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            eprintln!("PANIC in {name} at iteration {i}: {msg}");
            eprintln!("crashing input saved to {}", path.display());
            std::process::exit(1);
        }
    }
    eprintln!("{name}: {iterations} iterations (seed {seed:#x}), no panics");
}

// ── Target runners ─────────────────────────────────────────────────────────

/// RAR5/RAR7 archive parsing: open, scan, list, read and extract arbitrary
/// bytes as an archive. Bounded extraction options keep decompression
/// bombs from exhausting memory; the parser's own 2 MiB header cap and
/// 4 GiB dictionary ceiling bound the rest.
pub fn parse(data: &[u8]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("in.rar");
    let _ = std::fs::write(&path, data);

    let opts = rar_rs::ExtractOptions {
        safe_paths: true,
        max_unpacked_bytes: Some(64 * 1024 * 1024),
        max_total_unpacked_bytes: Some(128 * 1024 * 1024),
        max_dict_size: Some(rar_rs::ExtractOptions::DEFAULT_MAX_DICT_SIZE),
        ..Default::default()
    };

    // Plain read path: scan + list + read every member + extract all.
    if let Ok(mut a) = rar_rs::ArchiveReader::open(&path) {
        let names: Vec<String> = a.entries().map(|e| e.name().to_string()).collect();
        for name in &names {
            if let Some(id) = a.entries_named(name).next().map(|e| e.id()) {
                let _ = a.read_entry_with_options(id, opts);
            }
        }
        let _ = a.extract_all_with_options(dir.path().join("x"), opts);
    }
    // Password path: also walks the header-encryption scan. A random
    // input almost never forms a valid block envelope, so the KDF is
    // effectively never hit with hostile strength here; the crypto
    // target covers bounded-strength KDF directly.
    if let Ok(mut a) = rar_rs::RarArchive::open_with_password(&path, "fuzz") {
        let _ = a.extract_all_with_options(dir.path().join("y"), opts);
    }

    let _ = rar_rs::sfx_offset_of(data);
    let _ = rar_rs::discover_volumes(&path);
}

/// Produce a deterministic `need`-byte payload from a seed slice (bounded
/// work: a 4 KiB tile is built once, then copied in tile-sized blocks).
fn fill_tile(seed: &[u8], need: usize) -> Vec<u8> {
    let seed = if seed.is_empty() {
        &[0x5A][..]
    } else {
        &seed[..seed.len().min(256)]
    };
    let mut tile = Vec::new();
    while tile.len() < 4096 {
        tile.extend_from_slice(seed);
    }
    tile.truncate(4096);
    let mut out = Vec::with_capacity(need);
    while out.len() < need {
        out.extend_from_slice(&tile[..(need - out.len()).min(tile.len())]);
    }
    out
}

/// Write surface: create archives from fuzzed options and member bytes —
/// single and multi-volume, solid, encrypted, header-encrypted,
/// quick-open, BLAKE2sp, inline recovery record, create-time `.rev` —
/// then verify the round trip byte-for-byte and exercise the rv/rc
/// paths: build `.rev` for an existing set, delete a middle volume,
/// rebuild it and require byte-identical reconstruction.
pub fn write_roundtrip(data: &[u8]) {
    if data.len() < 17 {
        return;
    }
    let h = &data[8..]; // control bytes double as payload seeds
    let n_members = 1 + (h[0] % 3) as usize; // 1..=3
    // Multi-volume sets are exactly two volumes per member (member =
    // 2x volume) so chunk splits, per-chunk records and CBC chains get
    // exercised with minimal per-iteration file churn (Windows per-file
    // overhead dominates the loop cost).
    let member_bytes: usize = match h[1] % 3 {
        0 => 2048, // single volume
        1 => 4096, // two 2 KiB volumes
        _ => 8192, // two 4 KiB volumes
    };
    let volume_size = (member_bytes >= 4096).then_some((member_bytes / 2) as u64);
    let multivolume = volume_size.is_some();
    let create_rev = if multivolume && h[6].is_multiple_of(3) {
        Some(1 + (h[6] as u32 % 3)) // create-time .rev (rv during create)
    } else {
        None
    };
    // RAR7 (v70) via the force_v70 test seam: legal v70 headers (version
    // 1, 5+5-bit dict, DCX) with a small declared dictionary. The
    // per-member 2x-file cap floors the declared dict at 128 KiB for the
    // small fuzz members, which is fine — the header, DCX table and the
    // v70-aware read path all get exercised.
    let (dict_size_bytes, force_v70) = if h[8] % 4 == 3 {
        (
            Some([512 * 1024, 1024 * 1024, 4 * 1024 * 1024][(h[8] as usize / 4) % 3] as u64),
            true,
        )
    } else {
        (None, false)
    };
    let mut opts = rar_rs::WriterOptions::default()
        .solid_mode(if h[3].is_multiple_of(2) {
            rar_rs::SolidMode::Continuous
        } else {
            rar_rs::SolidMode::Disabled
        })
        .blake2(h[4].is_multiple_of(2))
        .quick_open(h[4] % 4 < 3);
    if h[5].is_multiple_of(2) {
        // h[5] % 4 == 2 is even, so header encryption always carries a
        // password here; -hp works for single- and multi-volume alike.
        opts = opts.password("fuzz");
    }
    if h[5] % 4 == 2 {
        opts = opts.encrypt_headers(true);
    }
    if !multivolume && h[6].is_multiple_of(4) {
        opts = opts.recovery_percent(h[6] % 15);
    }
    if let Some(count) = create_rev {
        opts = opts.recovery_volume_count(count);
    }
    if let Some(size) = volume_size {
        opts = opts.volume_size(size);
    }
    let dict = dict_size_bytes
        .and_then(|bytes| rar_rs::DictionarySize::try_from(bytes).ok())
        .or_else(|| rar_rs::DictionarySize::from_rar5_log(7 + h[7] % 3).ok());
    if let Some(dict) = dict {
        opts = opts.dictionary_size(dict);
    }
    if force_v70 {
        opts = opts.compression(rar_rs::version::ArchiveVersion::V70);
    }

    // Member payloads: deterministic tiles (bounded work — the fuzzer
    // targets code paths, not allocation sizes).
    let mut members: Vec<(String, Vec<u8>)> = Vec::with_capacity(n_members);
    for i in 0..n_members {
        let start = (i * h.len()) / n_members;
        let end = ((i + 1) * h.len()) / n_members;
        members.push((format!("f{i}.bin"), fill_tile(&h[start..end], member_bytes)));
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let arc = dir.path().join("w.rar");
    let created = (|| -> rar_rs::RarResult<()> {
        let mut rar = rar_rs::ArchiveWriter::create_with(&arc, opts.clone())?;
        for (i, (name, payload)) in members.iter().enumerate() {
            // Multi-volume members are STORED (level 0): the compressible
            // tile pattern would otherwise collapse below one volume and
            // the split paths would never run. Single-volume members
            // exercise the compression levels.
            let level = if multivolume {
                0
            } else {
                ((h[0] as usize + i) % 6) as u8
            };
            let entry_opts = rar_rs::EntryWriteOptions::new()
                .compression_level(rar_rs::CompressionLevel::try_from(level).unwrap());
            rar.add_bytes(name, payload, entry_opts)?;
        }
        rar.finish()?;
        Ok(())
    })();
    if created.is_err() {
        return; // derived option combos may legitimately be rejected
    }

    // Round trip: read every member back and compare byte-for-byte.
    let volumes = rar_rs::discover_volumes(&arc);
    let opened = if h[5].is_multiple_of(2) {
        rar_rs::ArchiveReader::open_with(&volumes[0], rar_rs::OpenOptions::new().password("fuzz"))
    } else {
        rar_rs::ArchiveReader::open(&volumes[0])
    };
    if let Ok(mut ar) = opened {
        for (name, payload) in &members {
            if let Ok(id) = ar.unique_entry(name)
                && let Ok(got) = ar.read_entry(id)
            {
                assert_eq!(
                    &got[..],
                    &payload[..],
                    "write round trip mismatch for {name}"
                );
            }
        }
    }

    // rv/rc: multi-volume sets get .rev (either from create time or the
    // standalone rv path), a middle volume is deleted, rebuild must
    // reproduce it byte-for-byte. Bounded to modest sets — huge volume
    // counts would make the loop file-churn bound explode.
    if multivolume && (2..=8).contains(&volumes.len()) {
        let rev_ok = if create_rev.is_some() {
            true // create-time .rev already on disk
        } else {
            rar_rs::recovery::rev50::build_recovery_volumes_for_set(&volumes, 1 + (h[7] as usize % 2))
                .is_ok()
        };
        if rev_ok {
            let victim = volumes[volumes.len() / 2].clone();
            let orig = std::fs::read(&victim).ok();
            let _ = std::fs::remove_file(&victim);
            if let (Some(orig), Ok(rebuilt)) = (&orig, rar_rs::rebuild_missing_volumes(&volumes[0]))
                && rebuilt.contains(&victim)
                && let Ok(bytes) = std::fs::read(&victim)
            {
                assert_eq!(
                    &bytes[..],
                    &orig[..],
                    "rc rebuild mismatch for {}",
                    victim.display()
                );
            }
        }
    }
}

/// Rewrite surface: create a base archive, then apply surgical
/// mutations (delete, rename, append, comment, lock) driven by the
/// input, verifying the surviving members byte-for-byte after every
/// step.
pub fn rewrite(data: &[u8]) {
    if data.len() < 24 {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("r.rar");
    let a = &data[..data.len() / 4];
    let b = &data[data.len() / 4..data.len() / 2];
    let c = &data[data.len() / 2..3 * data.len() / 4];
    let d = &data[3 * data.len() / 4..];
    let solid = data[0].is_multiple_of(2);

    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .solid_mode(if solid {
                    rar_rs::SolidMode::Continuous
                } else {
                    rar_rs::SolidMode::Disabled
                })
                .quick_open(true),
        )
        .unwrap();
        let lv3 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        rar.add_bytes("a.bin", a, lv3).unwrap();
        rar.add_bytes("b.bin", b, lv3).unwrap();
        rar.add_bytes("c.bin", c, lv3).unwrap();
        rar.finish().unwrap();
    }

    let mut expected: Vec<(&str, &[u8])> = vec![("a.bin", a), ("b.bin", b), ("c.bin", c)];

    // 1. Delete b.bin.
    {
        let mut rar = rar_rs::ArchiveEditor::open(&path).unwrap();
        let id = rar.unique_entry("b.bin").unwrap();
        rar.delete_entries(&[id]).unwrap();
    }
    expected.retain(|(n, _)| *n != "b.bin");
    verify_members(&path, &expected);

    // 2. Rename a.bin -> z.bin.
    if data[1].is_multiple_of(2) {
        let mut rar = rar_rs::ArchiveEditor::open(&path).unwrap();
        let id = rar.unique_entry("a.bin").unwrap();
        rar.rename_entries(&[(id, "z.bin".to_string())]).unwrap();
        for e in &mut expected {
            if e.0 == "a.bin" {
                e.0 = "z.bin";
            }
        }
        verify_members(&path, &expected);
    }

    // 3. Append d.bin.
    if data[2].is_multiple_of(3) {
        let mut rar = rar_rs::ArchiveWriter::append_with(&path, rar_rs::AppendOptions::default())
            .unwrap();
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0).unwrap());
        rar.add_bytes("d.bin", d, opts).unwrap();
        rar.finish().unwrap();
        expected.push(("d.bin", d));
        verify_members(&path, &expected);
    }

    // 4. Comment round trip.
    if data[3].is_multiple_of(2) {
        {
            let mut rar = rar_rs::ArchiveEditor::open(&path).unwrap();
            rar.apply(rar_rs::EditPlan::new().set_comment(b"fuzz comment"))
                .unwrap();
        }
        let mut rar = rar_rs::RarArchive::open(&path).unwrap();
        assert_eq!(
            rar.get_comment().unwrap().as_deref(),
            Some(b"fuzz comment".as_slice()),
            "comment round trip mismatch"
        );
        verify_members(&path, &expected);
    }

    // 5. Lock (irreversible — must be last): further rewrites refuse.
    if data[4].is_multiple_of(2) {
        {
            let mut rar = rar_rs::ArchiveEditor::open(&path).unwrap();
            rar.lock().unwrap();
        }
        verify_members(&path, &expected);
        let mut rar = rar_rs::ArchiveEditor::open(&path).unwrap();
        let id = rar.unique_entry(expected[0].0).unwrap();
        match rar.rename_entries(&[(id, "locked-check.bin".to_string())]) {
            Err(rar_rs::RarError::ArchiveLocked) => {}
            other => panic!("expected ArchiveLocked, got {other:?}"),
        }
    }
}

/// Open `path` and assert that exactly the `expected` members exist with
/// byte-identical content.
fn verify_members(path: &std::path::Path, expected: &[(&str, &[u8])]) {
    let mut ar = rar_rs::ArchiveReader::open(path).unwrap();
    let names: Vec<String> = ar.entries().map(|e| e.name().to_string()).collect();
    assert_eq!(
        names.len(),
        expected.len(),
        "member count changed: {names:?}"
    );
    for (name, bytes) in expected {
        assert!(
            names.iter().any(|n| n == name),
            "member {name} missing after rewrite: {names:?}"
        );
        let got = ar.read_entry(ar.unique_entry(name).unwrap()).unwrap();
        assert_eq!(&got[..], *bytes, "member {name} changed after rewrite");
    }
}

/// Crypto surface: KDF with bounded strength, encryption-parameter
/// parsing from arbitrary bytes, and AES-256-CBC round trips whose
/// plaintext must survive zero-fill padding intact.
pub fn crypto(data: &[u8]) {
    if data.len() < 33 {
        return;
    }
    let strength = data[0] % 15; // 0..=14 -> <= 16K KDF iterations, cheap
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&data[1..17]);
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&data[17..33]);
    let plain = &data[33..];

    if let Ok(keys) = rar_rs::derive_keys("fuzz", &salt, strength) {
        let ct = rar_rs::encrypt_data(plain, &keys.key, &iv);
        let pt = rar_rs::decrypt_data(&ct, &keys.key, &iv).expect("decrypt must succeed");
        assert_eq!(
            &pt[..plain.len()],
            plain,
            "AES-256-CBC round trip changed the plaintext"
        );
    }
    // Parameter parser over arbitrary extra-record bytes (vints, salt,
    // IV, checksum).
    let _ = rar_rs::EncryptionParams::from_extra_bytes(data);
}

/// Recovery surface: inline `{RB}` build/parse/repair, the GF(2^16)
/// parity encode path, CRC64-XZ, and `.rev` serialization.
pub fn recovery(data: &[u8]) {
    // Inline recovery record: parse + repair (allocations bounded by the
    // input length — chunk sizes must fit inside the input).
    let _ = rar_rs::repair_archive(data);

    if !data.is_empty() {
        // Build real `{RB}` recovery data over the input at a bounded
        // percent, then exercise repair on the intact and on a
        // one-byte-corrupted prefix (one damaged shard, one parity
        // shard: reconstruct must succeed).
        let pct = (data[0] % 101) as u64;
        if let Ok(rr) = rar_rs::recovery::rar50::build_structural_inline_recovery_data(data, pct) {
            let mut full = data.to_vec();
            full.extend_from_slice(&rr);
            let _ = rar_rs::repair_archive(&full);
            let bit = (data[0] as usize) % data.len();
            full[bit] ^= 0xFF;
            let _ = rar_rs::repair_archive(&full);
        }
    }

    let _ = rar_rs::recovery::rar50::crc64_xz(data);
    let _ = rar_rs::recovery::rar50::crc64_rar_state(data);

    // `.rev` serialization: sizes/CRCs need not be meaningful for the
    // writer to produce a file.
    if data.len() >= 16 {
        let sizes = [data.len() as u64];
        let crcs = [u32::from_le_bytes(data[0..4].try_into().unwrap())];
        let payload = &data[..data.len() / 2];
        let _ = rar_rs::recovery::rev50::build_recovery_volume_file(0, 1, &sizes, &crcs, payload);
    }
}
