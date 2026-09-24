//! Archive-level RAR5 encryption header block (type 0x04).
//!
//! The framing is the container's business, so this lives with the RAR5 format
//! rather than in `crypto` (which owns the encryption *vocabulary* and
//! primitives). The two functions here are the only place a `RawBlock` meets
//! encryption parameters.

use crate::crypto::EncryptionParams;
use crate::crypto::rar50::{
    ENCR_FLAG_CHECKSUM, ENCR_IV_SIZE, ENCR_KEY_SIZE, ENCR_SALT_SIZE, ENCR_VERSION_AES256,
    MAX_KDF_COUNT_LOG, check_encr_flags,
};
use crate::error::{RarError, RarResult};
use crate::vint;

use super::RawBlock;

/// Parse the archive-level encryption header block (type 0x04).
///
/// The block body (after block_type and flags vints) contains:
/// `[vint encr_version] [vint encr_flags] [u8 strength] [16-byte salt]`
/// Optionally followed by a 12-byte password check value if encr_flags & 0x01.
pub(crate) fn parse_archive_encrypt_header(raw: &RawBlock) -> RarResult<EncryptionParams> {
    let data = &raw.header_data;
    let mut offset = 0;

    // Skip block_type and block_flags (already parsed, but stored in header_data)
    let (_, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::format(format!("block type: {e}")))?;
    offset += n;
    let (_, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::format(format!("block flags: {e}")))?;
    offset += n;

    // Encryption-specific fields
    let (version, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::format(format!("encr version: {e}")))?;
    offset += n;
    if version > u64::from(ENCR_VERSION_AES256) {
        return Err(RarError::format(format!(
            "unsupported encryption version {version}"
        )));
    }
    let (flags, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::format(format!("encr flags: {e}")))?;
    offset += n;
    check_encr_flags(flags)?;

    if offset >= data.len() {
        return Err(RarError::format("truncated encryption header"));
    }
    let strength = data[offset];
    offset += 1;
    if strength > MAX_KDF_COUNT_LOG {
        return Err(RarError::format(format!(
            "encryption strength {strength} exceeds maximum {MAX_KDF_COUNT_LOG}"
        )));
    }

    if offset + ENCR_SALT_SIZE > data.len() {
        return Err(RarError::format("truncated encryption header salt"));
    }
    let mut salt = [0u8; ENCR_SALT_SIZE];
    salt.copy_from_slice(&data[offset..offset + ENCR_SALT_SIZE]);
    offset += ENCR_SALT_SIZE;

    let checksum = if flags & u64::from(ENCR_FLAG_CHECKSUM) != 0 {
        if data.len().saturating_sub(offset) < 12 {
            return Err(RarError::format(
                "truncated encryption header password check value",
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
pub(crate) fn derive_header_key(
    raw: &RawBlock,
    password: Option<&str>,
) -> RarResult<[u8; ENCR_KEY_SIZE]> {
    let password = password
        .ok_or_else(|| RarError::encrypted("archive has encrypted headers; provide a password"))?;
    let params = parse_archive_encrypt_header(raw)?;
    let keys = params
        .derive_and_verify(password)?
        .ok_or(RarError::WrongPassword)?;
    Ok(keys.key)
}

/// Serialize an archive-level encryption header block (type 0x04).
///
/// Written once after the main archive header when header encryption is
/// enabled; every subsequent header block is `[16-byte IV][AES-256-CBC
/// encrypted header]`. Body: `[block_type vint] [block_flags vint]
/// [encr_version vint] [encr_flags vint] [u8 strength] [16-byte salt]
/// [12-byte check value]` — matching `parse_archive_encrypt_header`.
pub(crate) fn build_archive_encrypt_header_block(params: &EncryptionParams) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(vint::encode(crate::format::rar5::BLOCK_TYPE_ENCRYPT_HEADER));
    body.extend(vint::encode(0u64)); // block flags
    body.extend(vint::encode(ENCR_VERSION_AES256 as u64));
    // The archive-level record carries only the password-check bit; the
    // hash-key bit (0x0002) belongs to per-file records.
    body.extend(vint::encode(u64::from(params.flags & 0x01)));
    body.push(params.strength);
    body.extend_from_slice(&params.salt);
    if let Some(ck) = params.checksum {
        body.extend_from_slice(&ck);
    }
    super::serialize::frame_block(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::rar50::ENCR_FLAG_HASH_MAC;

    /// The archive-level encryption header as a raw block:
    /// `[type][flags][version][encr flags][strength][salt][checksum?]`.
    ///
    /// Moved here from `crypto/rar50.rs`: the body *is* the container
    /// framing, so its tests belong next to the parser that reads it.
    fn archive_encrypt_header(version: u64, flags: u64) -> RawBlock {
        let mut data = Vec::new();
        data.extend(vint::encode(crate::format::rar5::BLOCK_TYPE_ENCRYPT_HEADER));
        data.extend(vint::encode(0u64));
        data.extend(vint::encode(version));
        data.extend(vint::encode(flags));
        data.push(4);
        data.extend_from_slice(&[0x11u8; ENCR_SALT_SIZE]);
        RawBlock {
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
                parse_archive_encrypt_header(&archive_encrypt_header(0, flags)).is_ok(),
                "flags {flags:#x}"
            );
        }
        let with_check = u64::from(ENCR_FLAG_CHECKSUM | ENCR_FLAG_HASH_MAC);
        let mut raw = archive_encrypt_header(0, with_check);
        raw.header_data.extend_from_slice(&[0u8; 12]);
        assert!(parse_archive_encrypt_header(&raw).is_ok());
    }

    #[test]
    fn truncated_password_check_value_is_rejected() {
        // Flagged check value with only 11 trailing bytes: treating it as
        // "no check" would let `verify_password` accept any password.
        let mut raw = archive_encrypt_header(0, u64::from(ENCR_FLAG_CHECKSUM));
        raw.header_data.extend_from_slice(&[0u8; 11]);
        let err = parse_archive_encrypt_header(&raw).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");
    }

    /// The ENCR tag is declared in `crypto` (the layer that switches on it)
    /// and in `format::rar5` (the container's own table). Both must agree, or
    /// the writer would emit a record the reader cannot find.
    #[test]
    fn encr_record_tag_matches_the_container_table() {
        assert_eq!(
            crate::crypto::rar50::EXTRA_FILE_ENCRYPTION,
            crate::format::rar5::EXTRA_FILE_ENCRYPTION
        );
    }
}
