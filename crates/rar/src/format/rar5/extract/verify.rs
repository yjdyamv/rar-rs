//! Integrity verification of decoded members.

use crate::archive::RarArchive;
use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::model::FileHeader;

impl RarArchive {
    /// Verify CRC32 and BLAKE2sp integrity of decoded data. Encrypted
    /// members use the hash-key MAC when the encryption record requests it.
    pub(crate) fn verify_integrity(
        &self,
        idx: usize,
        crc: u32,
        blake: Option<[u8; 32]>,
        params: Option<&crypto::EncryptionParams>,
        keys: Option<&crypto::DerivedKeys>,
    ) -> RarResult<()> {
        let hdr = &self.entries[idx].header;
        verify_integrity_for(hdr, crc, blake, params, keys)
    }

    /// [`crate::format::rar5::headers::read_block`].
    pub(crate) fn archive_block_key(&self) -> RarResult<Option<[u8; 32]>> {
        let encr = match self.archive_encr.as_ref() {
            Some(encr) => encr,
            None => return Ok(None),
        };
        let password = match self.password.as_ref() {
            Some(password) => password,
            None => return Ok(None),
        };
        encr.get_key(password).map(Some)
    }
}

/// Verify CRC32 and BLAKE2sp integrity against a file header. Encrypted
/// members use the hash-key MAC when the encryption record requests it.
pub(super) fn verify_integrity_for(
    hdr: &FileHeader,
    crc: u32,
    blake: Option<[u8; 32]>,
    params: Option<&crypto::EncryptionParams>,
    keys: Option<&crypto::DerivedKeys>,
) -> RarResult<()> {
    let uses_mac = params.is_some_and(|p| p.uses_hash_mac());

    if let Some(expected) = hdr.crc32_val {
        let mut actual = crc;
        if uses_mac {
            let keys = keys.ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: missing derived keys for MAC verification",
                    hdr.name
                ))
            })?;
            actual = keys.mac_crc32(actual);
        }
        if actual != expected {
            return Err(RarError::Crc {
                expected,
                actual,
                context: hdr.name.clone(),
            });
        }
    }

    if let (Some(expected), Some(actual)) = (hdr.hash_value, blake) {
        let actual = if uses_mac {
            let keys = keys.ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: missing derived keys for hash MAC verification",
                    hdr.name
                ))
            })?;
            keys.mac_hash32(actual)
        } else {
            actual
        };
        if !crypto::constant_time_eq(&expected, &actual) {
            return Err(RarError::HashMismatch {
                expected,
                actual,
                context: hdr.name.clone(),
            });
        }
    }
    Ok(())
}
