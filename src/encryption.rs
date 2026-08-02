/// RAR5 Encryption Support
///
/// RAR5 uses AES-256 in CBC mode with a key derived from a password via
/// PBKDF2-HMAC-SHA256.
///
/// Encryption scheme:
/// 1. Key derivation: PBKDF2-HMAC-SHA256 with 16-byte salt, 2^strength iterations
/// 2. IV: 16-byte random initialization vector per file
/// 3. Padding: Zero-fill to 16-byte AES block boundary
/// 4. Header encryption: When archive-level encryption header is present,
///    all subsequent blocks (including file headers) are also encrypted.
use crate::constants::*;
use crate::error::{RarError, RarResult};
use crate::vint;

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use sha2::Digest;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

// ── Key Derivation ──────────────────────────────────────────────────────────

/// Derive a 32-byte AES-256 key from `password` using PBKDF2-HMAC-SHA256.
pub fn derive_key(password: &str, salt: &[u8], iterations: u32) -> [u8; ENCR_KEY_SIZE] {
    let mut key = [0u8; ENCR_KEY_SIZE];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, iterations, &mut key);
    key
}

// ── Encryption / Decryption ──────────────────────────────────────────────────

/// Encrypt `plaintext` with AES-256-CBC using zero-fill padding.
pub fn encrypt_data(plaintext: &[u8], key: &[u8; 32], iv: &[u8; 16]) -> Vec<u8> {
    let block_size = 16;
    let padded_len = ((plaintext.len() + block_size - 1) / block_size * block_size).max(block_size);
    let mut buf = vec![0u8; padded_len];
    buf[..plaintext.len()].copy_from_slice(plaintext);

    let enc = Aes256CbcEnc::new(key.into(), iv.into());
    enc.encrypt_padded_vec_mut::<aes::cipher::block_padding::NoPadding>(&buf)
}

/// Decrypt AES-256-CBC ciphertext. Returns decrypted bytes including any
/// zero-fill padding; caller should truncate to the known unpacked size.
pub fn decrypt_data(ciphertext: &[u8], key: &[u8; 32], iv: &[u8; 16]) -> RarResult<Vec<u8>> {
    if ciphertext.len() % 16 != 0 {
        return Err(RarError::Format(format!(
            "ciphertext length {} is not a multiple of 16",
            ciphertext.len()
        )));
    }
    let dec = Aes256CbcDec::new(key.into(), iv.into());
    dec.decrypt_padded_vec_mut::<aes::cipher::block_padding::NoPadding>(ciphertext)
        .map_err(|e| RarError::Format(format!("AES decrypt error: {e}")))
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

    /// Verify a password against the stored check value (if present).
    ///
    /// RAR5 stores a 12-byte check: 8 bytes from PBKDF2 with extra
    /// iterations (XOR-folded) plus a 4-byte checksum which is the first
    /// 4 bytes of SHA-256 over those 8 bytes. Returns true if the password
    /// is correct or no check value is stored.
    pub fn verify_password(&self, password: &str) -> bool {
        let ck = match &self.checksum {
            Some(c) => c,
            None => return true,
        };

        // Derive PswCheckValue using iterations + 32
        let mut psw_check_value = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
            password.as_bytes(),
            &self.salt,
            self.iterations + 32,
            &mut psw_check_value,
        );

        // XOR four 8-byte blocks to get 8-byte PswCheck
        let mut psw_check = [0u8; 8];
        for i in 0..4 {
            for j in 0..8 {
                psw_check[j] ^= psw_check_value[i * 8 + j];
            }
        }

        if psw_check != ck[..8] {
            return false;
        }
        if ck.len() >= 12 {
            // 4-byte checksum: first 4 bytes of SHA-256 over the 8-byte
            // check value (the reference implementations verify this too).
            let digest = sha2::Sha256::digest(&psw_check);
            return digest[..4] == ck[8..12];
        }
        true
    }

    /// Derive and return the AES key for `password`.
    pub fn get_key(&self, password: &str) -> [u8; ENCR_KEY_SIZE] {
        derive_key(password, &self.salt, self.iterations)
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

    /// Generate random encryption parameters with a password verification checksum.
    ///
    /// Each file gets a unique random salt and IV. The 12-byte checksum
    /// consists of an 8-byte XOR-folded PswCheck plus a 4-byte SHA-256
    /// checksum over it, matching the native RAR5 format.
    pub fn generate_for_password(password: &str, strength: u8) -> Self {
        use rand::Rng;
        let mut rng = rand::rng();

        let mut salt = [0u8; ENCR_SALT_SIZE];
        let mut iv = [0u8; ENCR_IV_SIZE];
        rng.fill(&mut salt);
        rng.fill(&mut iv);

        let iterations = 1u32 << strength;

        // Derive PswCheckValue: PBKDF2 with (iterations + 32)
        let mut psw_check_value = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
            password.as_bytes(),
            &salt,
            iterations + 32,
            &mut psw_check_value,
        );

        // XOR-fold four 8-byte blocks → 8-byte PswCheck
        let mut psw_check = [0u8; 8];
        for i in 0..4 {
            for j in 0..8 {
                psw_check[j] ^= psw_check_value[i * 8 + j];
            }
        }

        // 4-byte checksum: first 4 bytes of SHA-256 over the 8-byte check
        // value (matches WinRAR/unrar and 7-Zip).
        let digest = sha2::Sha256::digest(&psw_check);
        let mut checksum = [0u8; 12];
        checksum[..8].copy_from_slice(&psw_check);
        checksum[8..12].copy_from_slice(&digest[..4]);

        EncryptionParams {
            version: ENCR_VERSION_AES256,
            flags: 0x01,
            strength,
            salt,
            iv,
            checksum: Some(checksum),
            iterations,
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
        body.extend(vint::encode(crate::constants::BLOCK_TYPE_ENCRYPT_HEADER));
        body.extend(vint::encode(0u64)); // block flags
        body.extend(vint::encode(ENCR_VERSION_AES256 as u64));
        body.extend(vint::encode(self.flags as u64));
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
pub fn parse_archive_encrypt_header(raw: &crate::headers::RawBlock) -> RarResult<EncryptionParams> {
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
}
