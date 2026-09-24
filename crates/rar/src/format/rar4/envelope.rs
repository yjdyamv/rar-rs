//! The RAR 1.5–4.x block envelope, owned once: the `head_size`/`LONG_BLOCK`
//! guards, the 16-bit header CRC, the `-hp` header cipher, and the one
//! `read_block` every walk uses.
//!
//! Callers differ only in policy: a fresh scan verifies header CRCs, a
//! byte-exact rewriter keeps the exact on-disk header bytes, and the
//! recovery scanner tolerates damaged headers (no CRC check). The function
//! advances the stream past the block's data area, so a header-only walk
//! never seeks by hand.

use std::io::{Read, Seek, SeekFrom};

use super::{
    COMM_HEAD, FHD_COMMENT, FILE_HEAD, LONG_BLOCK, MAIN_HEAD, MARK_HEAD, file_header_crc_end,
};
use crate::crc32::crc32;
use crate::crypto::Rar30Cipher;
use crate::error::{RarError, RarResult};

/// A parsed RAR4 block envelope.
#[derive(Debug, Clone)]
pub(crate) struct Rar4Block {
    pub(crate) head_crc: u16,
    pub(crate) head_type: u8,
    pub(crate) flags: u16,
    /// Absolute offset where the block starts.
    pub(crate) offset: u64,
    /// Offset where the block's data area starts (block start + the header's
    /// on-disk byte count: `head_size`, or `8 + align16(head_size)` for
    /// `-hp` header-encrypted blocks).
    pub(crate) header_end: u64,
    /// Bytes on disk for this whole block (header + optional data area).
    pub(crate) total_size: u64,
    /// Data area length (`ADD_SIZE` when `LONG_BLOCK` is set, else `0`).
    pub(crate) add_size: u64,
    /// Plaintext header bytes (for validation / name parsing).
    pub(crate) header: Vec<u8>,
    /// Exact on-disk header bytes when [`EnvelopePolicy::retain_raw`] asked
    /// for them: identical to `header` for plaintext blocks,
    /// `[8-byte salt][ciphertext]` for `-hp` ones. `None` otherwise.
    pub(crate) raw_header: Option<Vec<u8>>,
}

impl Rar4Block {
    /// Bytes the header occupies on disk.
    pub(crate) fn on_disk_header(&self) -> u64 {
        self.header_end - self.offset
    }

    /// Absolute offset of the block's data area.
    pub(crate) fn data_offset(&self) -> u64 {
        self.header_end
    }

    /// Absolute offset just past the block.
    pub(crate) fn end(&self) -> u64 {
        self.offset + self.total_size
    }

    /// The exact on-disk header bytes, falling back to the plaintext header
    /// when the reader was not asked to retain them.
    pub(crate) fn raw_header(&self) -> &[u8] {
        self.raw_header.as_deref().unwrap_or(&self.header)
    }
}

/// What one block read verifies and keeps.
#[derive(Clone, Copy)]
pub(crate) struct EnvelopePolicy {
    /// Check the 16-bit `HEAD_CRC`. Callers whose headers were already
    /// validated (the edit layout scan) or may legitimately be damaged (the
    /// recovery scanner looking for the record to repair) pass `false`.
    pub(crate) verify_crc: bool,
    /// Keep the exact on-disk header bytes in `raw_header`; byte-exact
    /// rewriters need them, everyone else skips the copy.
    pub(crate) retain_raw: bool,
}

impl EnvelopePolicy {
    /// A fresh archive scan: CRC-checked headers, no raw copy.
    pub(crate) const SCAN: Self = Self {
        verify_crc: true,
        retain_raw: false,
    };
    /// A byte-exact rewrite: the open scan already validated the headers, so
    /// the CRC is not re-checked, and the raw bytes are kept.
    pub(crate) const EDIT: Self = Self {
        verify_crc: false,
        retain_raw: true,
    };
    /// A walk of an archive whose headers a previous pass already validated
    /// and whose raw bytes nobody needs (layout planning, comment lookup):
    /// no CRC check, no raw copy.
    pub(crate) const PLAN: Self = Self {
        verify_crc: false,
        retain_raw: false,
    };
    /// A damaged-archive scan: no CRC check, no raw copy.
    pub(crate) const REPAIR: Self = Self {
        verify_crc: false,
        retain_raw: false,
    };
}

/// Read one block header at the stream's position, decrypting it when
/// `encrypted` is set (`password` is then required). Returns `None` at a
/// clean end of stream and leaves the stream just past the block's data
/// area, so the next call reads the next block.
pub(crate) fn read_block<R: Read + Seek>(
    stream: &mut R,
    encrypted: bool,
    password: Option<&[u8]>,
    policy: EnvelopePolicy,
) -> RarResult<Option<Rar4Block>> {
    let start = stream.stream_position()?;
    let mut block = if encrypted {
        read_encrypted_block(stream, start, password, policy)?
    } else {
        read_plain_block(stream, start, policy)?
    };
    if let Some(block) = block.as_mut() {
        stream.seek(SeekFrom::Start(block.end()))?;
    }
    Ok(block)
}

/// Scan forward from `start` for the next structurally valid plaintext block
/// header, so a salvage scan can resync past a corrupt one. Each offset is
/// cheaply pre-filtered (a known head type and a `head_size` that fits the
/// file) before the header is read and its CRC-16 verified, so a corrupt
/// region costs a few bytes of reads per offset instead of a full header read.
/// Returns `Ok(None)` past the last block; on success the stream is left just
/// past the header, like [`read_block`].
pub(crate) fn resync_block<R: Read + Seek>(
    stream: &mut R,
    start: u64,
) -> RarResult<Option<Rar4Block>> {
    let end = stream.seek(SeekFrom::End(0))?;
    let mut pos = start;
    while pos < end {
        stream.seek(SeekFrom::Start(pos))?;
        if block_candidate(stream, pos, end)? {
            match read_block(stream, false, None, EnvelopePolicy::SCAN) {
                Ok(Some(block)) => return Ok(Some(block)),
                Ok(None) => return Ok(None),
                Err(RarError::Io(e)) => return Err(RarError::Io(e)),
                // Structurally plausible but not a real block (CRC failed):
                // keep scanning.
                Err(_) => {}
            }
        }
        pos += 1;
    }
    Ok(None)
}

/// Cheap pre-filter for [`resync_block`]: the bytes at `pos` could start a
/// plaintext block. RAR4 head types are `0x72..=0x7b` and `head_size` counts
/// the whole header. The position is restored, so the authoritative
/// [`read_block`] still starts at `pos`.
fn block_candidate<R: Read + Seek>(stream: &mut R, pos: u64, end: u64) -> RarResult<bool> {
    let mut base = [0u8; 7];
    if read_some(stream, &mut base)? < base.len() {
        return Ok(false);
    }
    let head_type = base[2];
    let head_size = u64::from(u16::from_le_bytes([base[5], base[6]]));
    let candidate = (0x72..=0x7b).contains(&head_type) && head_size >= 7 && pos + head_size <= end;
    stream.seek(SeekFrom::Start(pos))?;
    Ok(candidate)
}

/// Read a plaintext block header.
fn read_plain_block<R: Read + Seek>(
    stream: &mut R,
    start: u64,
    policy: EnvelopePolicy,
) -> RarResult<Option<Rar4Block>> {
    let mut base = [0u8; 7];
    let n = read_some(stream, &mut base)?;
    if n == 0 {
        return Ok(None);
    }
    if n < 7 {
        return Err(RarError::format("RAR4: truncated block header"));
    }
    let head_size = u16::from_le_bytes([base[5], base[6]]);
    if head_size < 7 {
        return Err(RarError::format(format!(
            "RAR4: block head_size {head_size} too small"
        )));
    }

    // Read the full header.
    let mut header = base.to_vec();
    if head_size as usize > 7 {
        let mut rest = vec![0u8; head_size as usize - 7];
        read_exact(stream, &mut rest).map_err(|err| match err {
            RarError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                RarError::format("RAR4: truncated block header")
            }
            other => other,
        })?;
        header.extend_from_slice(&rest);
    }
    let mut block = read_envelope(start, header, u64::from(head_size), policy.verify_crc)?;
    if policy.retain_raw {
        block.raw_header = Some(block.header.clone());
    }
    Ok(Some(block))
}

/// Read an `-hp` encrypted block: `[8-byte salt][AES-128-CBC ciphertext]`,
/// where the ciphertext holds the whole header (7-byte prefix included)
/// padded to a 16-byte multiple. The head size only becomes known after
/// decrypting the first block.
fn read_encrypted_block<R: Read + Seek>(
    stream: &mut R,
    start: u64,
    password: Option<&[u8]>,
    policy: EnvelopePolicy,
) -> RarResult<Option<Rar4Block>> {
    let Some(password) = password else {
        return Err(RarError::encrypted(
            "RAR4: header-encrypted archive, a password is required",
        ));
    };
    let Some((header, raw, on_disk_header)) = decrypt_header(stream, password)? else {
        return Ok(None);
    };
    // RAR4 header encryption has no MAC or password check value, so a header
    // that does not decrypt into a valid envelope can only be attributed to
    // the password (a genuinely corrupt header is indistinguishable). Map the
    // parse/CRC failure deterministically instead of leaking whichever error
    // the garbage bytes happened to hit — otherwise the same wrong password
    // reports `Crc` or `Format` depending on the random salt.
    let mut block = match read_envelope(start, header, on_disk_header, policy.verify_crc) {
        Ok(block) => block,
        Err(RarError::Crc { .. } | RarError::Format(_)) => {
            return Err(RarError::WrongPassword);
        }
        Err(other) => return Err(other),
    };
    if policy.retain_raw {
        block.raw_header = Some(raw);
    }
    Ok(Some(block))
}

/// Plaintext header, exact on-disk header bytes and on-disk header length.
type DecryptedHeader = (Vec<u8>, Vec<u8>, u64);

/// Decrypt one `-hp` header from `stream` (positioned at the block start):
/// `[8-byte salt][AES-128-CBC ciphertext]`, ciphertext padded to 16 bytes.
/// Returns the plaintext header, the exact on-disk header bytes and its
/// length; `None` at a clean end of stream. A read that runs past the end
/// after the first block decrypted surfaces as
/// [`RarError::WrongPassword`] (a wrong password yields a garbage head size
/// and the following read runs past the archive).
fn decrypt_header<R: Read>(stream: &mut R, password: &[u8]) -> RarResult<Option<DecryptedHeader>> {
    let mut first = [0u8; 24];
    let n = read_some(stream, &mut first)?;
    if n == 0 {
        return Ok(None);
    }
    if n < 24 {
        return Err(RarError::format("RAR4: truncated encrypted block header"));
    }
    let salt: [u8; 8] = first[..8].try_into().unwrap();
    let mut cipher = Rar30Cipher::new(password, Some(salt))
        .map_err(|e| RarError::format(format!("RAR4 header key setup: {e}")))?;
    let mut block0: [u8; 16] = first[8..24].try_into().unwrap();
    cipher
        .decrypt_in_place(&mut block0)
        .map_err(|e| RarError::format(format!("RAR4 header decrypt: {e}")))?;
    let head_size = u16::from_le_bytes([block0[5], block0[6]]);
    if head_size < 7 {
        // Garbage `head_size` from the first decrypted block: a wrong
        // password (see `read_encrypted_block` for why that is the only
        // deterministic attribution).
        return Err(RarError::WrongPassword);
    }
    let align16 = ((head_size as usize) + 15) & !15;
    let mut rest = vec![0u8; align16 - 16];
    read_exact(stream, &mut rest).map_err(|err| match err {
        // A wrong password yields garbage `head_size` from the first
        // decrypted block; following it reads past the end of the archive.
        // Surface that as a clear password problem instead of a bare I/O
        // error, mirroring the RAR5 wrong-password stage.
        RarError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => RarError::WrongPassword,
        other => other,
    })?;
    let mut raw = first.to_vec();
    raw.extend_from_slice(&rest);
    cipher
        .decrypt_in_place(&mut rest)
        .map_err(|e| RarError::format(format!("RAR4 header decrypt: {e}")))?;
    let mut header = block0.to_vec();
    header.extend_from_slice(&rest);
    header.truncate(head_size as usize);
    let on_disk_header = (8 + align16) as u64;
    Ok(Some((header, raw, on_disk_header)))
}

/// Parse and validate a block envelope from a full plaintext header. This is
/// the single RAR4 envelope parser: every caller (streaming scan, layout
/// re-scan, recovery scan) routes through it so the `head_size`/`LONG_BLOCK`
/// guards cannot diverge.
///
/// `on_disk_prefix` is the number of bytes the header occupies on disk
/// (`head_size` for plaintext blocks, `8 + align16(head_size)` when the
/// block was stored header-encrypted). `verify_crc` checks the 16-bit
/// `HEAD_CRC`: callers whose header is already validated by a previous pass
/// (the edit layout scan) or may legitimately be damaged (the recovery
/// scanner looking for the record to repair) pass `false`.
pub(crate) fn read_envelope(
    start: u64,
    header: Vec<u8>,
    on_disk_prefix: u64,
    verify_crc: bool,
) -> RarResult<Rar4Block> {
    if header.len() < 7 {
        return Err(RarError::format("RAR4: truncated block header"));
    }
    let head_crc = u16::from_le_bytes([header[0], header[1]]);
    let head_type = header[2];
    let flags = u16::from_le_bytes([header[3], header[4]]);
    let head_size = u16::from_le_bytes([header[5], header[6]]);
    if head_size < 7 {
        return Err(RarError::format(format!(
            "RAR4: block head_size {head_size} too small"
        )));
    }
    if header.len() < head_size as usize {
        return Err(RarError::format("RAR4: truncated block header"));
    }

    // Validate header CRC (16-bit) over bytes[2..head_size], except for
    // MARK (which has no meaningful CRC), AV/SIGN (documented bad), and
    // the 0xFFFF sentinel (RAR 1.5.4-era "no CRC" marker).
    if verify_crc {
        let should_check = !matches!(head_type, MARK_HEAD | 0x76 | 0x79) && head_crc != 0xFFFF;
        let crc_end = header_crc_end(&header, head_type, flags);
        if should_check {
            let actual = (crc32(&header[2..crc_end]) & 0xffff) as u16;
            if actual != head_crc {
                return Err(RarError::crc(
                    head_crc as u32,
                    actual as u32,
                    format!("RAR4 block type {head_type:#x} header"),
                ));
            }
        }
    }

    let add_size = if flags & LONG_BLOCK != 0 {
        if header.len() < 11 {
            return Err(RarError::format("RAR4: header missing LONG_BLOCK size"));
        }
        u64::from(u32::from_le_bytes(header[7..11].try_into().unwrap()))
    } else {
        0
    };

    Ok(Rar4Block {
        head_crc,
        head_type,
        flags,
        offset: start,
        header_end: start + on_disk_prefix,
        total_size: on_disk_prefix + add_size,
        add_size,
        header,
        raw_header: None,
    })
}

/// Where the header CRC coverage ends: some block types with a nested
/// comment stop before the comment (which has its own CRC).
fn header_crc_end(header: &[u8], head_type: u8, flags: u16) -> usize {
    match head_type {
        MAIN_HEAD if flags & 0x0002 != 0 => 13.min(header.len()),
        FILE_HEAD if flags & FHD_COMMENT != 0 => file_header_crc_end(header),
        // A standalone COMM_HEAD's `HEAD_SIZE` spans the 13-byte block
        // header plus its payload, but `HEAD_CRC` covers only the header
        // (unrar's `SIZEOF_COMMHEAD`); the payload is protected by the
        // block's own `COMM_CRC`.
        COMM_HEAD => 13.min(header.len()),
        _ => header.len(),
    }
}

fn read_some(stream: &mut impl Read, buf: &mut [u8]) -> RarResult<usize> {
    let mut read = 0;
    while read < buf.len() {
        let n = stream.read(&mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    Ok(read)
}

fn read_exact(stream: &mut impl Read, buf: &mut [u8]) -> RarResult<()> {
    stream.read_exact(buf).map_err(RarError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_block_retains_the_raw_header_and_advances_past_the_data_area() {
        // A FILE_HEAD with a 4-byte data area.
        let mut header = vec![0u8; 32];
        header[2] = FILE_HEAD;
        header[3..5].copy_from_slice(&LONG_BLOCK.to_le_bytes());
        header[5..7].copy_from_slice(&32u16.to_le_bytes());
        header[7..11].copy_from_slice(&4u32.to_le_bytes());
        let crc = (crc32(&header[2..]) & 0xffff) as u16;
        header[0..2].copy_from_slice(&crc.to_le_bytes());
        let mut bytes = header.clone();
        bytes.extend_from_slice(&[1, 2, 3, 4]);

        let mut stream = std::io::Cursor::new(bytes);
        let block = read_block(&mut stream, false, None, EnvelopePolicy::EDIT)
            .unwrap()
            .expect("first block");
        assert_eq!(block.head_type, FILE_HEAD);
        assert_eq!(block.add_size, 4);
        assert_eq!(block.raw_header().len(), 32);
        assert_eq!(stream.position(), block.end());

        // A policy that does not retain raw bytes falls back to the
        // plaintext header, which is identical for a plaintext block.
        let plan = read_block(
            &mut std::io::Cursor::new(header.clone()),
            false,
            None,
            EnvelopePolicy::PLAN,
        )
        .unwrap()
        .expect("plan block");
        assert!(plan.raw_header.is_none());
        assert_eq!(plan.raw_header(), plan.header.as_slice());

        // The stream now sits past the data area: the next read is a clean
        // end of input.
        assert!(
            read_block(&mut stream, false, None, EnvelopePolicy::EDIT)
                .unwrap()
                .is_none()
        );

        // A prefix claiming a 32-byte header with only 13 bytes left is a
        // truncation error, not a silent end of input.
        let mut cut = std::io::Cursor::new(header[..20].to_vec());
        let err = read_block(&mut cut, false, None, EnvelopePolicy::EDIT).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err:?}");
    }

    #[test]
    fn scan_policy_verifies_the_header_crc_and_repair_policy_does_not() {
        let mut header = vec![0u8; 32];
        header[2] = FILE_HEAD;
        header[5..7].copy_from_slice(&32u16.to_le_bytes());
        // Engineered CRC mismatch: bytes 2..32 left as zeros.
        let mut bytes = header.clone();
        bytes.extend_from_slice(&[0u8; 16]);

        let err = read_block(
            &mut std::io::Cursor::new(bytes.clone()),
            false,
            None,
            EnvelopePolicy::SCAN,
        )
        .unwrap_err();
        assert!(matches!(err, RarError::Crc { .. }), "got {err:?}");

        let block = read_block(
            &mut std::io::Cursor::new(bytes),
            false,
            None,
            EnvelopePolicy::REPAIR,
        )
        .unwrap()
        .expect("repair policy tolerates a bad CRC");
        assert_eq!(block.head_type, FILE_HEAD);
    }

    #[test]
    fn encrypted_header_truncated_after_the_first_cipher_block_is_a_wrong_password() {
        let mut header = vec![0u8; 32];
        header[2] = FILE_HEAD;
        header[3..5].copy_from_slice(&LONG_BLOCK.to_le_bytes());
        header[5..7].copy_from_slice(&32u16.to_le_bytes());
        header[7..11].copy_from_slice(&4u32.to_le_bytes());
        let (encrypted, _) =
            crate::format::rar4::write::encrypt_block_header(&header, "pw").unwrap();

        // Only the salt and the first cipher block survive: the head size
        // decrypted from it (32) promises 16 more cipher bytes, and reading
        // past the end of input is exactly what a wrong password looks like.
        let err = read_block(
            &mut std::io::Cursor::new(encrypted[..24].to_vec()),
            true,
            Some(b"pw"),
            EnvelopePolicy::PLAN,
        )
        .unwrap_err();
        assert!(matches!(err, RarError::WrongPassword), "got {err:?}");
    }

    #[test]
    fn encrypted_block_with_a_bad_header_crc_is_a_wrong_password() {
        // A wrong RAR4 password yields a plausible-looking head size often
        // enough that the parser reaches the header CRC. That path must
        // report WrongPassword too, not the Crc error the garbage happened
        // to hit (the random salt made this nondeterministic before).
        let mut header = vec![0u8; 32];
        header[2] = FILE_HEAD;
        header[3..5].copy_from_slice(&LONG_BLOCK.to_le_bytes());
        header[5..7].copy_from_slice(&32u16.to_le_bytes());
        header[7..11].copy_from_slice(&4u32.to_le_bytes());
        // Leave HEAD_CRC zero: the computed CRC is not zero, so the
        // decrypted header parses and then fails verification.
        let (encrypted, _) =
            crate::format::rar4::write::encrypt_block_header(&header, "pw").unwrap();

        let err = read_block(
            &mut std::io::Cursor::new(encrypted),
            true,
            Some(b"pw"),
            EnvelopePolicy::SCAN,
        )
        .unwrap_err();
        assert!(matches!(err, RarError::WrongPassword), "got {err:?}");
    }

    #[test]
    fn encrypted_block_with_an_impossible_head_size_is_a_wrong_password() {
        // A garbage head size below the 7-byte minimum is the other shape a
        // wrong password takes; it must also be WrongPassword rather than a
        // Format error.
        let header = vec![0u8; 16];
        let (encrypted, _) =
            crate::format::rar4::write::encrypt_block_header(&header, "pw").unwrap();

        let err = read_block(
            &mut std::io::Cursor::new(encrypted),
            true,
            Some(b"pw"),
            EnvelopePolicy::SCAN,
        )
        .unwrap_err();
        assert!(matches!(err, RarError::WrongPassword), "got {err:?}");
    }

    #[test]
    fn encrypted_edit_retains_the_ciphertext_and_yields_the_plaintext_header() {
        let mut header = vec![0u8; 32];
        header[2] = FILE_HEAD;
        header[3..5].copy_from_slice(&LONG_BLOCK.to_le_bytes());
        header[5..7].copy_from_slice(&32u16.to_le_bytes());
        header[7..11].copy_from_slice(&4u32.to_le_bytes());
        let (encrypted, on_disk) =
            crate::format::rar4::write::encrypt_block_header(&header, "pw").unwrap();

        let block = read_block(
            &mut std::io::Cursor::new(encrypted.clone()),
            true,
            Some(b"pw"),
            EnvelopePolicy::EDIT,
        )
        .unwrap()
        .expect("encrypted block");
        assert_eq!(block.header, header);
        assert_eq!(block.raw_header(), &encrypted[..]);
        assert_eq!(block.on_disk_header(), on_disk);
        assert_eq!(block.add_size, 4);
    }
}
