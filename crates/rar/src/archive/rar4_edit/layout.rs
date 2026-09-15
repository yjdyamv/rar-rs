//! Main-header access and the block/layout scan.
//!
//! `Rar4Layout` is a parsed plaintext view of an archive: the main header,
//! every FILE_HEAD in order, the end-of-archive offset and any legacy
//! recovery record. [`crate::format::rar4::read_block`] walks a `Read + Seek` source one
//! block at a time (bounded buffers, member payloads never buffered),
//! transparently decrypting block headers on `-hp` archives so callers see
//! plaintext headers either way; [`emit_block`] is its write-side counterpart
//! (fresh salt per rebuilt header), and [`copy_range`] streams verbatim
//! ranges between files.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::header_crc16;
use crate::archive::RarArchive;
use crate::error::{RarError, RarResult};
use crate::format::rar4::{
    ENDARC_HEAD, EnvelopePolicy, FILE_HEAD, LONG_BLOCK, MAIN_HEAD, MHD_LOCK, MHD_PASSWORD,
    MHD_VOLUME, NEWSUB_HEAD, Rar4Block, read_block,
};
use crate::fs::atomic::copy_prefix;

/// Main header flag: the archive comment is embedded after the fixed
/// 13-byte main header (RAR 1.5–2.9 layout). The reader's header-CRC
/// coverage stops at 13 bytes when it is set.
pub(super) const MHD_COMMENT: u16 = 0x0002;

/// Patch the main-header flags: OR `set_bits` into the flags word at
/// bytes 3..5 and recompute the CRC16 over the reader's coverage. The
/// fixed main header is 13 bytes, but RAR 1.5–2.9 embeds an archive
/// comment (`MHD_COMMENT`) right after it: the full header bytes are kept
/// (the following blocks must stay aligned), while the CRC still covers
/// only `[2..13]` — the reader stops there when the comment flag is set
/// (see `format::rar4::header_crc_end`).
pub(super) fn patch_main_header(main: &[u8], set_bits: u16) -> RarResult<Vec<u8>> {
    if main.len() < 13 || main[2] != MAIN_HEAD {
        return Err(RarError::Format(
            "RAR4: main header block is malformed".into(),
        ));
    }
    let flags = u16::from_le_bytes([main[3], main[4]]);
    let mut patched = main.to_vec();
    let new_flags = flags | set_bits;
    patched[3..5].copy_from_slice(&new_flags.to_le_bytes());
    let crc_end = if new_flags & MHD_COMMENT != 0 {
        13
    } else {
        patched.len()
    };
    let crc = header_crc16(&patched[2..crc_end]);
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

/// The first volume of the archive's set (`volume_paths[0]`); the opened
/// path when the catalog carries no volume list. Archive comments and the
/// lock bit live on the first volume regardless of which part was opened.
pub(super) fn first_volume(archive: &RarArchive) -> &Path {
    archive
        .volume_paths
        .first()
        .map(|path| path.as_path())
        .unwrap_or(archive.path.as_path())
}

/// Signature offset of the set's first volume: the opened archive's own
/// offset when it is the first volume, otherwise a bounded scan of the
/// first volume (which may carry an SFX stub).
pub(super) fn first_volume_signature_offset(archive: &RarArchive) -> RarResult<usize> {
    let first = first_volume(archive);
    if first == archive.path.as_path() {
        return Ok(archive.sfx_offset as usize);
    }
    let mut file = File::open(first).map_err(RarError::Io)?;
    locate_signature(&mut file)
}

/// Bounded scan for the RAR4 signature at the head of `stream` (a later
/// volume carries it at offset 0; the first volume may follow an SFX stub).
pub(super) fn locate_signature(stream: &mut File) -> RarResult<usize> {
    let file_len = stream.metadata().map_err(RarError::Io)?.len();
    let scan = usize::try_from(file_len.min(crate::detect::SFX_SCAN_LIMIT as u64))
        .map_err(|_| RarError::Format("RAR4: volume size overflows host address space".into()))?;
    let mut head = vec![0u8; scan];
    stream.seek(SeekFrom::Start(0))?;
    stream.read_exact(&mut head).map_err(RarError::Io)?;
    crate::detect::find_bytes(&head, crate::detect::RAR4_SIGNATURE)
        .ok_or_else(|| RarError::Format("RAR4: volume has no archive signature".into()))
}

/// Copy `len` bytes starting at `offset` from `reader` to `writer` with a
/// bounded buffer (used to stream member payloads and the kept tail).
pub(super) fn copy_range(
    reader: &mut File,
    writer: &mut File,
    offset: u64,
    len: u64,
) -> RarResult<()> {
    reader.seek(SeekFrom::Start(offset))?;
    copy_prefix(reader, writer, len).map_err(RarError::Io)?;
    Ok(())
}

/// Whether the archive on disk is `-hp` header-encrypted (the main header is
/// always plaintext and carries MHD_PASSWORD).
pub(super) fn archive_is_header_encrypted(archive: &RarArchive) -> RarResult<bool> {
    let (_offset, main_header) = read_main_from_file(&archive.path, archive.sfx_offset)?;
    Ok(main_flags(&main_header)? & MHD_PASSWORD != 0)
}

/// Whether the set's first volume carries the locked main-header flag: the
/// lock lives there regardless of which part is open, so every edit checks
/// it there.
pub(super) fn archive_is_locked(archive: &RarArchive) -> RarResult<bool> {
    let first = first_volume(archive);
    let sfx_offset = first_volume_signature_offset(archive)? as u64;
    let (_offset, main_header) = read_main_from_file(first, sfx_offset)?;
    Ok(main_flags(&main_header)? & MHD_LOCK != 0)
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
    let block = read_block(&mut file, false, None, EnvelopePolicy::PLAN)?
        .ok_or_else(|| RarError::Format("RAR4: missing main header".into()))?;
    Ok((block.offset, block.header))
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
    // official `rar k` does (the later volumes' headers stay untouched),
    // regardless of which part was opened.
    let first = first_volume(archive);
    let sfx_offset = first_volume_signature_offset(archive)? as u64;
    let (main_offset, main_header) = read_main_from_file(first, sfx_offset)?;
    let flags = main_flags(&main_header)?;
    if flags & MHD_LOCK != 0 {
        return Ok(()); // already locked; nothing to do
    }
    let patched = patch_main_header(&main_header, MHD_LOCK)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(first)
        .map_err(RarError::Io)?;
    file.seek(SeekFrom::Start(main_offset))?;
    file.write_all(&patched).map_err(RarError::Io)?;
    file.sync_all().map_err(RarError::Io)?;
    Ok(())
}

// ── Layout scan ────────────────────────────────────────────────────────────

/// A legacy recovery record ([`PROTECT_HEAD`](crate::format::rar4) 0x78 or
/// the NEWSUB `RR` 0x7a) located by the streaming layout scan.
pub(super) struct ProtectRecord {
    /// File-absolute offset where the recovery block starts.
    pub(super) block_offset: usize,
    /// File-absolute end of the recovery block's data area.
    pub(super) data_end: usize,
    /// Number of 512-byte parity sectors.
    pub(super) rec_sectors: u32,
    /// `Protect!` (0x78) or `Protect+` (0x7a).
    pub(super) mark: [u8; 8],
}

/// A parsed plaintext, single-volume RAR4 layout. The member headers are the
/// only bytes retained; file data is streamed past.
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
    /// The first recovery record found before the end-of-archive block.
    pub(super) protect: Option<ProtectRecord>,
}

/// Parse a legacy recovery-record block header. Mirrors
/// `recovery::legacy_rr::scan_protect_with_password`'s header checks without
/// touching the record's data area.
fn parse_protect_record(view: &Rar4Block) -> RarResult<Option<ProtectRecord>> {
    const PROTECT_HEAD: u8 = 0x78;
    let header = &view.header;
    let flags = u16::from_le_bytes([header[3], header[4]]);
    // RAR 2.5-era PROTECT_HEAD (0x78): 26-byte fixed header with the
    // `Protect!` mark in the last eight bytes.
    if view.head_type == PROTECT_HEAD
        && header.len() == 26
        && flags & LONG_BLOCK != 0
        && header.get(18..26) == Some(b"Protect!")
    {
        let rec_sectors = u16::from_le_bytes(header[12..14].try_into().unwrap());
        let total_blocks = u32::from_le_bytes(header[14..18].try_into().unwrap());
        if u64::from(total_blocks) * 2 + u64::from(rec_sectors) * 512 != view.add_size {
            return Err(RarError::Format(
                "RAR4: recovery data size does not match header".into(),
            ));
        }
        return Ok(Some(ProtectRecord {
            block_offset: view.offset as usize,
            data_end: view.end() as usize,
            rec_sectors: u32::from(rec_sectors),
            mark: [0x50, 0x72, 0x6f, 0x74, 0x65, 0x63, 0x74, 0x21], // "Protect!"
        }));
    }
    // RAR 3.x/4.x NEWSUB (0x7a) named `RR`: FILE_HEAD-shaped header whose
    // 20-byte tail after the name is `Protect+` + rec_sectors + total_blocks.
    if view.head_type == NEWSUB_HEAD
        && flags & LONG_BLOCK != 0
        && header.len() >= 32 + 2 + 20
        && header.get(32..34) == Some(b"RR")
    {
        let name_size = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
        let tail = 32 + name_size;
        if header.get(tail..tail + 8) != Some(b"Protect+") {
            return Ok(None);
        }
        let Some(rec_bytes) = header.get(tail + 8..tail + 12) else {
            return Err(RarError::Format("RAR4: recovery header truncated".into()));
        };
        let Some(total_bytes) = header.get(tail + 12..tail + 16) else {
            return Err(RarError::Format("RAR4: recovery header truncated".into()));
        };
        let rec_sectors = u32::from_le_bytes(rec_bytes.try_into().unwrap());
        let total_blocks = u32::from_le_bytes(total_bytes.try_into().unwrap());
        if u64::from(total_blocks) * 2 + u64::from(rec_sectors) * 512 != view.add_size {
            return Err(RarError::Format(
                "RAR4: recovery data size does not match header".into(),
            ));
        }
        return Ok(Some(ProtectRecord {
            block_offset: view.offset as usize,
            data_end: view.end() as usize,
            rec_sectors,
            mark: [0x50, 0x72, 0x6f, 0x74, 0x65, 0x63, 0x74, 0x2b], // "Protect+"
        }));
    }
    Ok(None)
}

/// Walk the block stream of a RAR4 archive from `stream` with bounded
/// buffers, decrypting the headers of a `-hp` archive with `password`
/// (ignored for plaintext ones). The archive must already be validated (the
/// editor only reaches here after a successful open scan), so headers are
/// not CRC-checked again; only the envelope bounds are.
pub(super) fn scan_layout_stream(
    stream: &mut (impl Read + Seek),
    sfx_offset: usize,
    password: Option<&str>,
) -> RarResult<Rar4Layout> {
    stream.seek(SeekFrom::Start(sfx_offset as u64))?;
    let mut sig = [0u8; 7];
    stream.read_exact(&mut sig).map_err(RarError::Io)?;
    if &sig != crate::detect::RAR4_SIGNATURE {
        return Err(RarError::Format(
            "RAR4: signature mismatch while editing".into(),
        ));
    }
    let mut main: Option<(usize, Vec<u8>, u16)> = None;
    let mut endarc: Option<usize> = None;
    let mut files = Vec::new();
    let mut protect = None;
    // Latched from the main header: `MHD_PASSWORD` means every later block
    // header is encrypted.
    let mut hp: Option<&[u8]> = None;
    while let Some(view) = read_block(stream, hp.is_some(), hp, EnvelopePolicy::PLAN)? {
        let start = view.offset as usize;
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
        } else if protect.is_none() {
            protect = parse_protect_record(&view)?;
        }
    }
    let (main_offset, main_header, main_flags) =
        main.ok_or_else(|| RarError::Format("RAR4: archive is missing its main header".into()))?;
    let endarc_offset = endarc.ok_or_else(|| {
        RarError::Format("RAR4: archive is missing the end-of-archive block".into())
    })?;
    Ok(Rar4Layout {
        sfx_offset,
        main_offset,
        main_header,
        main_flags,
        header_encrypted: hp.is_some(),
        endarc_offset,
        files,
        protect,
    })
}

/// [`scan_layout_stream`] over an in-memory archive copy (kept for the
/// layout/`-hp` tests, which build archives in memory).
#[cfg(test)]
pub(super) fn scan_layout(
    bytes: &[u8],
    sfx_offset: usize,
    password: Option<&str>,
) -> RarResult<Rar4Layout> {
    let mut cursor = std::io::Cursor::new(bytes);
    scan_layout_stream(&mut cursor, sfx_offset, password)
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
