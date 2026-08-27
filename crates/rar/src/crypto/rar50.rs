use crate::error::{RarError, RarResult};
use crate::rar50::vint;
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
use crate::rar50::*;

use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha256 = Hmac<Sha256>;

/// Maximum accepted KDF strength exponent (2^24 iterations). Larger values
/// are rejected at parse time to prevent CPU denial-of-service.
pub const MAX_KDF_COUNT_LOG: u8 = 24;

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
        digest.chunks_exact(4).fold(0, |acc, chunk| {
            acc ^ u32::from_le_bytes(chunk.try_into().unwrap())
        })
    }

    /// MAC a 32-byte hash with the hash key (RAR5 encrypted-file hashes).
    pub fn mac_hash32(&self, hash: [u8; 32]) -> [u8; 32] {
        let digest = hmac_sha256(&self.hash_key, &hash);
        let mut out = [0u8; 32];
        for (slot, chunk) in out.chunks_exact_mut(4).zip(digest.chunks_exact(4)) {
            slot.copy_from_slice(&u32::from_le_bytes(chunk.try_into().unwrap()).to_le_bytes());
        }
        out
    }

    /// Verify the 12-byte stored password check value.
    pub fn check_password(&self, stored: &[u8; 12]) -> bool {
        let checksum = Sha256::digest(&stored[..8]);
        constant_time_eq(&self.password_check, &stored[..8])
            && constant_time_eq(&checksum[..4], &stored[8..12])
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

    let mut first_input = Vec::with_capacity(salt.len() + 4);
    first_input.extend_from_slice(salt);
    first_input.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_sha256(password.as_bytes(), &first_input);
    let mut accumulator = u;
    let mut taps = [[0u8; ENCR_KEY_SIZE]; 3];
    let mut iterations = (1u32 << strength) - 1;

    for tap in &mut taps {
        for _ in 0..iterations {
            u = hmac_sha256(password.as_bytes(), &u);
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

/// Derive a 32-byte AES-256 key (kept for API compatibility).
pub fn derive_key(password: &str, salt: &[u8], iterations: u32) -> [u8; ENCR_KEY_SIZE] {
    let mut key = [0u8; ENCR_KEY_SIZE];
    pbkdf2_fallback(password, salt, iterations, &mut key);
    key
}

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
        for block in data.chunks_exact_mut(16) {
            for (byte, iv_byte) in block.iter_mut().zip(self.iv) {
                *byte ^= iv_byte;
            }
            let block: &mut [u8; 16] = block.try_into().expect("16-byte AES block");
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
        for block in data.chunks_exact_mut(16) {
            let ciphertext: [u8; 16] = block.try_into().expect("16-byte AES block");
            let block: &mut [u8; 16] = block.try_into().expect("16-byte AES block");
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
        let (flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("encr flags: {e}")))?;
        offset += n;

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

        let checksum = if flags & 0x01 != 0 && offset + 12 <= data.len() {
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
        self.flags & 0x0002 != 0
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

    /// Derive the full key material for `password` (single KDF pass).
    pub fn derive_keys(&self, password: &str) -> RarResult<DerivedKeys> {
        derive_keys(password, &self.salt, self.strength)
    }

    /// Derive and return the AES key for `password`.
    pub fn get_key(&self, password: &str) -> [u8; ENCR_KEY_SIZE] {
        self.derive_keys(password)
            .map(|k| k.key)
            .unwrap_or([0u8; ENCR_KEY_SIZE])
    }

    /// Decrypt ciphertext with password using stored parameters.
    pub fn decrypt(&self, ciphertext: &[u8], password: &str) -> RarResult<Vec<u8>> {
        let key = self.get_key(password);
        decrypt_data(ciphertext, &key, &self.iv)
    }

    /// Encrypt plaintext with password using stored parameters.
    pub fn encrypt(&self, plaintext: &[u8], password: &str) -> Vec<u8> {
        let key = self.get_key(password);
        encrypt_data(plaintext, &key, &self.iv)
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

        EncryptionParams {
            version: ENCR_VERSION_AES256,
            flags: 0x03,
            strength,
            salt,
            iv,
            checksum: Some(checksum),
            iterations: 1u32 << strength,
        }
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
        body.extend(vint::encode(crate::rar50::BLOCK_TYPE_ENCRYPT_HEADER));
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

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();

        let mut out = Vec::with_capacity(4 + header_content.len());
        out.extend(crc.to_le_bytes());
        out.extend(header_content);
        out
    }
}

/// Parse the archive-level encryption header block (type 0x04).
///
/// The block body (after block_type and flags vints) contains:
/// `[vint encr_version] [vint encr_flags] [u8 strength] [16-byte salt]`
/// Optionally followed by a 12-byte password check value if encr_flags & 0x01.
pub fn parse_archive_encrypt_header(
    raw: &crate::rar50::headers::RawBlock,
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
    let (flags, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("encr flags: {e}")))?;
    offset += n;

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

    let checksum = if flags & 0x01 != 0 && offset + 12 <= data.len() {
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

/// Check if a file header's extra area contains an encryption record.
pub fn is_encrypted(extra_data: &[u8]) -> bool {
    parse_encryption_extra(extra_data)
        .map(|p| p.is_some())
        .unwrap_or(false)
}

/// Parse the extra area of a file header to find encryption parameters.
/// Returns None if no encryption record is found.
pub fn parse_encryption_extra(extra_data: &[u8]) -> RarResult<Option<EncryptionParams>> {
    let mut offset = 0;
    while offset < extra_data.len() {
        let (rec_size, n) = vint::decode_from_slice(extra_data, offset)
            .map_err(|e| RarError::Format(format!("extra record size: {e}")))?;
        offset += n;

        let rec_end = offset + rec_size as usize;
        if rec_end > extra_data.len() {
            break;
        }

        let (rec_type, tn) = vint::decode_from_slice(extra_data, offset)
            .map_err(|e| RarError::Format(format!("extra record type: {e}")))?;

        if rec_type == EXTRA_FILE_ENCRYPTION {
            let params = EncryptionParams::from_extra_bytes(&extra_data[offset + tn..rec_end])?;
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
    fn mac_crc32_is_deterministic_and_password_sensitive() {
        let p1 = EncryptionParams::generate_for_password("pw", 4);
        let p2 = EncryptionParams::generate_for_password("pw", 4);
        let a = p1.mac_crc32(0x12345678, "pw").unwrap();
        // Same params (salt) -> same MAC; different salt -> different MAC.
        let b = p1.mac_crc32(0x12345678, "pw").unwrap();
        assert_eq!(a, b);
        let b2 = p2.mac_crc32(0x12345678, "pw").unwrap();
        assert_ne!(a, b2);
        let c = p1.mac_crc32(0x12345679, "pw").unwrap();
        assert_ne!(a, c);
        let d = p1.mac_crc32(0x12345678, "other").unwrap();
        assert_ne!(a, d);
    }
}
