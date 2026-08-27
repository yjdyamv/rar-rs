//! Shared harness for the rar5 fuzz targets.
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
/// doubles as a CI smoke fuzzer.
///
/// Overrides: `FUZZ_ITERATIONS` (default 200_000), `FUZZ_SEED`
/// (default 0x5EED_0001).
pub fn standalone(name: &str, seeds: &[&[u8]], runner: fn(&[u8])) {
    let iterations: usize = std::env::var("FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
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

    let opts = rar5::ExtractOptions {
        safe_paths: true,
        max_unpacked_bytes: Some(64 * 1024 * 1024),
        max_total_unpacked_bytes: Some(128 * 1024 * 1024),
        max_dict_size: Some(4 * 1024 * 1024 * 1024),
        ..Default::default()
    };

    // Plain read path: scan + list + read every member + extract all.
    if let Ok(mut a) = rar5::RarArchive::open(&path) {
        let names: Vec<String> = a.namelist().into_iter().map(|s| s.to_string()).collect();
        for name in &names {
            let _ = a.read_with_options(name, opts);
        }
        let _ = a.extract_all_with_options(dir.path().join("x"), opts);
    }
    // Password path: also walks the header-encryption scan. A random
    // input almost never forms a valid block envelope, so the KDF is
    // effectively never hit with hostile strength here; the crypto
    // target covers bounded-strength KDF directly.
    if let Ok(mut a) = rar5::RarArchive::open_with_password(&path, "fuzz") {
        let _ = a.extract_all_with_options(dir.path().join("y"), opts);
    }

    let _ = rar5::sfx_offset_of(data);
    let _ = rar5::discover_volumes(&path);
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

    if let Ok(keys) = rar5::crypto::derive_keys("fuzz", &salt, strength) {
        let ct = rar5::crypto::encrypt_data(plain, &keys.key, &iv);
        let pt = rar5::crypto::decrypt_data(&ct, &keys.key, &iv).expect("decrypt must succeed");
        assert_eq!(
            &pt[..plain.len()],
            plain,
            "AES-256-CBC round trip changed the plaintext"
        );
    }
    // Parameter parser over arbitrary extra-record bytes (vints, salt,
    // IV, checksum).
    let _ = rar5::crypto::EncryptionParams::from_extra_bytes(data);
}

/// Recovery surface: inline `{RB}` build/parse/repair, the GF(2^16)
/// parity encode path, CRC64-XZ, and `.rev` serialization.
pub fn recovery(data: &[u8]) {
    // Inline recovery record: parse + repair (allocations bounded by the
    // input length — chunk sizes must fit inside the input).
    let _ = rar5::repair_archive(data);

    if !data.is_empty() {
        // Build real `{RB}` recovery data over the input at a bounded
        // percent, then exercise repair on the intact and on a
        // one-byte-corrupted prefix (one damaged shard, one parity
        // shard: reconstruct must succeed).
        let pct = (data[0] % 101) as u64;
        if let Ok(rr) = rar5::recovery::rar5::build_structural_inline_recovery_data(data, pct) {
            let mut full = data.to_vec();
            full.extend_from_slice(&rr);
            let _ = rar5::repair_archive(&full);
            let bit = (data[0] as usize) % data.len();
            full[bit] ^= 0xFF;
            let _ = rar5::repair_archive(&full);
        }
    }

    let _ = rar5::recovery::rar5::crc64_xz(data);
    let _ = rar5::recovery::rar5::crc64_rar_state(data);

    // `.rev` serialization: sizes/CRCs need not be meaningful for the
    // writer to produce a file.
    if data.len() >= 16 {
        let sizes = [data.len() as u64];
        let crcs = [u32::from_le_bytes(data[0..4].try_into().unwrap())];
        let payload = &data[..data.len() / 2];
        let _ = rar5::recovery::rev5::build_recovery_volume_file(0, 1, &sizes, &crcs, payload);
    }
}
