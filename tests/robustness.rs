//! Robustness tests converted from the former cargo-fuzz harnesses.
//!
//! The `fuzz/` tree (libFuzzer targets + corpus) was removed because
//! long-running fuzzing overheated the machine. Each former fuzz target is
//! now a deterministic, bounded `#[test]` over seeded pseudo-random inputs:
//! errors are expected and swallowed, and the goal is only to catch panics,
//! overflows, OOM aborts and unbounded loops.

use rar5::encryption;
use rar5::recovery::rar5 as recovery;

/// xorshift64* PRNG with a fixed seed, matching `tests/interop.rs`.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for byte in buf.iter_mut() {
            *byte = (self.next_u64() >> 32) as u8;
        }
    }
}

fn random_bytes(rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let len = (rng.next_u64() as usize) % (max_len + 1);
    let mut data = vec![0u8; len];
    rng.fill(&mut data);
    data
}

/// Former `archive_parse` fuzz target: parse a RAR5 archive and read every
/// member. Exercises signature detection, block scanning, vint decoding,
/// header parsing, table decoding, decompression, filters and integrity
/// checks.
#[test]
fn archive_parse_random_inputs_do_not_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fuzz-archive.rar");
    let mut rng = Rng::new(0x5EED_0001);

    let opts = rar5::ExtractOptions {
        safe_paths: true,
        max_unpacked_bytes: Some(64 * 1024 * 1024),
        max_total_unpacked_bytes: Some(128 * 1024 * 1024),
        ..Default::default()
    };

    for _ in 0..100 {
        let data = random_bytes(&mut rng, 256 * 1024);
        std::fs::write(&path, &data).expect("write archive input");

        let mut archive = match rar5::RarArchive::open(&path) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let names: Vec<String> = archive
            .namelist()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        for name in names {
            let _ = archive.read_with_options(&name, opts);
        }
    }
}

/// Former `unpack50_decode` fuzz target: RAR5 compressed member decode.
/// First 8 bytes become the unpacked size, byte 8 the dictionary log and
/// the rest the compressed bitstream.
#[test]
fn unpack50_decode_random_inputs_do_not_panic() {
    let mut rng = Rng::new(0x5EED_0002);

    for _ in 0..500 {
        let mut header = [0u8; 9];
        rng.fill(&mut header);
        let mut sz = [0u8; 8];
        sz.copy_from_slice(&header[..8]);
        let dict = header[8];
        // Keep the fuzzer's guard, but restrict the random domain so every
        // iteration actually reaches the decoder with bounded work: the
        // window is allocated up front from the unpacked size.
        let unpacked = u64::from_le_bytes(sz) & 0x00FF_FFFF;
        if unpacked > 256 * 1024 * 1024 {
            continue;
        }
        let stream = random_bytes(&mut rng, 1024 * 1024);
        let _ = rar5::codec::decode_standalone(&stream, unpacked, dict.min(13));
    }
}

/// Former `rar50_crypto` fuzz target: encryption-record parsing, PBKDF2 key
/// derivation, AES-256-CBC decrypt, password check and the hash-key MAC.
fn exercise_crypto(data: &[u8]) {
    if data.len() < 2 {
        return;
    }
    let with_checksum = data[0] & 1 != 0;
    // Cap the KDF exponent: full-strength PBKDF2 would dominate the test;
    // strength <= 8 exercises the same code paths.
    let strength = data[1] % 8;

    let mut extra: Vec<u8> = Vec::new();
    extra.extend_from_slice(&rar5::vint::encode(1)); // version
    extra.extend_from_slice(&rar5::vint::encode(if with_checksum { 0x02 } else { 0x00 })); // flags
    extra.push(strength);
    let mut salt = [0x42u8; 16];
    let mut iv = [0x24u8; 16];
    let mut check = [0xAAu8; 12];
    let salt_len = salt.len();
    let iv_len = iv.len();
    let check_len = check.len();
    if data.len() > 2 + salt_len {
        salt.copy_from_slice(&data[2..2 + salt_len]);
    }
    if data.len() > 2 + salt_len + iv_len {
        iv.copy_from_slice(&data[2 + salt_len..2 + salt_len + iv_len]);
    }
    if data.len() > 2 + salt_len + iv_len + check_len && with_checksum {
        check.copy_from_slice(&data[2 + salt_len + iv_len..2 + salt_len + iv_len + check_len]);
    }
    extra.extend_from_slice(&salt);
    extra.extend_from_slice(&iv);
    if with_checksum {
        extra.extend_from_slice(&check);
    }

    if let Ok(params) = encryption::EncryptionParams::from_extra_bytes(&extra) {
        let ciphertext = &data[data.len().min(2 + 128)..];
        let ciphertext = &ciphertext[..ciphertext.len().min(1 << 16)];
        let _ = params.verify_password("");
        let _ = params.decrypt(ciphertext, "");
        if let Ok(keys) = params.derive_keys("")
            && ciphertext.len() >= 16
            && ciphertext.len().is_multiple_of(16)
        {
            let _ = encryption::decrypt_data(ciphertext, &keys.key, &iv);
        }
    }
}

#[test]
fn rar50_crypto_random_inputs_do_not_panic() {
    // Regression inputs found by the former fuzzer (slow units).
    exercise_crypto(&[0x0a, 0xb8, 0x24, 0x7a]);
    exercise_crypto(&[0x90, 0x78]);

    let mut rng = Rng::new(0x5EED_0003);
    for _ in 0..1000 {
        let data = random_bytes(&mut rng, 64 * 1024);
        exercise_crypto(&data);
    }
}

/// Former `rar5_recovery` fuzz target: inline recovery-record parse +
/// repair, GF(16) parity codec and crc64 hashing.
#[test]
fn rar5_recovery_random_inputs_do_not_panic() {
    let mut rng = Rng::new(0x5EED_0004);

    for _ in 0..500 {
        let data = random_bytes(&mut rng, 256 * 1024);
        if data.len() > 8 {
            let _ = recovery::crc64_xz(&data);
            let _ = recovery::crc64_rar_state(&data);
        }
        let _ = recovery::repair_inline_recovery_archive(&data);
    }
}
