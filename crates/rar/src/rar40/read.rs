//! RAR 1.5–4.x member read + decode.
//!
//! Reads a member's packed payload (single volume), decrypts it with the
//! cipher selected by `unp_ver` (15 → RAR15 stream, 20/26 → RAR20 block, and
//! 29+ → RAR30 AES-CBC block), then decodes it. STORE members pass through
//! raw; compressed members with unpack version 29 or newer decode through
//! [`crate::codec::rar29`] (solid chains share one decoder instance passed in
//! by the caller). RAR 1.5/2.x compressed members and RAR3/4 PPMd or
//! VM-filtered members are reported as not yet supported.

use crate::codec::rar20::Rar20Decoder;
use crate::codec::rar29::Rar29Decoder;
use crate::crc32;
use crate::crypto::{Rar15Cipher, Rar20Cipher, Rar30Cipher};
use crate::error::{RarError, RarResult};
use crate::rar50::headers::FileHeader;
use std::io::{Read, Seek, SeekFrom};

/// Decode a single-volume RAR4 member into memory.
///
/// `decoder` carries persistent solid-chain state (`None` for a standalone
/// member): compressed members feed it, and its look-behind window then
/// serves the next solid member. The returned Vec holds exactly this member's
/// unpacked bytes.
pub(crate) fn decode_member_bytes(
    stream: &mut (impl Read + Seek),
    hdr: &FileHeader,
    password: Option<&str>,
    decoder: Option<&mut Rar29Decoder>,
) -> RarResult<Vec<u8>> {
    if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
        return Ok(Vec::new());
    }

    let mut packed = vec![0u8; hdr.packed_size as usize];
    stream.seek(SeekFrom::Start(hdr.data_offset))?;
    stream.read_exact(&mut packed).map_err(RarError::Io)?;

    let encrypted = hdr.flags & super::FHD_PASSWORD as u64 != 0;
    if encrypted {
        let password = password
            .ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: encrypted member, no password provided",
                    hdr.name
                ))
            })?
            .as_bytes();
        decrypt_in_place(hdr, password, &mut packed)?;
    }

    let unp_size = hdr.unpacked_size as usize;
    if super::is_stored(hdr.comp_method) {
        // STORE: encrypted payload includes block padding, and STORE never
        // expands, so trim to the unpacked size.
        if packed.len() > unp_size {
            packed.truncate(unp_size);
        }
        return Ok(packed);
    }

    if hdr.unp_ver >= 29 {
        let out = match decoder {
            Some(dec) => dec.decode_member(&packed, hdr.unpacked_size)?,
            None => Rar29Decoder::new().decode_member(&packed, hdr.unpacked_size)?,
        };
        return Ok(out);
    }

    if hdr.unp_ver == 20 || hdr.unp_ver == 26 {
        // RAR 2.x LZSS+Huffman. The shared RAR29 solid-chain decoder is not
        // reused here: solid RAR 2.x chains (flagged FHD_SOLID on old
        // archives) are not yet supported, so every member decodes fresh.
        let out = Rar20Decoder::new().decode_member(&packed, hdr.unpacked_size)?;
        return Ok(out);
    }

    Err(RarError::Unsupported(format!(
        "RAR 1.5 compressed members (unpack version {}) are not yet supported",
        hdr.unp_ver
    )))
}

/// Decrypt `data` in place according to the member's cipher.
pub(crate) fn decrypt_in_place(
    hdr: &FileHeader,
    password: &[u8],
    data: &mut [u8],
) -> RarResult<()> {
    match hdr.unp_ver {
        15 => {
            Rar15Cipher::new(password).crypt_in_place(data);
            Ok(())
        }
        20 | 26 => Rar20Cipher::new(password)
            .decrypt_in_place(data)
            .map_err(|e| RarError::Format(format!("RAR4 RAR20 decrypt: {e}"))),
        v if v >= 29 => {
            let mut cipher = Rar30Cipher::new(password, hdr.salt)
                .map_err(|e| RarError::Format(format!("RAR4 RAR30 key setup: {e}")))?;
            cipher
                .decrypt_in_place(data)
                .map_err(|e| RarError::Format(format!("RAR4 RAR30 decrypt: {e}")))
        }
        other => Err(RarError::Unsupported(format!(
            "RAR4 encryption unpack version {other} not supported"
        ))),
    }
}

/// Compute the RFC/standard CRC-32 of `data` for RAR4 integrity checking.
pub(crate) fn member_crc(data: &[u8]) -> u32 {
    crc32::crc32(data)
}
