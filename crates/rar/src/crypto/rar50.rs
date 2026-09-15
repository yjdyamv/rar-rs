//! RAR5 encryption: AES-256-CBC, the chained HMAC-SHA256 key-derivation
//! function, and the hash-key MAC that protects encrypted members' checksums.
//!
//! Ported from the `rars` project (<https://github.com/bitplane/rars>), licensed
//! MIT OR Apache-2.0, at upstream revision `c08a17b`. See NOTICE for
//! attribution and the unresolved workspace-metadata/COPYING difference.

use crate::error::{RarError, RarResult};
use crate::format::rar5::vint;
/// RAR5 Encryption Support
///
/// RAR5 uses AES-256 in CBC mode with keys derived from a password via a
/// chained HMAC-SHA256 KDF (equivalent to PBKDF2-HMAC-SHA256):
///
/// 1. Key derivation: one HMAC chain produces the AES key (at 2^strength
///    iterations), the 32-byte hash key (16 iterations later, used to MAC
///    checksums of encrypted data) and the password check value (another
///    16 iterations later, XOR-folded to 8 bytes).
/// 2. IV: 16-byte random initialization vector per file.
/// 3. Padding: zero-fill to a 16-byte AES block boundary.
/// 4. Header encryption: when an archive-level encryption header is
///    present, all subsequent blocks (including file headers) are also
///    encrypted.
use crate::format::rar5::{
    ENCR_FLAG_CHECKSUM, ENCR_FLAG_HASH_MAC, ENCR_IV_SIZE, ENCR_KEY_SIZE, ENCR_SALT_SIZE,
    ENCR_VERSION_AES256, EXTRA_FILE_ENCRYPTION,
};

use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha256 = Hmac<Sha256>;

/// Maximum accepted KDF strength exponent (2^24 iterations). Larger values
/// are rejected at parse time to prevent CPU denial-of-service.
pub const MAX_KDF_COUNT_LOG: u8 = 24;

/// The defined bits of the encryption-record `flags` field. Unknown bits are
/// rejected at parse time: a vint can carry more than 8 bits, and truncating
/// it to `u8` (e.g. `0x102` -> `0x02`) would silently flip `uses_hash_mac`.
const ENCR_FLAGS_KNOWN: u64 = (ENCR_FLAG_CHECKSUM | ENCR_FLAG_HASH_MAC) as u64;

/// Reject an encryption-record `flags` value with undefined bits set.
fn check_encr_flags(flags: u64) -> RarResult<()> {
    if flags & !ENCR_FLAGS_KNOWN != 0 {
        return Err(RarError::Format(format!(
            "unsupported encryption flags {flags:#x} (known bits {ENCR_FLAGS_KNOWN:#x})"
        )));
    }
    Ok(())
}

/// Constant-time byte-slice comparison.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Keys derived from a password by the RAR5 KDF.
///
/// Zeroized on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct DerivedKeys {
    /// AES-256 data/header encryption key.
    pub key: [u8; ENCR_KEY_SIZE],
    /// Key used to MAC stored checksums of encrypted data.
    pub hash_key: [u8; ENCR_KEY_SIZE],
    /// 8-byte XOR-folded password check value.
    pub password_check: [u8; 8],
}

impl DerivedKeys {
    /// MAC a CRC32 value with the hash key (RAR5 encrypted-file checksums).
    pub fn mac_crc32(&self, crc: u32) -> u32 {
        let digest = hmac_sha256(&self.hash_key, &crc.to_le_bytes());
        digest
            .as_chunks::<4>()
            .0
            .iter()
            .fold(0, |acc, chunk| acc ^ u32::from_le_bytes(*chunk))
    }

    /// MAC a 32-byte hash with the hash key (RAR5 encrypted-file hashes).
    pub fn mac_hash32(&self, hash: [u8; 32]) -> [u8; 32] {
        let digest = hmac_sha256(&self.hash_key, &hash);
        let mut out = [0u8; 32];
        for (slot, chunk) in out
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(digest.as_chunks::<4>().0)
        {
            *slot = u32::from_le_bytes(*chunk).to_le_bytes();
        }
        out
    }

    /// Verify the 12-byte stored password check value.
    pub fn check_password(&self, stored: &[u8; 12]) -> bool {
        let checksum = Sha256::digest(&stored[..8]);
        constant_time_eq(&self.password_check, &stored[..8])
            && constant_time_eq(&checksum[..4], &stored[8..12])
    }

    /// Verify the stored check value of a *service* record.
    ///
    /// RAR 5.21 and earlier wrote an all-zero `PswCheck` in service records
    /// ("STM" items) even when the checksum flag was set; UnRAR treats that
    /// value as "no check data" and accepts any password. Member records keep
    /// the strict [`Self::check_password`] comparison.
    pub fn check_password_service(&self, stored: &[u8; 12]) -> bool {
        if stored[..8] == [0u8; 8] {
            return true;
        }
        self.check_password(stored)
    }
}

/// Derive key material from `password` using the RAR5 chained KDF.
///
/// Runs a single HMAC chain of `2^strength + 32` iterations and slices the
/// XOR-folded accumulators at `2^strength`, `2^strength + 16` and
/// `2^strength + 32`, matching WinRAR's derivation of key, hash key and
/// password check value.
pub fn derive_keys(
    password: &str,
    salt: &[u8; ENCR_SALT_SIZE],
    strength: u8,
) -> RarResult<DerivedKeys> {
    if strength > MAX_KDF_COUNT_LOG {
        return Err(RarError::Format(format!(
            "KDF strength {strength} exceeds maximum {MAX_KDF_COUNT_LOG}"
        )));
    }

    // WinRAR copies the password into a 127-wchar_t buffer before running the
    // KDF (see `clamp_password`). Without the clamp the two sides derive
    // different keys for longer passwords and cannot open each other's
    // archives.
    let password = crate::crypto::clamp_password(password.as_bytes());

    let mut first_input = Vec::with_capacity(salt.len() + 4);
    first_input.extend_from_slice(salt);
    first_input.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_sha256(password, &first_input);
    let mut accumulator = u;
    let mut taps = [[0u8; ENCR_KEY_SIZE]; 3];
    let mut iterations = (1u32 << strength) - 1;

    for tap in &mut taps {
        for _ in 0..iterations {
            u = hmac_sha256(password, &u);
            for (acc, byte) in accumulator.iter_mut().zip(u) {
                *acc ^= byte;
            }
        }
        *tap = accumulator;
        iterations = 16;
    }

    let mut password_check = [0u8; 8];
    for (i, byte) in password_check.iter_mut().enumerate() {
        *byte = taps[2][i] ^ taps[2][i + 8] ^ taps[2][i + 16] ^ taps[2][i + 24];
    }

    let result = DerivedKeys {
        key: taps[0],
        hash_key: taps[1],
        password_check,
    };
    u.zeroize();
    accumulator.zeroize();
    taps.zeroize();
    Ok(result)
}

/// Derive a 32-byte AES-256 key with the PBKDF2-HMAC-SHA256 fallback.
///
/// Test oracle for the production KDF (`derive_keys`): the fallback mirrors
/// the legacy single-block derivation, not the RAR5 tap chain.
#[cfg(test)]
pub fn derive_key(password: &str, salt: &[u8], iterations: u32) -> [u8; ENCR_KEY_SIZE] {
    let mut key = [0u8; ENCR_KEY_SIZE];
    pbkdf2_fallback(password, salt, iterations, &mut key);
    key
}

#[cfg(test)]
fn pbkdf2_fallback(password: &str, salt: &[u8], iterations: u32, out: &mut [u8; 32]) {
    // Legacy compatibility helper: PBKDF2-HMAC-SHA256 block 1.
    let mut first_input = Vec::with_capacity(salt.len() + 4);
    first_input.extend_from_slice(salt);
    first_input.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password.as_bytes(), &first_input);
    let mut acc = u;
    for _ in 1..iterations {
        u = hmac_sha256(password.as_bytes(), &u);
        for (a, b) in acc.iter_mut().zip(u) {
            *a ^= b;
        }
    }
    out.copy_from_slice(&acc);
    u.zeroize();
    acc.zeroize();
}
// ── Encryption / Decryption ──────────────────────────────────────────────────

/// AES-256-CBC state for RAR5, whose IV advances block by block and whose
/// zero-fill padding is handled by the caller.
#[derive(ZeroizeOnDrop)]
struct Aes256Cbc {
    cipher: Aes256,
    iv: [u8; 16],
}

impl Aes256Cbc {
    fn new(key: &[u8; 32], iv: &[u8; 16]) -> Self {
        Self {
            cipher: Aes256::new(key.into()),
            iv: *iv,
        }
    }

    fn encrypt_in_place(&mut self, data: &mut [u8]) -> RarResult<()> {
        if !data.len().is_multiple_of(16) {
            return Err(RarError::Format(format!(
                "plaintext length {} is not a multiple of 16",
                data.len()
            )));
        }
        for block in data.as_chunks_mut::<16>().0 {
            for (byte, iv_byte) in block.iter_mut().zip(self.iv) {
                *byte ^= iv_byte;
            }
            self.cipher.encrypt_block(block.into());
            self.iv.copy_from_slice(block);
        }
        Ok(())
    }

    fn decrypt_in_place(&mut self, data: &mut [u8]) -> RarResult<()> {
        if !data.len().is_multiple_of(16) {
            return Err(RarError::Format(format!(
                "ciphertext length {} is not a multiple of 16",
                data.len()
            )));
        }
        for block in data.as_chunks_mut::<16>().0 {
            let ciphertext: [u8; 16] = *block;
            self.cipher.decrypt_block(block.into());
            for (byte, iv_byte) in block.iter_mut().zip(self.iv) {
                *byte ^= iv_byte;
            }
            self.iv = ciphertext;
        }
        Ok(())
    }
}

/// RAR5 zero-fill padding length for `plain_len` plaintext bytes: padded
/// to a 16-byte AES block boundary, with a minimum of one block (empty
/// members still produce 16 ciphertext bytes).
pub fn zero_padded_len(plain_len: u64) -> u64 {
    plain_len.div_ceil(16).max(1) * 16
}

/// Encrypt `plaintext` with AES-256-CBC using zero-fill padding.
pub fn encrypt_data(plaintext: &[u8], key: &[u8; 32], iv: &[u8; 16]) -> Vec<u8> {
    let padded_len = zero_padded_len(plaintext.len() as u64) as usize;
    let mut buf = vec![0u8; padded_len];
    buf[..plaintext.len()].copy_from_slice(plaintext);

    let mut cipher = Aes256Cbc::new(key, iv);
    cipher
        .encrypt_in_place(&mut buf)
        .expect("padded length is a multiple of 16");
    buf
}

/// Streaming AES-256-CBC encryptor for one RAR5 member.
///
/// The IV chain carries across `encrypt_in_place` calls, so a member's
/// ciphertext can be produced in bounded chunks (multi-volume archives
/// split the continuous ciphertext stream at arbitrary byte boundaries).
/// Callers must handle the zero-fill padding of the final block.
pub struct Aes256CbcStream {
    inner: Aes256Cbc,
}

impl Aes256CbcStream {
    pub fn new(key: &[u8; 32], iv: &[u8; 16]) -> Self {
        Self {
            inner: Aes256Cbc::new(key, iv),
        }
    }

    /// Encrypt `data` in place and advance the IV chain. `data.len()` must
    /// be a multiple of 16 (the RAR5 block size).
    pub fn encrypt_in_place(&mut self, data: &mut [u8]) -> RarResult<()> {
        self.inner.encrypt_in_place(data)
    }
}

/// Decrypt AES-256-CBC ciphertext. Returns decrypted bytes including any
/// zero-fill padding; caller should truncate to the known unpacked size.
pub fn decrypt_data(ciphertext: &[u8], key: &[u8; 32], iv: &[u8; 16]) -> RarResult<Vec<u8>> {
    if !ciphertext.len().is_multiple_of(16) {
        return Err(RarError::Format(format!(
            "ciphertext length {} is not a multiple of 16",
            ciphertext.len()
        )));
    }
    let mut buf = ciphertext.to_vec();
    let mut cipher = Aes256Cbc::new(key, iv);
    cipher
        .decrypt_in_place(&mut buf)
        .map_err(|e| RarError::Format(format!("AES decrypt error: {e}")))?;
    Ok(buf)
}

// ── Encryption Parameters ───────────────────────────────────────────────────

/// Holds the encryption parameters for a single encrypted file or header.
#[derive(Clone, Debug)]
pub struct EncryptionParams {
    pub version: u8,
    pub flags: u8,
    pub strength: u8,
    pub salt: [u8; ENCR_SALT_SIZE],
    pub iv: [u8; ENCR_IV_SIZE],
    pub checksum: Option<[u8; 12]>,
    pub iterations: u32,
}

impl EncryptionParams {
    /// Parse encryption parameters from the extra area encryption record bytes.
    pub fn from_extra_bytes(data: &[u8]) -> RarResult<Self> {
        let mut offset = 0;

        let (version, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("encr version: {e}")))?;
        offset += n;
        if version > u64::from(ENCR_VERSION_AES256) {
            return Err(RarError::Format(format!(
                "unsupported encryption version {version}"
            )));
        }
        let (flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("encr flags: {e}")))?;
        offset += n;
        check_encr_flags(flags)?;

        if offset >= data.len() {
            return Err(RarError::Format("truncated encryption record".into()));
        }
        let strength = data[offset];
        offset += 1;
        if strength > MAX_KDF_COUNT_LOG {
            return Err(RarError::Format(format!(
                "encryption strength {strength} exceeds maximum {MAX_KDF_COUNT_LOG}"
            )));
        }

        if offset + ENCR_SALT_SIZE > data.len() {
            return Err(RarError::Format("truncated salt".into()));
        }
        let mut salt = [0u8; ENCR_SALT_SIZE];
        salt.copy_from_slice(&data[offset..offset + ENCR_SALT_SIZE]);
        offset += ENCR_SALT_SIZE;

        if offset + ENCR_IV_SIZE > data.len() {
            return Err(RarError::Format("truncated IV".into()));
        }
        let mut iv = [0u8; ENCR_IV_SIZE];
        iv.copy_from_slice(&data[offset..offset + ENCR_IV_SIZE]);
        offset += ENCR_IV_SIZE;

        let checksum = if flags & u64::from(ENCR_FLAG_CHECKSUM) != 0 {
            // A flagged check value must be complete: treating a truncated
            // one as "no check" would make `verify_password` accept any
            // password for the member.
            if data.len().saturating_sub(offset) < 12 {
                return Err(RarError::Format(
                    "truncated encryption password check value".into(),
                ));
            }
            let mut ck = [0u8; 12];
            ck.copy_from_slice(&data[offset..offset + 12]);
            Some(ck)
        } else {
            None
        };

        let iterations = 1u32 << strength;

        Ok(EncryptionParams {
            version: version as u8,
            flags: flags as u8,
            strength,
            salt,
            iv,
            checksum,
            iterations,
        })
    }

    /// True when checksums in this archive are MAC'd with the hash key
    /// (encryption record flag 0x0002).
    pub fn uses_hash_mac(&self) -> bool {
        self.flags & ENCR_FLAG_HASH_MAC != 0
    }

    /// Verify a password against the stored check value (if present).
    ///
    /// RAR5 stores a 12-byte check: 8 bytes from the KDF with extra
    /// iterations (XOR-folded) plus a 4-byte checksum which is the first
    /// 4 bytes of SHA-256 over those 8 bytes. Comparisons are
    /// constant-time. Returns true if the password is correct or no check
    /// value is stored.
    pub fn verify_password(&self, password: &str) -> bool {
        let ck = match &self.checksum {
            Some(c) => c,
            None => return true,
        };
        match self.derive_keys(password) {
            Ok(keys) => keys.check_password(ck),
            Err(_) => false,
        }
    }

    /// Derive the key material once and verify the stored check value in the
    /// same pass.
    ///
    /// Returns `Ok(None)` when the stored check value rejects the password and
    /// `Ok(Some(keys))` otherwise (including when no check value is stored),
    /// so the KDF never runs twice for an encrypted member. Errors are
    /// reserved for malformed or unsupported parameters.
    pub fn derive_and_verify(&self, password: &str) -> RarResult<Option<DerivedKeys>> {
        self.derive_and_verify_with(password, false)
    }

    /// [`Self::derive_and_verify`] for service records ("STM" items), whose
    /// all-zero `PswCheck` from RAR 5.21 and earlier means "not present".
    pub fn derive_and_verify_service(&self, password: &str) -> RarResult<Option<DerivedKeys>> {
        self.derive_and_verify_with(password, true)
    }

    fn derive_and_verify_with(
        &self,
        password: &str,
        service: bool,
    ) -> RarResult<Option<DerivedKeys>> {
        let keys = self.derive_keys(password)?;
        let verified = match &self.checksum {
            Some(ck) if service => keys.check_password_service(ck),
            Some(ck) => keys.check_password(ck),
            None => true,
        };
        Ok(verified.then_some(keys))
    }

    /// Derive the full key material for `password` (single KDF pass).
    pub fn derive_keys(&self, password: &str) -> RarResult<DerivedKeys> {
        derive_keys(password, &self.salt, self.strength)
    }

    /// Derive and return the AES key for `password`.
    ///
    /// Returns the KDF error (e.g. an out-of-range strength) instead of
    /// silently degrading to a zero key, so callers never encrypt/decrypt
    /// against a wrong key.
    pub fn get_key(&self, password: &str) -> RarResult<[u8; ENCR_KEY_SIZE]> {
        self.derive_keys(password).map(|k| k.key)
    }

    /// Decrypt ciphertext with password using stored parameters.
    pub fn decrypt(&self, ciphertext: &[u8], password: &str) -> RarResult<Vec<u8>> {
        let key = self.get_key(password)?;
        decrypt_data(ciphertext, &key, &self.iv)
    }

    /// Encrypt plaintext with password using stored parameters.
    pub fn encrypt(&self, plaintext: &[u8], password: &str) -> RarResult<Vec<u8>> {
        let key = self.get_key(password)?;
        Ok(encrypt_data(plaintext, &key, &self.iv))
    }

    /// MAC a CRC32 for this file's password (for writing encrypted files).
    pub fn mac_crc32(&self, crc: u32, password: &str) -> RarResult<u32> {
        let keys = self.derive_keys(password)?;
        Ok(keys.mac_crc32(crc))
    }

    /// MAC a 32-byte hash for this file's password (for writing encrypted files).
    pub fn mac_hash32(&self, hash: [u8; 32], password: &str) -> RarResult<[u8; 32]> {
        let keys = self.derive_keys(password)?;
        Ok(keys.mac_hash32(hash))
    }

    /// Generate random encryption parameters with a password verification checksum.
    ///
    /// Each file gets a unique random salt and IV. The 12-byte checksum
    /// consists of an 8-byte XOR-folded PswCheck plus a 4-byte SHA-256
    /// checksum over it, matching the native RAR5 format. Flags include
    /// the hash-key bit (0x0002) so checksums of encrypted files are
    /// MAC'd, matching WinRAR behavior.
    pub fn generate_for_password(password: &str, strength: u8) -> Self {
        Self::generate_with_keys(password, strength).0
    }

    /// [`Self::generate_for_password`] returning the derived key material
    /// too, so a caller that keeps it (the header-encryption key cache) does
    /// not run the KDF again per block.
    pub(crate) fn generate_with_keys(password: &str, strength: u8) -> (Self, DerivedKeys) {
        generate_params_and_keys(password, strength, ENCR_FLAG_CHECKSUM | ENCR_FLAG_HASH_MAC)
    }

    /// Serialize to the RAR5 extra-area encryption record binary format.
    ///
    /// Format: `[record_size vint] [record_type vint] [body bytes]`
    pub fn to_extra_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(vint::encode(ENCR_VERSION_AES256 as u64));
        body.extend(vint::encode(self.flags as u64));
        body.push(self.strength);
        body.extend_from_slice(&self.salt);
        body.extend_from_slice(&self.iv);
        if let Some(ref ck) = self.checksum {
            body.extend_from_slice(ck);
        }

        let type_bytes = vint::encode(EXTRA_FILE_ENCRYPTION);
        let rec_size = type_bytes.len() + body.len();
        let mut out = Vec::new();
        out.extend(vint::encode(rec_size as u64));
        out.extend(type_bytes);
        out.extend(body);
        out
    }

    /// Serialize as an archive-level encryption header block (type 0x04).
    ///
    /// Written once after the main archive header when header encryption is
    /// enabled; every subsequent header block is `[16-byte IV][AES-256-CBC
    /// encrypted header]`. Body: `[block_type vint] [block_flags vint]
    /// [encr_version vint] [encr_flags vint] [u8 strength] [16-byte salt]
    /// [12-byte check value]` — matching `parse_archive_encrypt_header`.
    pub fn to_archive_header_block(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(vint::encode(crate::format::rar5::BLOCK_TYPE_ENCRYPT_HEADER));
        body.extend(vint::encode(0u64)); // block flags
        body.extend(vint::encode(ENCR_VERSION_AES256 as u64));
        // The archive-level record carries only the password-check bit;
        // the hash-key bit (0x0002) belongs to per-file records.
        body.extend(vint::encode((self.flags & 0x0001) as u64));
        body.push(self.strength);
        body.extend_from_slice(&self.salt);
        if let Some(ref ck) = self.checksum {
            body.extend_from_slice(ck);
        }

        crate::format::rar5::headers::frame_block(&body)
    }
}

/// Generate fresh random parameters and derive the key material in the same
/// pass, so a caller that keeps the keys never runs the KDF twice.
fn generate_params_and_keys(
    password: &str,
    strength: u8,
    flags: u8,
) -> (EncryptionParams, DerivedKeys) {
    let mut salt = [0u8; ENCR_SALT_SIZE];
    let mut iv = [0u8; ENCR_IV_SIZE];
    rand::fill(&mut salt);
    rand::fill(&mut iv);

    let keys = derive_keys(password, &salt, strength).expect("valid strength");
    let psw_check = keys.password_check;

    let digest = sha2::Sha256::digest(psw_check);
    let mut checksum = [0u8; 12];
    checksum[..8].copy_from_slice(&psw_check);
    checksum[8..12].copy_from_slice(&digest[..4]);

    (
        EncryptionParams {
            version: ENCR_VERSION_AES256,
            flags,
            strength,
            salt,
            iv,
            checksum: Some(checksum),
            iterations: 1u32 << strength,
        },
        keys,
    )
}

/// Per-member encryption session: the ENCR record parameters plus the key
/// material derived once for this member's random salt.
///
/// The header CRC/hash MACs and the payload cipher all reuse the derived
/// keys, so an encrypted member pays one KDF pass instead of one per
/// operation (generate, MAC CRC, MAC hash, encrypt).
pub(crate) struct MemberEncryption {
    params: EncryptionParams,
    keys: DerivedKeys,
}

impl MemberEncryption {
    /// Fresh random parameters for `password`, with the hash-key flags a
    /// member record carries.
    pub(crate) fn generate(password: &str, strength: u8) -> Self {
        Self::generate_with_flags(password, strength, ENCR_FLAG_CHECKSUM | ENCR_FLAG_HASH_MAC)
    }

    /// [`Self::generate`] with explicit ENCR flags: service records ("STM")
    /// carry the password check only and leave stored CRCs in plaintext.
    pub(crate) fn generate_with_flags(password: &str, strength: u8, flags: u8) -> Self {
        let (params, keys) = generate_params_and_keys(password, strength, flags);
        Self { params, keys }
    }

    /// The member's ENCR extra record bytes.
    pub(crate) fn extra_bytes(&self) -> Vec<u8> {
        self.params.to_extra_bytes()
    }

    /// MAC a CRC32 value for the stored header CRC.
    pub(crate) fn mac_crc32(&self, crc: u32) -> u32 {
        self.keys.mac_crc32(crc)
    }

    /// MAC a 32-byte hash for the stored hash record.
    pub(crate) fn mac_hash32(&self, hash: [u8; 32]) -> [u8; 32] {
        self.keys.mac_hash32(hash)
    }

    /// AES key and IV for the payload cipher; the streaming writer seeds its
    /// CBC chains from these.
    pub(crate) fn key_iv(&self) -> (&[u8; ENCR_KEY_SIZE], &[u8; ENCR_IV_SIZE]) {
        (&self.keys.key, &self.params.iv)
    }

    /// Encrypt a member payload with this session's key and IV.
    pub(crate) fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        encrypt_data(plaintext, &self.keys.key, &self.params.iv)
    }
}

/// Parse the archive-level encryption header block (type 0x04).
///
/// The block body (after block_type and flags vints) contains:
/// `[vint encr_version] [vint encr_flags] [u8 strength] [16-byte salt]`
/// Optionally followed by a 12-byte password check value if encr_flags & 0x01.
pub fn parse_archive_encrypt_header(
    raw: &crate::format::rar5::headers::RawBlock,
) -> RarResult<EncryptionParams> {
    let data = &raw.header_data;
    let mut offset = 0;

    // Skip block_type and block_flags (already parsed, but stored in header_data)
    let (_, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("block type: {e}")))?;
    offset += n;
    let (_, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
    offset += n;

    // Encryption-specific fields
    let (version, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("encr version: {e}")))?;
    offset += n;
    if version > u64::from(ENCR_VERSION_AES256) {
        return Err(RarError::Format(format!(
            "unsupported encryption version {version}"
        )));
    }
    let (flags, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("encr flags: {e}")))?;
    offset += n;
    check_encr_flags(flags)?;

    if offset >= data.len() {
        return Err(RarError::Format("truncated encryption header".into()));
    }
    let strength = data[offset];
    offset += 1;
    if strength > MAX_KDF_COUNT_LOG {
        return Err(RarError::Format(format!(
            "encryption strength {strength} exceeds maximum {MAX_KDF_COUNT_LOG}"
        )));
    }

    if offset + ENCR_SALT_SIZE > data.len() {
        return Err(RarError::Format("truncated encryption header salt".into()));
    }
    let mut salt = [0u8; ENCR_SALT_SIZE];
    salt.copy_from_slice(&data[offset..offset + ENCR_SALT_SIZE]);
    offset += ENCR_SALT_SIZE;

    let checksum = if flags & u64::from(ENCR_FLAG_CHECKSUM) != 0 {
        if data.len().saturating_sub(offset) < 12 {
            return Err(RarError::Format(
                "truncated encryption header password check value".into(),
            ));
        }
        let mut ck = [0u8; 12];
        ck.copy_from_slice(&data[offset..offset + 12]);
        Some(ck)
    } else {
        None
    };

    // Archive-level encryption header doesn't have its own IV —
    // each subsequent block carries its own IV.
    let iv = [0u8; ENCR_IV_SIZE];
    let iterations = 1u32 << strength;

    Ok(EncryptionParams {
        version: version as u8,
        flags: flags as u8,
        strength,
        salt,
        iv,
        checksum,
        iterations,
    })
}

/// Parse an archive encryption header block (type 0x04) that has already been
/// decoded into `raw`, verify the given password, and return the derived
/// header-decryption key.
pub fn derive_header_key(
    raw: &crate::format::rar5::headers::RawBlock,
    password: Option<&str>,
) -> RarResult<[u8; ENCR_KEY_SIZE]> {
    let password = password.ok_or_else(|| {
        RarError::Encrypted("archive has encrypted headers; provide a password".into())
    })?;
    let params = parse_archive_encrypt_header(raw)?;
    let keys = params
        .derive_and_verify(password)?
        .ok_or(RarError::WrongPassword)?;
    Ok(keys.key)
}

/// Parse the extra area of a file header to find encryption parameters.
/// Returns None if no encryption record is found.
pub fn parse_encryption_extra(extra_data: &[u8]) -> RarResult<Option<EncryptionParams>> {
    let mut offset = 0;
    while offset < extra_data.len() {
        let (rec_size, n) = vint::decode_from_slice(extra_data, offset)
            .map_err(|e| RarError::Format(format!("extra record size: {e}")))?;
        offset += n;

        // `rec_size` is attacker-controlled (a vint can decode to u64::MAX).
        // An overflowing or past-the-end record ends the scan instead of
        // wrapping the length into a start>end slice.
        let rec_end = usize::try_from(rec_size)
            .ok()
            .and_then(|size| offset.checked_add(size))
            .filter(|end| *end <= extra_data.len());
        let Some(rec_end) = rec_end else {
            break;
        };

        let (rec_type, tn) = vint::decode_from_slice(extra_data, offset)
            .map_err(|e| RarError::Format(format!("extra record type: {e}")))?;

        if rec_type == EXTRA_FILE_ENCRYPTION {
            let body_start = offset
                .checked_add(tn)
                .filter(|start| *start <= rec_end)
                .ok_or_else(|| RarError::Format("encryption record is malformed".into()))?;
            let params = EncryptionParams::from_extra_bytes(&extra_data[body_start..rec_end])?;
            return Ok(Some(params));
        }

        offset = rec_end;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = b"Hello, RAR5 encryption!";

        let ct = encrypt_data(plaintext, &key, &iv);
        assert!(ct.len() >= plaintext.len());
        assert_eq!(ct.len() % 16, 0);

        let pt = decrypt_data(&ct, &key, &iv).unwrap();
        assert_eq!(&pt[..plaintext.len()], plaintext.as_slice());
    }

    #[test]
    fn decrypt_wrong_length_fails() {
        let key = [0u8; 32];
        let iv = [0u8; 16];
        let bad = vec![0u8; 15]; // not a multiple of 16
        assert!(decrypt_data(&bad, &key, &iv).is_err());
    }

    #[test]
    fn derive_key_deterministic() {
        let k1 = derive_key("test", b"salt1234salt1234", 100);
        let k2 = derive_key("test", b"salt1234salt1234", 100);
        assert_eq!(k1, k2);

        let k3 = derive_key("test2", b"salt1234salt1234", 100);
        assert_ne!(k1, k3);
    }

    #[test]
    fn derived_keys_match_legacy_pbkdf2_key() {
        let salt = *b"salt1234salt1234";
        let keys = derive_keys("test", &salt, 8).unwrap();
        // At strength 8, key == PBKDF2(2^8 iterations) block 1.
        let legacy = derive_key("test", &salt, 1 << 8);
        assert_eq!(keys.key, legacy);
    }

    #[test]
    fn strength_cap_rejected() {
        let mut data = vec![];
        data.extend(vint::encode(0u64)); // version
        data.extend(vint::encode(0x03u64)); // flags
        data.push(25); // strength > MAX_KDF_COUNT_LOG
        data.extend_from_slice(&[0u8; 16]); // salt
        data.extend_from_slice(&[0u8; 16]); // iv
        assert!(EncryptionParams::from_extra_bytes(&data).is_err());
    }

    #[test]
    fn password_check_constant_time() {
        let params = EncryptionParams::generate_for_password("hunter2", 4);
        assert!(params.verify_password("hunter2"));
        assert!(!params.verify_password("hunter3"));
        // No stored check -> always accepted (per RAR5 spec).
        let mut no_check = params.clone();
        no_check.checksum = None;
        assert!(no_check.verify_password("anything"));
    }

    #[test]
    fn kdf_truncates_passwords_at_127_chars() {
        let salt = *b"salt1234salt1234";
        let long = "p".repeat(crate::crypto::MAX_PASSWORD_CHARS + 3);
        let prefix = "p".repeat(crate::crypto::MAX_PASSWORD_CHARS);
        let keys_long = derive_keys(&long, &salt, 4).unwrap();
        let keys_prefix = derive_keys(&prefix, &salt, 4).unwrap();
        assert_eq!(keys_long.key, keys_prefix.key);
        assert_eq!(keys_long.hash_key, keys_prefix.hash_key);
        assert_eq!(keys_long.password_check, keys_prefix.password_check);
    }

    #[test]
    fn generated_params_accept_the_clamped_password() {
        let long = "p".repeat(crate::crypto::MAX_PASSWORD_CHARS + 3);
        let prefix = "p".repeat(crate::crypto::MAX_PASSWORD_CHARS);
        let params = EncryptionParams::generate_for_password(&long, 4);
        assert!(params.verify_password(&long));
        assert!(params.verify_password(&prefix));
        assert!(!params.verify_password(&"p".repeat(crate::crypto::MAX_PASSWORD_CHARS - 1)));
    }

    /// Extra-area encryption record body with strength 4 and fixed salt/IV.
    fn encr_record(version: u64, flags: u64, checksum: Option<[u8; 12]>) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(vint::encode(version));
        data.extend(vint::encode(flags));
        data.push(4);
        data.extend_from_slice(&[0x11u8; ENCR_SALT_SIZE]);
        data.extend_from_slice(&[0x22u8; ENCR_IV_SIZE]);
        if let Some(ck) = checksum {
            data.extend_from_slice(&ck);
        }
        data
    }

    /// Archive-level encryption header (`[type][flags][version][encr flags]
    /// [strength][salt][checksum?]`) as a raw block.
    fn archive_encrypt_header(version: u64, flags: u64) -> crate::format::rar5::headers::RawBlock {
        let mut data = Vec::new();
        data.extend(vint::encode(crate::format::rar5::BLOCK_TYPE_ENCRYPT_HEADER));
        data.extend(vint::encode(0u64));
        data.extend(vint::encode(version));
        data.extend(vint::encode(flags));
        data.push(4);
        data.extend_from_slice(&[0x11u8; ENCR_SALT_SIZE]);
        crate::format::rar5::headers::RawBlock {
            header_crc: 0,
            header_data: data,
            data_size: 0,
            data_offset: 0,
            block_type: 0,
            flags: 0,
        }
    }

    #[test]
    fn unknown_encryption_version_is_rejected() {
        let err = EncryptionParams::from_extra_bytes(&encr_record(
            1,
            u64::from(ENCR_FLAG_CHECKSUM),
            None,
        ))
        .unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");

        let err = parse_archive_encrypt_header(&archive_encrypt_header(1, 0)).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");
    }

    /// Undefined flag bits must be rejected instead of truncated; `flags`
    /// is a vint, so `0x102` must not become `0x02` (which would silently
    /// flip `uses_hash_mac`).
    #[test]
    fn unknown_encryption_flags_are_rejected() {
        for flags in [
            u64::from(ENCR_FLAG_CHECKSUM) | 0x04, // unknown bit next to a known one
            0x100,                                // only bits above the u8 range
            u64::from(ENCR_FLAG_HASH_MAC) | 0x100, // truncates to 0x02
        ] {
            let err = EncryptionParams::from_extra_bytes(&encr_record(0, flags, None)).unwrap_err();
            assert!(
                matches!(err, RarError::Format(_)),
                "flags {flags:#x}: {err}"
            );

            let err = parse_archive_encrypt_header(&archive_encrypt_header(0, flags)).unwrap_err();
            assert!(
                matches!(err, RarError::Format(_)),
                "flags {flags:#x}: {err}"
            );
        }

        // The defined combinations still parse (the check bit needs its
        // 12-byte value present).
        for flags in [0u64, u64::from(ENCR_FLAG_HASH_MAC)] {
            assert!(
                EncryptionParams::from_extra_bytes(&encr_record(0, flags, None)).is_ok(),
                "flags {flags:#x}"
            );
            assert!(
                parse_archive_encrypt_header(&archive_encrypt_header(0, flags)).is_ok(),
                "flags {flags:#x}"
            );
        }
        let with_check = u64::from(ENCR_FLAG_CHECKSUM | ENCR_FLAG_HASH_MAC);
        assert!(
            EncryptionParams::from_extra_bytes(&encr_record(0, with_check, Some([0u8; 12])))
                .is_ok()
        );
        let mut raw = archive_encrypt_header(0, with_check);
        raw.header_data.extend_from_slice(&[0u8; 12]);
        assert!(parse_archive_encrypt_header(&raw).is_ok());
    }

    #[test]
    fn truncated_password_check_value_is_rejected() {
        // Flagged check value with only 11 trailing bytes: treating it as
        // "no check" would let `verify_password` accept any password.
        let mut data = encr_record(
            u64::from(ENCR_VERSION_AES256),
            u64::from(ENCR_FLAG_CHECKSUM),
            None,
        );
        data.extend_from_slice(&[0u8; 11]);
        let err = EncryptionParams::from_extra_bytes(&data).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");

        let mut raw = archive_encrypt_header(0, u64::from(ENCR_FLAG_CHECKSUM));
        raw.header_data.extend_from_slice(&[0u8; 11]);
        let err = parse_archive_encrypt_header(&raw).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");
    }

    #[test]
    fn service_record_all_zero_password_check_is_accepted() {
        let params = EncryptionParams::from_extra_bytes(&encr_record(
            0,
            u64::from(ENCR_FLAG_CHECKSUM),
            Some([0u8; 12]),
        ))
        .unwrap();

        // Member records stay strict even when the stored check is zero.
        assert!(!params.verify_password("hunter2"));
        assert!(params.derive_and_verify("hunter2").unwrap().is_none());

        // Service records mirror UnRAR: an all-zero PswCheck means the check
        // is absent, so any password derives keys.
        for password in ["hunter2", "anything"] {
            let keys = params.derive_and_verify_service(password).unwrap().unwrap();
            assert_eq!(keys.key, params.derive_keys(password).unwrap().key);
        }
    }

    #[test]
    fn derive_and_verify_matches_derive_keys() {
        let params = EncryptionParams::generate_for_password("hunter2", 4);
        let direct = params.derive_keys("hunter2").unwrap();
        let keys = params
            .derive_and_verify("hunter2")
            .unwrap()
            .expect("right password");
        assert_eq!(keys.key, direct.key);
        assert_eq!(keys.hash_key, direct.hash_key);
        assert_eq!(keys.password_check, direct.password_check);

        assert!(
            params.derive_and_verify("hunter3").unwrap().is_none(),
            "wrong password must be rejected"
        );

        // No stored check value: derivation still succeeds.
        let mut no_check = params.clone();
        no_check.checksum = None;
        assert!(no_check.derive_and_verify("anything").unwrap().is_some());
    }

    /// The write-side session must produce the same ENCR record, MACs and
    /// ciphertext as the per-call password API over the same parameters: it
    /// only skips the repeated derivations, not any step.
    #[test]
    fn member_encryption_matches_the_password_api() {
        let password = "hunter2";
        let session = MemberEncryption::generate(password, 4);
        let parsed = parse_encryption_extra(&session.extra_bytes())
            .unwrap()
            .expect("one ENCR record");
        assert!(parsed.uses_hash_mac());

        let crc = 0xDEAD_BEEF;
        assert_eq!(
            session.mac_crc32(crc),
            parsed.mac_crc32(crc, password).unwrap()
        );
        let hash = [0x5Au8; 32];
        assert_eq!(
            session.mac_hash32(hash),
            parsed.mac_hash32(hash, password).unwrap()
        );
        let plaintext = b"member payload ".repeat(9);
        assert_eq!(
            session.encrypt(&plaintext),
            parsed.encrypt(&plaintext, password).unwrap()
        );

        // The streaming writer seeds its CBC chains from `key_iv`; a wrong
        // key or IV there would corrupt every streamed encrypted member
        // without failing the checks above.
        let direct = parsed.derive_keys(password).unwrap();
        let (key, iv) = session.key_iv();
        assert_eq!(*key, direct.key);
        assert_eq!(*iv, parsed.iv);
    }

    /// Service-record sessions ("STM") carry the password check only: stored
    /// CRCs stay in plaintext.
    #[test]
    fn member_encryption_without_hash_mac_keeps_plaintext_crcs() {
        let session = MemberEncryption::generate_with_flags("hunter2", 4, ENCR_FLAG_CHECKSUM);
        let parsed = parse_encryption_extra(&session.extra_bytes())
            .unwrap()
            .expect("one ENCR record");
        assert!(!parsed.uses_hash_mac());
        assert!(
            parsed.checksum.is_some(),
            "the password check must stay enabled"
        );
    }

    #[test]
    fn hostile_extra_record_size_does_not_panic() {
        // A vint size of u64::MAX then an encryption type: the record end
        // must not overflow into a start>end slice.
        let mut huge = vint::encode(u64::MAX);
        huge.extend(vint::encode(EXTRA_FILE_ENCRYPTION));
        assert!(parse_encryption_extra(&huge).is_ok());

        // A zero-size record whose type vint does not fit inside it: the
        // body range would invert. Must be an error, never a panic.
        let mut zero = vint::encode(0u64);
        zero.extend(vint::encode(EXTRA_FILE_ENCRYPTION));
        assert!(parse_encryption_extra(&zero).is_err());

        // A record that ends exactly at its type byte (empty body) decodes
        // as a malformed record rather than panicking.
        let mut short = vint::encode(1u64);
        short.extend(vint::encode(EXTRA_FILE_ENCRYPTION));
        assert!(parse_encryption_extra(&short).is_err());
    }
}
