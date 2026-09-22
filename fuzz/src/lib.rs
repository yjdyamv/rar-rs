//! Shared harness for the rar-rs fuzz targets.
//!
//! Each target is a `fn(&[u8])` runner (no panics tolerated — a panic is
//! a bug the fuzzer is looking for). The same runners drive libFuzzer
//! (`#[cfg(fuzzing)]`) and the standalone mutation loop (`standalone`),
//! so the targets run on stable Rust without libFuzzer.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../crates/rar/tests/support/structured.rs"]
pub mod structured;

/// Deterministic xorshift64* PRNG (no external deps).
///
/// [`Rng::from_input`] additionally consumes the fuzz input bytes as the
/// primary randomness source, so libFuzzer mutations steer the structured
/// mutators field by field instead of being flattened through a 64-bit seed
/// hash; the xorshift stream takes over once the input runs out.
pub struct Rng {
    state: u64,
    input: Vec<u8>,
    cursor: usize,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng {
            state: seed | 1,
            input: Vec::new(),
            cursor: 0,
        }
    }

    /// PRNG backed by the fuzz input: every `next_u64` draws up to eight
    /// bytes from `data` first, so a mutated input byte changes the
    /// structured decisions that follow it. Falls back to the xorshift
    /// stream (seeded from the input hash) when `data` is short or
    /// exhausted, keeping short/empty inputs deterministic.
    pub fn from_input(data: &[u8]) -> Self {
        Rng {
            state: structured::seed_from_bytes(data) | 1,
            input: data.to_vec(),
            cursor: 0,
        }
    }

    fn xorshift(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        for byte in &mut bytes {
            *byte = if self.cursor < self.input.len() {
                let value = self.input[self.cursor];
                self.cursor += 1;
                value
            } else {
                self.xorshift() as u8
            };
        }
        u64::from_le_bytes(bytes)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for byte in buf.iter_mut() {
            *byte = self.next_u64() as u8;
        }
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

/// Genuine WinRAR RAR 7.23 archive with a `-rr5%` inline recovery record
/// (`{RB}` chunks with WinRAR's own plan/geometry).
pub static CORPUS_RR: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../crates/rar/tests/fixtures/rar50/winrar5_with_recovery_rr5.rar"
));

/// Small genuine legacy archives: RAR 1.4, RAR 2.0, a WinRAR 5.91 STORE
/// RAR4 archive, a RAR 3.0 compressed archive and a RAR 2.5 `PROTECT_HEAD`
/// recovery-record archive. The `legacy` target mutates their block
/// envelopes with the 16-bit header CRC recomputed where it is verified.
pub static CORPUS_LEGACY: &[&[u8]] = &[
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/rar/tests/fixtures/rar13/MULTIFIL.RAR"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/rar/tests/fixtures/rar40/rar2/rar20.rar"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/rar/tests/fixtures/rar40/winrar591_store_m0.rar"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/rar/tests/fixtures/rar40/rar300/compressed_text_rar300.rar"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/rar/tests/fixtures/rar40/repair/rar250_protect_head_rr1.rar"
    )),
];

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
/// (default 0x5EED_0001). Returns the number of iterations run.
pub fn standalone(name: &str, seeds: &[&[u8]], runner: fn(&[u8])) -> usize {
    standalone_with(name, seeds, runner, 200_000)
}

/// Like [`standalone`], with a target-specific default iteration count
/// (write-side targets do real file I/O per iteration and default lower;
/// `FUZZ_ITERATIONS` always overrides). Returns the number of iterations
/// run so callers can assert target-specific coverage floors.
pub fn standalone_with(
    name: &str,
    seeds: &[&[u8]],
    runner: fn(&[u8]),
    default_iterations: usize,
) -> usize {
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
    iterations
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
                let _ = a.read_entry_with_options(id, opts.clone());
            }
        }
        let _ = a.extract_all_with_options(dir.path().join("x"), opts.clone());
    }
    // Password path: also walks the header-encryption scan. A random
    // input almost never forms a valid block envelope, so the KDF is
    // effectively never hit with hostile strength here; the crypto
    // target covers bounded-strength KDF directly.
    if let Ok(mut a) =
        rar_rs::ArchiveReader::open_with(&path, rar_rs::OpenOptions::new().password("fuzz"))
    {
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

/// Standalone coverage counters for [`write_roundtrip`]. The corpus control
/// bytes can freeze one option combination across iterations, so a run could
/// pass while the multi-volume and rv/rc paths never executed; the `write`
/// binary prints these after the loop via [`report_write_coverage`] and
/// asserts floors.
static WRITE_RUNS: AtomicUsize = AtomicUsize::new(0);
static WRITE_CREATED: AtomicUsize = AtomicUsize::new(0);
static WRITE_MULTIVOLUME: AtomicUsize = AtomicUsize::new(0);
static WRITE_RECOVERY: AtomicUsize = AtomicUsize::new(0);

/// Print the write target's standalone coverage counters and assert the run
/// actually reached the create, multi-volume and rv/rc paths. Called once by
/// the `write` binary after the mutation loop; `iterations` is the number of
/// runner invocations the loop performed.
pub fn report_write_coverage(iterations: usize) {
    let runs = WRITE_RUNS.swap(0, Ordering::Relaxed);
    let created = WRITE_CREATED.swap(0, Ordering::Relaxed);
    let multivolume = WRITE_MULTIVOLUME.swap(0, Ordering::Relaxed);
    let recovery = WRITE_RECOVERY.swap(0, Ordering::Relaxed);
    eprintln!(
        "write coverage: {created}/{runs} valid inputs created archives, {multivolume} multi-volume, \
         {recovery} rv/rc rebuilds ({iterations} iterations)"
    );
    assert!(
        created > 0,
        "write target created no archives in {iterations} iterations"
    );
    assert!(
        multivolume >= iterations / 10,
        "write multi-volume coverage starved: {multivolume} multi-volume creations over \
         {iterations} iterations"
    );
    assert!(
        recovery >= multivolume / 2,
        "write rv/rc coverage starved: {recovery} rebuilds over {multivolume} multi-volume creations"
    );
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
    WRITE_RUNS.fetch_add(1, Ordering::Relaxed);
    let h = &data[8..]; // control bytes double as payload seeds
    let n_members = 1 + (h[0] % 3) as usize; // 1..=3
    // Multi-volume sets are exactly two volumes per member (member =
    // 2x volume) so chunk splits, per-chunk records and CBC chains get
    // exercised with minimal per-iteration file churn (Windows per-file
    // overhead dominates the loop cost).
    let (member_bytes, volume_size): (usize, Option<u64>) = match h[1] % 3 {
        0 => (2048, None),       // single volume
        1 => (4096, Some(2048)), // two 2 KiB volumes
        _ => (8192, Some(4096)), // two 4 KiB volumes
    };
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
    // Header encryption needs a password; h[5] % 4 == 2 is even, so the
    // password branch below always runs with it.
    let encrypt_headers = h[5] % 4 == 2;
    let mut opts = rar_rs::WriterOptions::default()
        .solid_mode(if h[3].is_multiple_of(2) {
            rar_rs::SolidMode::Continuous
        } else {
            rar_rs::SolidMode::Disabled
        })
        .blake2(h[4].is_multiple_of(2))
        // Quick-open is rejected with data volumes and with header
        // encryption, so derive it only when neither is selected. Without
        // this gate most multi-volume inputs died in validation and the
        // rv/rc rebuild path below never ran.
        .quick_open(h[4] % 4 < 3 && !multivolume && !encrypt_headers);
    if h[5].is_multiple_of(2) {
        opts = opts.password("fuzz");
    }
    if encrypt_headers {
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
    let mut rar = match rar_rs::ArchiveWriter::create_with(&arc, opts) {
        Ok(rar) => rar,
        Err(rar_rs::RarError::InvalidOption(_)) => return, // derived combo rejected
        Err(err) => panic!("create_with failed with a non-InvalidOption error: {err}"),
    };
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
        rar.add_bytes(name, payload, entry_opts)
            .unwrap_or_else(|err| panic!("add_bytes({name}) failed: {err}"));
    }
    rar.finish()
        .unwrap_or_else(|err| panic!("finish failed: {err}"));
    WRITE_CREATED.fetch_add(1, Ordering::Relaxed);
    if multivolume {
        WRITE_MULTIVOLUME.fetch_add(1, Ordering::Relaxed);
    }

    // Round trip: read every member back and compare byte-for-byte. A
    // successful create must be readable; an open/locate/read failure
    // here is a defect, not an unreachable option combination (only an
    // `InvalidOption` from `create_with` may skip the rest).
    let volumes = rar_rs::discover_volumes(&arc);
    let mut failures: Vec<String> = Vec::new();
    let opened = if h[5].is_multiple_of(2) {
        rar_rs::ArchiveReader::open_with(&volumes[0], rar_rs::OpenOptions::new().password("fuzz"))
    } else {
        rar_rs::ArchiveReader::open(&volumes[0])
    };
    match opened {
        Ok(mut ar) => {
            for (name, payload) in &members {
                match ar.unique_entry(name) {
                    Ok(id) => match ar.read_entry(id) {
                        Ok(got) if got == *payload => {}
                        Ok(_) => failures.push(format!("round trip mismatch for {name}")),
                        Err(err) => failures.push(format!("read_entry({name}): {err}")),
                    },
                    Err(err) => failures.push(format!("unique_entry({name}): {err}")),
                }
            }
        }
        Err(err) => failures.push(format!("open after create: {err}")),
    }

    // rv/rc: multi-volume sets get .rev (either from create time or the
    // standalone rv path), a middle volume is deleted, rebuild must
    // reproduce it byte-for-byte. Bounded to modest sets — huge volume
    // counts would make the loop file-churn bound explode.
    if multivolume && volumes.len() < 2 {
        failures.push(format!(
            "multi-volume create produced {} volume(s)",
            volumes.len()
        ));
    }
    if multivolume && (2..=8).contains(&volumes.len()) {
        let rev_ok = if create_rev.is_some() {
            true // create-time .rev already on disk
        } else {
            match rar_rs::build_recovery_volumes_for_set(&volumes, 1 + (h[7] as usize % 2)) {
                Ok(_) => true,
                Err(err) => {
                    failures.push(format!("build_recovery_volumes_for_set: {err}"));
                    false
                }
            }
        };
        if rev_ok {
            WRITE_RECOVERY.fetch_add(1, Ordering::Relaxed);
            let victim = volumes[volumes.len() / 2].clone();
            match std::fs::read(&victim) {
                Ok(orig) => {
                    let _ = std::fs::remove_file(&victim);
                    match rar_rs::rebuild_missing_volumes(&volumes[0]) {
                        Ok(rebuilt) => {
                            if !rebuilt.contains(&victim) {
                                failures
                                    .push(format!("rebuild did not report {}", victim.display()));
                            }
                            match std::fs::read(&victim) {
                                Ok(bytes) if bytes == orig => {}
                                Ok(_) => failures
                                    .push(format!("rc rebuild mismatch for {}", victim.display())),
                                Err(err) => failures.push(format!(
                                    "rebuilt volume {} unreadable: {err}",
                                    victim.display()
                                )),
                            }
                        }
                        Err(err) => failures.push(format!("rebuild_missing_volumes: {err}")),
                    }
                }
                Err(err) => {
                    failures.push(format!("read victim volume {}: {err}", victim.display()))
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "write round-trip failures: {failures:#?}"
    );
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
        let mut rar =
            rar_rs::ArchiveWriter::append_with(&path, rar_rs::AppendOptions::default()).unwrap();
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
        let mut rar = rar_rs::ArchiveReader::open(&path).unwrap();
        assert_eq!(
            rar.comment().unwrap().as_deref(),
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

    if let Ok(keys) = rar_rs::wire::derive_keys("fuzz", &salt, strength) {
        let ct = rar_rs::wire::encrypt_data(plain, &keys.key, &iv);
        let pt = rar_rs::wire::decrypt_data(&ct, &keys.key, &iv).expect("decrypt must succeed");
        assert_eq!(
            &pt[..plain.len()],
            plain,
            "AES-256-CBC round trip changed the plaintext"
        );
    }
    // Parameter parser over arbitrary extra-record bytes (vints, salt,
    // IV, checksum).
    let _ = rar_rs::wire::EncryptionParams::from_extra_bytes(data);
}

/// Recovery surface: inline `{RB}` build/parse/repair, the GF(2^16)
/// parity encode path, CRC64-XZ, and `.rev` serialization. Beyond raw
/// bytes, a structured strategy builds *valid* records with the wire API
/// and mutates their plan/geometry with the checksums recomputed, so the
/// shard arithmetic and relocation scan actually run (random bytes cannot
/// pass the CRC64-XZ gate).
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
        if let Ok(rr) = rar_rs::wire::build_structural_inline_recovery_data(data, pct) {
            let mut full = data.to_vec();
            full.extend_from_slice(&rr);
            let _ = rar_rs::repair_archive(&full);
            let bit = (data[0] as usize) % data.len();
            full[bit] ^= 0xFF;
            let _ = rar_rs::repair_archive(&full);
        }
    }

    // Structured `{RB}` mutations: valid records over an archive-shaped
    // prefix, then plan/geometry fields, shard states and parity flipped
    // with the CRC64-XZ recomputed (plus truncations). This is what
    // reaches `split_prefix_shards`, the relocation scan and the
    // Reed-Solomon solve.
    let mut rng = Rng::new(structured::seed_from_bytes(data));
    let prefix_len = 64 + rng.below(CORPUS_WINRAR.len().saturating_sub(64).max(1));
    let prefix = &CORPUS_WINRAR[..prefix_len.min(CORPUS_WINRAR.len())];
    let pct = u64::from(data.first().copied().unwrap_or(2) % 20) + 1;
    for (case, _) in structured::inline_rr_cases(prefix, pct, &mut rng) {
        let _ = rar_rs::repair_archive(&case);
    }

    // A genuine WinRAR-produced RR archive: repair it, then mutate the real
    // chunk fields/states with the CRC recomputed. Gated on a larger input
    // so the per-iteration cost stays close to the raw loop's.
    if data.len() >= 256 {
        let _ = rar_rs::repair_archive(CORPUS_RR);
        for (case, _) in structured::rr_archive_mutations(CORPUS_RR, &mut rng) {
            let _ = rar_rs::repair_archive(&case);
        }
    }

    let _ = rar_rs::wire::crc64_xz(data);
    let _ = rar_rs::wire::crc64_rar_state(data);

    // `.rev` serialization: sizes/CRCs need not be meaningful for the
    // writer to produce a file.
    if data.len() >= 16 {
        let sizes = [data.len() as u64];
        let crcs = [u32::from_le_bytes(data[0..4].try_into().unwrap())];
        let payload = &data[..data.len() / 2];
        let _ = rar_rs::wire::build_recovery_volume_file(0, 1, &sizes, &crcs, payload);
    }
}

/// Recovery-volume / streaming-repair surface: `repair_archive_path` on a
/// structured inline record, fabricated REV5 sets driven through
/// `rebuild_missing_volumes` (header CRC recomputed after mutation), and
/// rev3 `.rev` sets built with the public API and mutated at the trailer
/// (CRC32 recomputed) before a volume is taken away. Reaches the shard
/// math and the `.rev` naming/layout discovery.
pub fn rev(data: &[u8]) {
    let mut rng = Rng::from_input(data);
    let dir = tempfile::tempdir().expect("tempdir");

    // Streaming repair of one structured inline-RR case (a different code
    // path from the in-memory repair in `recovery`).
    let prefix_len = 64 + rng.below(CORPUS_WINRAR.len().saturating_sub(64).max(1));
    let prefix = &CORPUS_WINRAR[..prefix_len.min(CORPUS_WINRAR.len())];
    let pct = u64::from(data.first().copied().unwrap_or(2) % 20) + 1;
    let cases = structured::inline_rr_cases(prefix, pct, &mut rng);
    if !cases.is_empty() {
        let pick = rng.below(cases.len());
        let src = dir.path().join("inline.rar");
        let dst = dir.path().join("inline.fixed.rar");
        if std::fs::write(&src, &cases[pick].0).is_ok() {
            let _ = rar_rs::repair_archive_path(&src, &dst);
        }
    }

    rev5_fabricated(dir.path(), data, &mut rng);
    rev3_fabricated(dir.path(), data, &mut rng);
}

/// Fabricate a small REV5 volume set, write its `.rev` (mutated with the
/// header CRC recomputed), remove a middle volume and run `rar rc`.
fn rev5_fabricated(dir: &std::path::Path, data: &[u8], rng: &mut Rng) {
    let width = 1 + rng.below(3);
    let data_count = 2 + rng.below(3); // 2..=4
    let rec_count = 1 + rng.below(2); // 1..=2
    // Zeroed volumes (zero parity) make a *successful* rebuild; random
    // volumes run the same solve and fail the recorded-CRC check after it.
    let zeroed = data.first().is_none_or(|byte| byte & 1 == 0);
    let size = 2 * (1 + rng.below(96));
    let base = "set";
    let volume_path = |index: usize| dir.join(format!("{base}.part{:0width$}.rar", index + 1));

    let mut sizes = Vec::with_capacity(data_count);
    let mut crcs = Vec::with_capacity(data_count);
    for index in 0..data_count {
        let mut bytes = vec![0u8; size];
        if !zeroed {
            rng.fill(&mut bytes);
        }
        crcs.push(structured::crc32_ieee(&bytes));
        sizes.push(bytes.len() as u64);
        let _ = std::fs::write(volume_path(index), &bytes);
    }
    let mut payload = vec![0u8; size];
    if !zeroed {
        rng.fill(&mut payload);
    }

    for k in 0..rec_count {
        let file = rar_rs::wire::build_recovery_volume_file(k, rec_count, &sizes, &crcs, &payload);
        let path = dir.join(format!("{base}.part{:0width$}.rev", k + 1));
        let bytes = if k == 0 {
            let mut cases = structured::rev5_mutations(&file, rng);
            cases.push((file, "rev5 as built"));
            let pick = rng.below(cases.len());
            cases.swap_remove(pick).0
        } else {
            file
        };
        let _ = std::fs::write(&path, &bytes);
    }

    let missing = 1 + rng.below(data_count - 1); // never the first volume
    let _ = std::fs::remove_file(volume_path(missing));
    let _ = rar_rs::rebuild_missing_volumes(&volume_path(0));
}

/// Fabricate a small legacy RAR4 volume set, build its rev3 `.rev` with the
/// public API, mutate the trailer (CRC32 recomputed) when the layout has
/// one, remove a middle volume and run `rar rc` (which walks
/// `collect_recovery_volumes` and the name/layout scoring).
fn rev3_fabricated(dir: &std::path::Path, data: &[u8], rng: &mut Rng) {
    let control = data.first().copied().unwrap_or(0);
    let new_naming = control & 2 != 0;
    let trailer = control & 4 != 0;
    let entry_from_rev = control & 8 != 0;
    let data_count = 2 + rng.below(2); // 2..=3
    let rec_count = 1 + rng.below(2); // 1..=2
    let size = 32 + 2 * rng.below(64);
    let base = "set";

    let mut paths = Vec::with_capacity(data_count);
    for index in 0..data_count {
        let path = if new_naming {
            dir.join(format!("{base}.part{}.rar", index + 1))
        } else if index == 0 {
            dir.join(format!("{base}.rar"))
        } else {
            dir.join(format!("{base}.r{:02}", index - 1))
        };
        let mut bytes = vec![0u8; size];
        if index == 0 {
            // Dispatch to the legacy codec: `collect_recovery_volumes`
            // recognises the set from the first volume's signature.
            bytes[..7].copy_from_slice(b"Rar!\x1a\x07\x00");
        }
        rng.fill(&mut bytes[7..size.saturating_sub(7)]);
        if !trailer {
            // Non-zero tail selects the legacy name-encoded layout.
            bytes[size - 1] = 0x5a;
        }
        let _ = std::fs::write(&path, &bytes);
        paths.push(path);
    }

    let Ok(revs) = rar_rs::build_recovery_volumes_for_set(&paths, rec_count) else {
        return;
    };
    if trailer
        && let Some(rev) = revs.first()
        && let Ok(bytes) = std::fs::read(rev)
    {
        let mut cases = structured::rev3_trailer_mutations(&bytes, rng);
        cases.push((bytes, "rev3 as built"));
        let pick = rng.below(cases.len());
        let _ = std::fs::write(rev, &cases[pick].0);
    }

    let missing = 1 + rng.below(data_count - 1);
    let _ = std::fs::remove_file(&paths[missing]);
    let entry = if entry_from_rev {
        revs.first().cloned().unwrap_or_else(|| paths[0].clone())
    } else {
        paths[0].clone()
    };
    let _ = rar_rs::rebuild_missing_volumes(&entry);
}

/// Legacy block-envelope surface: mutate the headers of small genuine RAR4
/// and RAR13 archives (recomputing the 16-bit RAR4 header CRC and the RAR13
/// rolling member checksum where needed) and run the full read path —
/// signature scan, block walk, header parse, member decode and the legacy
/// recovery-record scan.
pub fn legacy(data: &[u8]) {
    let mut rng = Rng::from_input(data);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("in.rar");
    let fixed = dir.path().join("fixed.rar");
    let opts = rar_rs::ExtractOptions {
        safe_paths: true,
        max_unpacked_bytes: Some(8 * 1024 * 1024),
        max_total_unpacked_bytes: Some(16 * 1024 * 1024),
        ..Default::default()
    };

    // Round-robin the per-input case budget across seeds: with a single
    // shared budget the first two seeds' ~64 cases consumed everything, so
    // the winrar591/rar300/rar250 seeds — including the `PROTECT_HEAD`
    // recovery-repair exercise below — never ran. `executed` makes any
    // future starvation loud instead of silent.
    const CASES_PER_INPUT: usize = 64;
    let per_seed: Vec<Vec<structured::Case>> = CORPUS_LEGACY
        .iter()
        .map(|seed| structured::legacy_block_cases(seed, &mut rng))
        .collect();
    let mut executed = vec![0usize; per_seed.len()];
    let mut budget = CASES_PER_INPUT;
    let mut round = 0usize;
    'rounds: loop {
        let mut progressed = false;
        for (seed_index, cases) in per_seed.iter().enumerate() {
            let Some((case, _)) = cases.get(round) else {
                continue;
            };
            progressed = true;
            if budget == 0 {
                break 'rounds;
            }
            budget -= 1;
            executed[seed_index] += 1;
            if std::fs::write(&path, case).is_err() {
                continue;
            }
            if let Ok(mut reader) = rar_rs::ArchiveReader::open(&path) {
                let names: Vec<String> = reader
                    .entries()
                    .map(|entry| entry.name().to_string())
                    .collect();
                for name in names.iter().take(4) {
                    if let Some(id) = reader.entries_named(name).next().map(|entry| entry.id()) {
                        let _ = reader.read_entry_with_options(id, opts.clone());
                    }
                }
            }
            // The RAR 2.5 `PROTECT_HEAD` seed exercises the legacy
            // recovery scan/repair on the mutated header stream.
            if CORPUS_LEGACY[seed_index].starts_with(b"Rar!\x1a\x07\x00") {
                let _ = rar_rs::repair_legacy_archive_path(&path, &fixed);
            }
        }
        if !progressed {
            break;
        }
        round += 1;
    }
    // Coverage report, off in normal runs (`FUZZ_LEGACY_COUNTS=1` opts in).
    if std::env::var_os("FUZZ_LEGACY_COUNTS").is_some() {
        static REPORT: std::sync::Once = std::sync::Once::new();
        REPORT.call_once(|| eprintln!("legacy per-seed cases: {executed:?}"));
    }
    assert!(
        executed.iter().all(|&count| count > 0),
        "legacy seed starvation: per-seed case counts {executed:?}"
    );
}
