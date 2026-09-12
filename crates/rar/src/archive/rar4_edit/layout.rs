//! Main-header access and the block/layout scan.
//!
//! `Rar4Layout` is a parsed plaintext view of a single-volume archive: the
//! main header, every FILE_HEAD in order and the end-of-archive offset.
//! [`read_block_view`] transparently decrypts block headers on `-hp`
//! archives so callers see plaintext headers either way; [`emit_block`] is
//! its write-side counterpart (fresh salt per rebuilt header).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::header_crc16;
use crate::archive::RarArchive;
use crate::error::{RarError, RarResult};
use crate::format::rar4::{
    ENDARC_HEAD, FILE_HEAD, LONG_BLOCK, MAIN_HEAD, MHD_LOCK, MHD_PASSWORD, MHD_VOLUME,
};

/// Patch the main-header flags: OR `set_bits` into the flags word at
/// bytes 3..5 and recompute the CRC16. The main header is 13 bytes; when a
/// nested comment is present the reader's CRC coverage stops at 13 bytes,
/// which is exactly where the flags live, so recomputing over `[2..13]` is
/// correct with or without a comment.
pub(super) fn patch_main_header(main: &[u8], set_bits: u16) -> RarResult<Vec<u8>> {
    if main.len() < 13 || main[2] != MAIN_HEAD {
        return Err(RarError::Format(
            "RAR4: main header block is malformed".into(),
        ));
    }
    let flags = u16::from_le_bytes([main[3], main[4]]);
    let mut patched = main[..13].to_vec();
    patched[3..5].copy_from_slice(&(flags | set_bits).to_le_bytes());
    let crc = header_crc16(&patched[2..]);
    patched[..2].copy_from_slice(&crc.to_le_bytes());
    Ok(patched)
}

/// Parse the flags out of a raw main header block.
pub(super) fn main_flags(main: &[u8]) -> RarResult<u16> {
    if main.len() < 13 || main[2] != MAIN_HEAD {
        return Err(RarError::Format(
            "RAR4: main header block is malformed".into(),
        ));
    }
    Ok(u16::from_le_bytes([main[3], main[4]]))
}

/// The archive password, when one is set (an empty string counts as none).
/// `-hp` header encryption cannot be read or rewritten without it.
pub(super) fn header_password(archive: &RarArchive) -> Option<&str> {
    archive.password.as_deref().filter(|p| !p.is_empty())
}

/// Whether the archive on disk is `-hp` header-encrypted (the main header is
/// always plaintext and carries MHD_PASSWORD).
pub(super) fn archive_is_header_encrypted(archive: &RarArchive) -> RarResult<bool> {
    let (_offset, main_header) = read_main_from_file(&archive.path, archive.sfx_offset)?;
    Ok(main_flags(&main_header)? & MHD_PASSWORD != 0)
}

/// Read the archive's main header block (which starts 7 bytes after the
/// signature, i.e. at `sfx_offset + 7`), returning its file-absolute
/// offset and raw bytes. Works for `-hp` archives too: their main header
/// is the plaintext marker.
fn read_main_from_file(path: &Path, sfx_offset: u64) -> RarResult<(u64, Vec<u8>)> {
    let mut file = File::open(path).map_err(RarError::Io)?;
    file.seek(SeekFrom::Start(sfx_offset))?;
    let mut sig = [0u8; 7];
    file.read_exact(&mut sig).map_err(RarError::Io)?;
    if &sig != crate::detect::RAR4_SIGNATURE {
        return Err(RarError::Format(
            "RAR4: signature mismatch while editing".into(),
        ));
    }
    let mut prefix = [0u8; 7];
    file.read_exact(&mut prefix).map_err(RarError::Io)?;
    let head_size = u16::from_le_bytes([prefix[5], prefix[6]]) as usize;
    if head_size < 7 {
        return Err(RarError::Format(
            "RAR4: main header head_size too small".into(),
        ));
    }
    let mut header = prefix.to_vec();
    if head_size > 7 {
        let mut rest = vec![0u8; head_size - 7];
        file.read_exact(&mut rest).map_err(RarError::Io)?;
        header.extend_from_slice(&rest);
    }
    Ok((sfx_offset + 7, header))
}

/// Refuse edits on multi-volume RAR4 sets. Used by the append path: like
/// official `rar`, appending to a volume set is refused ("Cannot modify
/// volume"); header-level renames and lock are handled per volume instead.
pub(super) fn refuse_unsupported_containers(
    archive: &RarArchive,
    main_flags: u16,
) -> RarResult<()> {
    if archive.volume_paths.len() > 1 || main_flags & MHD_VOLUME != 0 {
        return Err(RarError::Unsupported(
            "editing multi-volume RAR4 archives is not supported".into(),
        ));
    }
    Ok(())
}

/// Lock the archive (`rar k`): set the MHD_LOCK bit in the main header and
/// recompute its CRC16, patching the 13-byte block in place. Locking is
/// irreversible; an already-locked archive is a no-op.
pub(crate) fn lock_archive(archive: &RarArchive) -> RarResult<()> {
    // Multi-volume sets lock the first volume's main header, which is what
    // official `rar k` does (the later volumes' headers stay untouched).
    let (main_offset, main_header) = read_main_from_file(&archive.path, archive.sfx_offset)?;
    let flags = main_flags(&main_header)?;
    if flags & MHD_LOCK != 0 {
        return Ok(()); // already locked; nothing to do
    }
    let patched = patch_main_header(&main_header, MHD_LOCK)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&archive.path)
        .map_err(RarError::Io)?;
    file.seek(SeekFrom::Start(main_offset))?;
    file.write_all(&patched).map_err(RarError::Io)?;
    file.sync_all().map_err(RarError::Io)?;
    Ok(())
}

// ── Layout scan ────────────────────────────────────────────────────────────

/// A parsed plaintext, single-volume RAR4 layout over an in-memory copy of
/// the archive.
pub(super) struct Rar4Layout {
    /// Offset where the archive signature starts (SFX stub length); the
    /// recovery-record sector grid is anchored here.
    pub(super) sfx_offset: usize,
    /// Absolute offset of the main header block.
    pub(super) main_offset: usize,
    /// Raw main header bytes (13 bytes, or longer when a nested comment is
    /// present).
    pub(super) main_header: Vec<u8>,
    /// Parsed main flags (as stored, LONG_BLOCK included).
    pub(super) main_flags: u16,
    /// The archive is `-hp` header-encrypted: every block after the main
    /// header is `[8B salt][AES-128-CBC header][plaintext data]`.
    pub(super) header_encrypted: bool,
    /// Absolute offset of the end-of-archive block.
    pub(super) endarc_offset: usize,
    /// Every FILE_HEAD block in archive order (file-absolute offset and raw
    /// header bytes). Archive-entry index `i` maps to `files[i]` on a
    /// single-volume archive (each member is one FILE_HEAD block).
    pub(super) files: Vec<(usize, Vec<u8>)>,
}

/// Read a block envelope at `pos`: `(head_size, flags, total_size)`.
fn block_envelope(bytes: &[u8], pos: usize) -> RarResult<(usize, u16, usize)> {
    if pos + 7 > bytes.len() {
        return Err(RarError::Format("RAR4: truncated block".into()));
    }
    let flags = u16::from_le_bytes([bytes[pos + 3], bytes[pos + 4]]);
    let head_size = u16::from_le_bytes([bytes[pos + 5], bytes[pos + 6]]) as usize;
    if head_size < 7 {
        return Err(RarError::Format("RAR4: block head_size too small".into()));
    }
    let add_size = if flags & LONG_BLOCK != 0 {
        if pos + 11 > bytes.len() {
            return Err(RarError::Format("RAR4: truncated block".into()));
        }
        u32::from_le_bytes(bytes[pos + 7..pos + 11].try_into().unwrap()) as usize
    } else {
        0
    };
    let total = head_size + add_size;
    if pos + total > bytes.len() {
        return Err(RarError::Format("RAR4: truncated block".into()));
    }
    Ok((head_size, flags, total))
}

/// One block of an in-memory RAR4 archive, with its header in plaintext
/// (decrypted when the archive is `-hp` header-encrypted).
pub(super) struct BlockView {
    pub(super) head_type: u8,
    /// Plaintext header bytes (`head_size` long, 7-byte prefix included).
    pub(super) header: Vec<u8>,
    /// Bytes the header occupies on disk: `head_size`, or
    /// `8 + align16(head_size)` for a header-encrypted block.
    pub(super) on_disk_header: usize,
    /// Bytes of data following the header (`add_size`).
    pub(super) add_size: usize,
    /// Total on-disk size of the block (`on_disk_header + add_size`).
    pub(super) total: usize,
}

impl BlockView {
    /// The block's data area (never encrypted: member payloads, parity...).
    pub(super) fn data<'a>(&self, bytes: &'a [u8], start: usize) -> &'a [u8] {
        &bytes[start + self.on_disk_header..start + self.total]
    }
}

/// Read the block at `pos`, transparently decrypting its header when
/// `password` is `Some` (the caller passes it only for `-hp` archives, and
/// only for blocks after the plaintext main header).
pub(super) fn read_block_view(
    bytes: &[u8],
    pos: usize,
    password: Option<&[u8]>,
) -> RarResult<BlockView> {
    let (header, on_disk_header, add_size, total) = match password {
        Some(password) => crate::format::rar4::decrypt_encrypted_header(bytes, pos, password)?,
        None => {
            let (head_size, _flags, total) = block_envelope(bytes, pos)?;
            (
                bytes[pos..pos + head_size].to_vec(),
                head_size,
                total - head_size,
                total,
            )
        }
    };
    Ok(BlockView {
        head_type: header[2],
        header,
        on_disk_header,
        add_size,
        total,
    })
}

/// Emit a block as `header` + `data`, encrypting the header with a fresh
/// salt when `password` is `Some` (`-hp`). The data area is never encrypted
/// by this helper: member payloads carry their own `-p` encryption and the
/// recovery record's tag/parity area must stay plaintext to remain
/// repairable.
pub(super) fn emit_block(
    out: &mut Vec<u8>,
    header: &[u8],
    data: &[u8],
    password: Option<&str>,
) -> RarResult<()> {
    match password {
        Some(password) => {
            let (ciphertext, _) =
                crate::format::rar4::write::encrypt_block_header(header, password)?;
            out.extend_from_slice(&ciphertext);
        }
        None => out.extend_from_slice(header),
    }
    out.extend_from_slice(data);
    Ok(())
}

/// Walk the block stream of an in-memory RAR4 archive, decrypting the
/// headers of a `-hp` archive with `password` (ignored for plaintext ones).
/// The archive must already be validated (the editor only reaches here after
/// a successful open scan), so headers are not CRC-checked again; only the
/// envelope bounds are.
pub(super) fn scan_layout(
    bytes: &[u8],
    sfx_offset: usize,
    password: Option<&str>,
) -> RarResult<Rar4Layout> {
    let sig = &bytes[sfx_offset..sfx_offset + 7];
    if sig != crate::detect::RAR4_SIGNATURE {
        return Err(RarError::Format(
            "RAR4: signature mismatch while editing".into(),
        ));
    }
    let mut pos = sfx_offset + 7;
    let mut main: Option<(usize, Vec<u8>, u16)> = None;
    let mut endarc: Option<usize> = None;
    let mut files = Vec::new();
    // Latched from the main header: `MHD_PASSWORD` means every later block
    // header is encrypted.
    let mut hp: Option<&[u8]> = None;
    while pos + 7 <= bytes.len() {
        let start = pos;
        let view = read_block_view(bytes, pos, hp)?;
        if view.head_type == MAIN_HEAD && main.is_none() {
            let flags = main_flags(&view.header)?;
            if flags & MHD_PASSWORD != 0 {
                let password = password.ok_or_else(|| {
                    RarError::Encrypted(
                        "editing a header-encrypted (-hp) RAR4 archive requires its password"
                            .into(),
                    )
                })?;
                hp = Some(password.as_bytes());
            }
            main = Some((start, view.header.clone(), flags));
        } else if view.head_type == ENDARC_HEAD {
            endarc = Some(start);
            break;
        } else if view.head_type == FILE_HEAD {
            files.push((start, view.header.clone()));
        }
        pos = start + view.total;
    }
    let (main_offset, main_header, main_flags) =
        main.ok_or_else(|| RarError::Format("RAR4: archive is missing its main header".into()))?;
    let endarc_offset = endarc.ok_or_else(|| {
        RarError::Format("RAR4: archive is missing its end-of-archive block".into())
    })?;
    Ok(Rar4Layout {
        sfx_offset,
        main_offset,
        main_header,
        main_flags,
        header_encrypted: hp.is_some(),
        endarc_offset,
        files,
    })
}
