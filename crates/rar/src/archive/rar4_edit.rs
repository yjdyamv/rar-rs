//! Header-level edits on existing RAR 1.5–4.x archives (ADR 0005, stage A).
//!
//! Three operations ship here: member rename (`rar rn`, and `rar ch` case
//! conversion which routes through the same rename path), inline
//! recovery-record add/replace (`rar rr`) and lock (`rar k`). All are pure
//! header/block surgery — no member data is decoded or recompressed, so
//! solid and non-solid archives are handled identically. The edits are
//! refused up front for multi-volume and header-encrypted (`-hp`)
//! archives, which the later stages of the RAR4 edit rollout will cover.
//!
//! Rename rebuilds each FILE_HEAD's encoded name field in place (keeping
//! every other field byte-identical, including salt / nested comment /
//! extended time) and recomputes the header CRC16 over the reader's
//! coverage. Directory renames expand to descendants, mirroring the RAR5
//! engine. Recovery (`rr`) and any edit on an archive that carries a
//! NEWSUB record rebuild that record over the new prefix — the tags are
//! offset-based, so a renamed header would otherwise leave stale tags.
//!
//! Lock (`k`) patches the 13-byte main header in place (mirroring the RAR5
//! lock). Recovery reads the whole archive into memory — the NEWSUB record
//! is XOR parity over the protected prefix, so the builder needs the
//! prefix anyway (the same bound the RAR4 creation path accepts) — and
//! stages a rewritten copy next to the archive before replacing it
//! atomically.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::RarArchive;
use super::transaction::EditSummary;
use crate::error::{RarError, RarResult};
use crate::format::rar4::write::encode_file_name;
use crate::format::rar4::{
    ENDARC_HEAD, FHD_COMMENT, FHD_LARGE, FHD_SALT, FHD_UNICODE, FILE_HEAD, LONG_BLOCK, MAIN_HEAD,
    MHD_LOCK, MHD_PASSWORD, MHD_RECOVERY, MHD_SOLID, MHD_VOLUME, NEWSUB_HEAD,
};
use crate::fs::atomic::{read_write_create, replace_file, temp_sibling_path};
use crate::recovery::legacy_rr::{
    build_legacy_recovery_block, recovery_sector_count, scan_protect,
};

/// RAR4 header CRC16: standard CRC-32 truncated to 16 bits over
/// `bytes[2..]` (the body after the CRC field).
fn header_crc16(body: &[u8]) -> u16 {
    (crate::crc32::crc32(body) & 0xffff) as u16
}

/// Patch the main-header flags: OR `set_bits` into the flags word at
/// bytes 3..5 and recompute the CRC16. The main header is 13 bytes; when a
/// nested comment is present the reader's CRC coverage stops at 13 bytes,
/// which is exactly where the flags live, so recomputing over `[2..13]` is
/// correct with or without a comment.
fn patch_main_header(main: &[u8], set_bits: u16) -> RarResult<Vec<u8>> {
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
fn main_flags(main: &[u8]) -> RarResult<u16> {
    if main.len() < 13 || main[2] != MAIN_HEAD {
        return Err(RarError::Format(
            "RAR4: main header block is malformed".into(),
        ));
    }
    Ok(u16::from_le_bytes([main[3], main[4]]))
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

/// Refuse edits on multi-volume RAR4 sets (the rewrite would need volume
/// rebalancing; see ADR 0005) and header-encrypted (`-hp`) archives (every
/// rebuilt or inserted block would need password re-encryption).
fn refuse_unsupported_containers(archive: &RarArchive, main_flags: u16) -> RarResult<()> {
    if archive.volume_paths.len() > 1 || main_flags & MHD_VOLUME != 0 {
        return Err(RarError::Unsupported(
            "editing multi-volume RAR4 archives is not supported".into(),
        ));
    }
    if main_flags & MHD_PASSWORD != 0 {
        return Err(RarError::Unsupported(
            "editing header-encrypted (-hp) RAR4 archives is not supported yet".into(),
        ));
    }
    Ok(())
}

/// Lock the archive (`rar k`): set the MHD_LOCK bit in the main header and
/// recompute its CRC16, patching the 13-byte block in place. Locking is
/// irreversible; an already-locked archive is a no-op.
pub(crate) fn lock_archive(archive: &RarArchive) -> RarResult<()> {
    if archive.volume_paths.len() > 1 {
        return Err(RarError::Unsupported(
            "locking multi-volume RAR4 archives is not supported (lock the first volume instead)"
                .into(),
        ));
    }
    let (main_offset, main_header) = read_main_from_file(&archive.path, archive.sfx_offset)?;
    let flags = main_flags(&main_header)?;
    refuse_unsupported_containers(archive, flags)?;
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
struct Rar4Layout {
    /// Offset where the archive signature starts (SFX stub length); the
    /// recovery-record sector grid is anchored here.
    sfx_offset: usize,
    /// Absolute offset of the main header block.
    main_offset: usize,
    /// Raw main header bytes (13 bytes, or longer when a nested comment is
    /// present).
    main_header: Vec<u8>,
    /// Parsed main flags (as stored, LONG_BLOCK included).
    main_flags: u16,
    /// Absolute offset of the end-of-archive block.
    endarc_offset: usize,
    /// Every FILE_HEAD block in archive order (file-absolute offset and raw
    /// header bytes). Archive-entry index `i` maps to `files[i]` on a
    /// single-volume archive (each member is one FILE_HEAD block).
    files: Vec<(usize, Vec<u8>)>,
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

/// Walk the plaintext block stream of an in-memory RAR4 archive. The
/// archive must already be validated (the editor only reaches here after a
/// successful open scan), so headers are not CRC-checked again; only the
/// envelope bounds are.
fn scan_layout(bytes: &[u8], sfx_offset: usize) -> RarResult<Rar4Layout> {
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
    while pos + 7 <= bytes.len() {
        let start = pos;
        let head_type = bytes[pos + 2];
        let (head_size, _flags, total) = block_envelope(bytes, pos)?;
        if head_type == MAIN_HEAD && main.is_none() {
            let header = bytes[start..start + head_size].to_vec();
            let flags = main_flags(&header)?;
            if flags & MHD_PASSWORD != 0 {
                return Err(RarError::Unsupported(
                    "editing header-encrypted (-hp) RAR4 archives is not supported yet".into(),
                ));
            }
            main = Some((start, header, flags));
        } else if head_type == ENDARC_HEAD {
            endarc = Some(start);
            break;
        } else if head_type == FILE_HEAD {
            files.push((start, bytes[start..start + head_size].to_vec()));
        }
        pos = start + total;
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
        endarc_offset,
        files,
    })
}

// ── Rename ─────────────────────────────────────────────────────────────────

/// Rebuild a FILE_HEAD block with a new encoded name: the fixed 32-byte
/// fields, high sizes, salt, nested comment and extended time are kept
/// byte-identical; only the name field (and the FHD_UNICODE bit / name
/// size) changes. The header CRC16 is recomputed over the reader's
/// coverage — for FILE_HEAD with a nested comment the coverage stops
/// before the comment, otherwise it covers the whole header body.
fn rename_file_header(header: &[u8], new_name: &str) -> RarResult<Vec<u8>> {
    if header.len() < 32 || header[2] != FILE_HEAD {
        return Err(RarError::Format(
            "RAR4: file header block is malformed".into(),
        ));
    }
    let flags = u16::from_le_bytes([header[3], header[4]]);
    let name_start = 32 + if flags & FHD_LARGE != 0 { 8 } else { 0 };
    if name_start + 2 > header.len() {
        return Err(RarError::Format(
            "RAR4: file header is missing its name field".into(),
        ));
    }
    let old_name_size = u16::from_le_bytes([header[26], header[27]]) as usize;
    let name_end = name_start + old_name_size;
    if name_end > header.len() {
        return Err(RarError::Format(
            "RAR4: file name extends past header".into(),
        ));
    }

    let (new_name_bytes, name_flags) = encode_file_name(new_name);
    let new_name_size = new_name_bytes.len();
    if new_name_size > u16::MAX as usize {
        return Err(RarError::InvalidOption(
            "RAR4: renamed member name is too long".into(),
        ));
    }
    let new_flags = (flags & !FHD_UNICODE) | name_flags;

    let mut out = Vec::with_capacity(header.len() - old_name_size + new_name_size);
    out.extend_from_slice(&header[..26]);
    out.extend_from_slice(&(new_name_size as u16).to_le_bytes());
    out.extend_from_slice(&header[28..name_start]);
    out.extend_from_slice(&new_name_bytes);
    out.extend_from_slice(&header[name_end..]);
    // The name grew or shrank: head_size moves with it (every other field
    // keeps its byte length).
    let new_head_size = out.len();
    if new_head_size > u16::MAX as usize {
        return Err(RarError::Format(
            "RAR4: renamed member header is too large".into(),
        ));
    }
    out[3..5].copy_from_slice(&new_flags.to_le_bytes());
    out[5..7].copy_from_slice(&(new_head_size as u16).to_le_bytes());

    let crc_end = if new_flags & FHD_COMMENT != 0 {
        let mut end = 32 + if new_flags & FHD_LARGE != 0 { 8 } else { 0 };
        end += new_name_size;
        if new_flags & FHD_SALT != 0 {
            end += 8;
        }
        end.min(out.len())
    } else {
        out.len()
    };
    let crc = header_crc16(&out[2..crc_end]);
    out[..2].copy_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// Resolve rename pairs (entry index -> new name) into a map, expanding
/// directory renames to their descendants exactly like the RAR5 engine
/// (`transaction.rs`): each directory member keeps a trailing `/`, every
/// other member whose name starts with `old/` is rewritten to `new/rest`,
/// and renaming the same member twice chains (the later pair sees the
/// earlier one's result as the old name).
fn build_rename_map(
    entries: &[crate::archive::ArchiveEntry],
    renames: &[(usize, String)],
) -> RarResult<(HashMap<usize, String>, usize)> {
    let mut map: HashMap<usize, String> = HashMap::new();
    let mut count = 0usize;
    for (idx, new) in renames {
        if *idx >= entries.len() {
            return Err(RarError::StaleEntryId);
        }
        let old_norm = map
            .get(idx)
            .map(|n| n.as_str())
            .unwrap_or(entries[*idx].name())
            .trim_end_matches('/')
            .to_string();
        let is_dir = entries[*idx].is_dir();
        let new_norm = new.trim_end_matches('/').to_string();
        if is_dir {
            map.insert(*idx, format!("{new_norm}/"));
            let prefix = format!("{old_norm}/");
            for (i, e) in entries.iter().enumerate() {
                if i == *idx || map.contains_key(&i) {
                    continue;
                }
                if let Some(rest) = e.name().strip_prefix(&prefix) {
                    map.insert(i, format!("{new_norm}/{rest}"));
                }
            }
        } else {
            map.insert(*idx, new_norm.clone());
        }
        count += 1;
    }
    Ok((map, count))
}

// ── Archive comment (RAR 3.x/4.x NEWSUB `CMT`) ─────────────────────────────

/// A RAR 3.x/4.x archive comment is a NEWSUB (0x7a) block named `CMT`,
/// placed right after the main header (WinRAR 6.23 layout). The payload is
/// either STORE bytes or a RAR29-LZSS stream (`method` 0x31–0x35). WinRAR
/// stores the comment as UTF-16LE (no BOM) when it carries characters
/// outside the single-byte range and as raw bytes otherwise; `rar cw`
/// writes the text back out.
///
/// Encode comment text for storage, mirroring WinRAR 6.23's convention:
/// the CMT block's `attr` bit 0 marks a UTF-16LE payload (no BOM); pure
/// ASCII comments are stored as raw bytes with the bit clear.
/// Returns `(payload, is_utf16)`.
fn encode_comment_text(bytes: &[u8]) -> (Vec<u8>, bool) {
    if bytes.is_ascii() {
        return (bytes.to_vec(), false);
    }
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    (out, true)
}

/// Decode a comment payload back to UTF-8 bytes. The block's `attr` bit 0
/// (WinRAR's unicode marker) selects UTF-16LE decoding; otherwise valid
/// UTF-8 is kept as-is and an even-length non-UTF-8 payload falls back to
/// the UTF-16 heuristic (for writers that omit the marker).
fn decode_comment_payload(payload: &[u8], unicode: bool) -> Vec<u8> {
    if unicode {
        let (units, _) = payload.as_chunks::<2>();
        let units: Vec<u16> = units
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units).into_bytes();
    }
    if std::str::from_utf8(payload).is_ok() || !payload.len().is_multiple_of(2) {
        return payload.to_vec();
    }
    let (units, _) = payload.as_chunks::<2>();
    let units: Vec<u16> = units
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if units.iter().any(|u| (0xD800..=0xDFFF).contains(u)) {
        return payload.to_vec();
    }
    String::from_utf16_lossy(&units).into_bytes()
}

/// Build a NEWSUB `CMT` block carrying `payload` uncompressed (method
/// STORE), shaped like WinRAR's comment block: a FILE_HEAD-form header
/// (32 fixed bytes + the 3-byte name `CMT`) followed by the payload.
/// `unicode` sets the comment's bit 0 of the attr field (WinRAR's marker
/// for a UTF-16LE payload).
fn build_comment_block(payload: &[u8], unicode: bool) -> Vec<u8> {
    let head_size = 32u16 + 3;
    let mut block = Vec::with_capacity(head_size as usize + payload.len());
    block.extend_from_slice(&[0u8, 0]); // header CRC, filled last
    block.push(0x7a); // NEWSUB_HEAD
    block.extend_from_slice(&LONG_BLOCK.to_le_bytes());
    block.extend_from_slice(&head_size.to_le_bytes());
    let len = payload.len() as u32;
    block.extend_from_slice(&len.to_le_bytes()); // packed
    block.extend_from_slice(&len.to_le_bytes()); // unpacked
    block.push(2); // host_os: Windows
    block.extend_from_slice(&crate::crc32::crc32(payload).to_le_bytes()); // file_crc
    block.extend_from_slice(&0u32.to_le_bytes()); // file_time
    block.push(29); // unp_ver
    block.push(crate::format::rar4::RAR4_METHOD_STORE); // method
    block.extend_from_slice(&3u16.to_le_bytes()); // name_size
    block.extend_from_slice(&u32::from(unicode).to_le_bytes()); // attr: bit 0 = UTF-16 payload
    block.extend_from_slice(b"CMT");
    block.extend_from_slice(payload);
    // Header CRC16 covers the 35-byte header body only (like WinRAR's
    // block, whose payload follows the covered region).
    let crc = header_crc16(&block[2..head_size as usize]);
    block[..2].copy_from_slice(&crc.to_le_bytes());
    block
}

/// Read the archive comment (`rar cw`): locate the NEWSUB `CMT` block and
/// decode its payload. Returns `None` when the archive has no comment.
/// Header-encrypted (`-hp`) archives are refused (their comment block is
/// encrypted with the header key).
pub(crate) fn read_comment(archive: &RarArchive) -> RarResult<Option<Vec<u8>>> {
    let bytes = fs::read(&archive.path).map_err(RarError::Io)?;
    let sfx_offset = archive.sfx_offset as usize;
    let sig = &bytes[sfx_offset..sfx_offset + 7];
    if sig != crate::detect::RAR4_SIGNATURE {
        return Err(RarError::Format(
            "RAR4: signature mismatch while reading the comment".into(),
        ));
    }
    let mut pos = sfx_offset + 7;
    let mut saw_main = false;
    while pos + 7 <= bytes.len() {
        let start = pos;
        let head_type = bytes[pos + 2];
        let (head_size, _flags, total) = block_envelope(&bytes, pos)?;
        if head_type == MAIN_HEAD && !saw_main {
            saw_main = true;
            if main_flags(&bytes[start..start + head_size])? & MHD_PASSWORD != 0 {
                return Err(RarError::Unsupported(
                    "reading the comment of a header-encrypted (-hp) RAR4 archive is not supported yet".into(),
                ));
            }
        } else if head_type == NEWSUB_HEAD && comment_block_name_is_cmt(&bytes, start, head_size) {
            let method = bytes[start + 25];
            let unp = u32::from_le_bytes(bytes[start + 11..start + 15].try_into().unwrap());
            let unicode =
                u32::from_le_bytes(bytes[start + 28..start + 32].try_into().unwrap()) & 1 != 0;
            let data = &bytes[start + head_size..start + total];
            let payload = if method == crate::format::rar4::RAR4_METHOD_STORE {
                data.to_vec()
            } else {
                decode_comment_stream(&bytes, start, head_size, total, unp as usize)?
            };
            return Ok(Some(decode_comment_payload(&payload, unicode)));
        }
        pos = start + total;
        if head_type == ENDARC_HEAD {
            break;
        }
    }
    Ok(None)
}

fn comment_block_name_is_cmt(bytes: &[u8], start: usize, _head_size: usize) -> bool {
    let ns = u16::from_le_bytes([bytes[start + 26], bytes[start + 27]]) as usize;
    bytes.get(start + 32..start + 32 + ns) == Some(b"CMT")
}

/// Decode a compressed comment payload through the shared RAR29 member
/// decoder (the payload is a plain single-chunk member stream).
fn decode_comment_stream(
    bytes: &[u8],
    start: usize,
    head_size: usize,
    total: usize,
    unpacked_size: usize,
) -> RarResult<Vec<u8>> {
    use crate::format::rar4::{MemberDecodeOptions, decode_member_bytes};
    use crate::model::{DataChunk, FileHeader};
    let method = bytes[start + 25];
    if !(0x31..=0x35).contains(&method) {
        return Err(RarError::Format(
            "RAR4: unsupported comment compression method".into(),
        ));
    }
    let data_offset = (start + head_size) as u64;
    let packed_size = (total - head_size) as u64;
    let hdr = FileHeader {
        unpacked_size: unpacked_size as u64,
        packed_size,
        comp_method: method.wrapping_sub(crate::format::rar4::RAR4_METHOD_STORE),
        data_offset,
        format_version: 4,
        unp_ver: 29,
        ..Default::default()
    };
    let chunk = DataChunk {
        volume_index: 0,
        data_offset,
        packed_size,
        crc32_val: None,
        is_final: true,
        extra_data: Vec::new(),
    };
    let stream = std::io::Cursor::new(bytes.to_vec());
    decode_member_bytes(
        &mut stream.clone(),
        &[],
        &[chunk],
        &hdr,
        MemberDecodeOptions {
            password: None,
            decoder: None,
            max_alloc_packed_bytes: 64 << 20,
            max_stream_packed_bytes: 64 << 20,
        },
    )
}

// ── Edit engine ────────────────────────────────────────────────────────────

/// Result of preparing an existing RAR4 archive for append: where the new
/// members start and whether a trailing NEWSUB record must be rebuilt.
pub(crate) struct AppendPrelude {
    /// Absolute byte offset where the append starts (the first byte of the
    /// trailing NEWSUB `RR` record, or of the end-of-archive block).
    pub truncate_pos: u64,
    /// Parity-sector count of the archive's NEWSUB record, if any (the
    /// record is dropped by the truncation and rebuilt at close with the
    /// same strength).
    pub rr_sectors: Option<u32>,
}

/// Prepare an existing single-volume, plaintext, non-solid RAR4 archive
/// for appending members. The archive's main flags gate the edit
/// (multi-volume, `-hp` and locked archives are refused; appending to a
/// solid archive needs the stage-C repack); the trailing NEWSUB recovery
/// record and the end-of-archive block are the truncation point.
pub(crate) fn append_prelude(archive: &RarArchive) -> RarResult<AppendPrelude> {
    let bytes = fs::read(&archive.path).map_err(RarError::Io)?;
    let layout = scan_layout(&bytes, archive.sfx_offset as usize)?;
    refuse_unsupported_containers(archive, layout.main_flags)?;
    if layout.main_flags & MHD_LOCK != 0 {
        return Err(RarError::ArchiveLocked);
    }
    if layout.main_flags & MHD_SOLID != 0 {
        return Err(RarError::Unsupported(
            "appending to solid RAR4 archives is not supported yet (needs the stage-C repack)"
                .into(),
        ));
    }
    // A trailing NEWSUB record sits between the last member and the
    // end-of-archive block; truncating at its start drops it (it cannot
    // protect members appended after it) and it is rebuilt at close. RAR
    // 2.5-era PROTECT_HEAD records cannot be rebuilt this way.
    let (truncate_pos, rr_sectors) = match scan_protect(&bytes)?.protect {
        Some(protect)
            if &protect.mark == b"Protect+" && protect.data_end <= layout.endarc_offset =>
        {
            (protect.block_offset as u64, Some(protect.rec_sectors))
        }
        Some(_) => {
            return Err(RarError::Unsupported(
                "RAR4: archives with a PROTECT_HEAD recovery record cannot be appended to in place; recreate the archive".into(),
            ));
        }
        None => (layout.endarc_offset as u64, None),
    };
    Ok(AppendPrelude {
        truncate_pos,
        rr_sectors,
    })
}

/// Apply one combined RAR4 edit transaction: delete members, rename
/// members, set/remove the archive comment, and/or add or rebuild the
/// recovery record, then atomically replace the archive and re-scan it.
/// All edits share one staged rewrite, so a failure leaves the original
/// file untouched.
///
/// Deleting members of a solid archive is refused (that needs the
/// decode->re-encode repack of stage C); non-solid archives drop the whole
/// FILE_HEAD + payload verbatim. Deleting every member erases the archive
/// file, matching `rar d`. `comment` mirrors the RAR5 engine's semantics:
/// `None` keeps the existing comment untouched, `Some(bytes)` installs it
/// (empty bytes remove it).
pub(crate) fn edit_rar4(
    archive: &mut RarArchive,
    deletes: &[usize],
    renames: &[(usize, String)],
    comment: Option<&[u8]>,
    force_rr: Option<u8>,
) -> RarResult<EditSummary> {
    if force_rr.is_some_and(|percent| percent > 100) {
        return Err(RarError::InvalidOption(
            "recovery percent must be in 0..=100".into(),
        ));
    }
    let bytes = fs::read(&archive.path).map_err(RarError::Io)?;
    let layout = scan_layout(&bytes, archive.sfx_offset as usize)?;
    refuse_unsupported_containers(archive, layout.main_flags)?;
    if layout.main_flags & MHD_LOCK != 0 {
        return Err(RarError::ArchiveLocked);
    }

    // Delete mask: duplicates are harmless; indexes past the catalog are
    // stale. Deleting members of a solid archive needs the stage-C repack.
    let mut deleted = vec![false; archive.entries.len()];
    let mut deleted_count = 0usize;
    for &idx in deletes {
        if idx >= deleted.len() {
            return Err(RarError::StaleEntryId);
        }
        if !deleted[idx] {
            deleted[idx] = true;
            deleted_count += 1;
        }
    }
    if deleted_count > 0 && layout.main_flags & MHD_SOLID != 0 {
        return Err(RarError::Unsupported(
            "deleting members of solid RAR4 archives is not supported yet (needs the stage-C repack)"
                .into(),
        ));
    }

    let (rename_map, renamed) = build_rename_map(&archive.entries, renames)?;
    for (idx, _) in renames {
        if deleted[*idx] {
            return Err(RarError::InvalidOption(
                "cannot rename a member that the same edit deletes".into(),
            ));
        }
    }
    if layout.files.len() != archive.entries.len() {
        return Err(RarError::Format(
            "RAR4: member layout does not match the scan (unsupported archive shape)".into(),
        ));
    }
    if layout.files.is_empty()
        && (deleted_count > 0 || rename_map.keys().next().is_some() || force_rr.is_some())
    {
        return Err(RarError::Format(
            "RAR4: archive has no members to edit".into(),
        ));
    }

    // Deleting every member erases the archive file (matching `rar d`);
    // comment and recovery-record changes would be silently dropped, so
    // they are refused too, exactly like the RAR5 engine.
    if deleted_count == archive.entries.len() {
        if force_rr.is_some() || comment.is_some() {
            return Err(RarError::InvalidOption(
                "cannot combine comment or recovery-record changes with deleting every member"
                    .into(),
            ));
        }
        std::fs::remove_file(&archive.path).map_err(RarError::Io)?;
        archive.entries.clear();
        return Ok(EditSummary {
            deleted: deleted_count,
            renamed: 0,
        });
    }

    // Decide the recovery-record action. A RAR 2.5-era PROTECT_HEAD record
    // (written after ENDARC, or with a non-NEWSUB mark) cannot be kept
    // valid through a prefix rewrite; refuse rather than leave a stale
    // record behind.
    let existing = match scan_protect(&bytes)?.protect {
        Some(protect)
            if &protect.mark == b"Protect+" && protect.data_end <= layout.endarc_offset =>
        {
            // Strip the old NEWSUB record; it lies entirely before the
            // end-of-archive block.
            Some((protect.block_offset, protect.data_end, protect.rec_sectors))
        }
        Some(_) => {
            return Err(RarError::Unsupported(
                "RAR4: archives with a PROTECT_HEAD recovery record cannot be edited in place; recreate the archive".into(),
            ));
        }
        None => None,
    };
    // `region_end`/`tail_from`: a rewrite with a record rebuilds the prefix
    // up to where the old record started (or the end-of-archive block) and
    // keeps everything from the old record's data end (or ENDARC) onward.
    let (region_end, tail_from, keep_sectors) = match (&force_rr, existing) {
        (_, Some((old_start, old_end, rec))) => (old_start, old_end, Some(rec)),
        (Some(_), None) => (layout.endarc_offset, layout.endarc_offset, None),
        (None, None) => (layout.endarc_offset, layout.endarc_offset, None),
    };
    let wants_record = force_rr.is_some() || keep_sectors.is_some();
    let patched_main = if wants_record {
        patch_main_header(&layout.main_header, MHD_RECOVERY)?
    } else {
        layout.main_header.clone()
    };
    let main_end = layout.main_offset + layout.main_header.len();
    let mut out = Vec::with_capacity(bytes.len() + 4096);
    out.extend_from_slice(&bytes[..layout.main_offset]);
    out.extend_from_slice(&patched_main);
    // A comment change lands its NEWSUB `CMT` block right after the main
    // header (WinRAR's placement). `Some(empty)` removes the comment.
    let replace_comment = comment.is_some();
    if let Some(bytes) = comment
        && !bytes.is_empty()
    {
        let (payload, unicode) = encode_comment_text(bytes);
        out.extend_from_slice(&build_comment_block(&payload, unicode));
    }

    let mut pos = main_end;
    let mut file_index = 0usize;
    while pos < region_end {
        let head_type = bytes[pos + 2];
        let (head_size, _flags, total) = block_envelope(&bytes, pos)?;
        if head_type == FILE_HEAD {
            if deleted[file_index] {
                // Drop the member's header and payload verbatim.
            } else if let Some(new_name) = rename_map.get(&file_index) {
                let rebuilt = rename_file_header(&bytes[pos..pos + head_size], new_name)?;
                out.extend_from_slice(&rebuilt);
                out.extend_from_slice(&bytes[pos + head_size..pos + total]);
            } else {
                out.extend_from_slice(&bytes[pos..pos + head_size]);
                out.extend_from_slice(&bytes[pos + head_size..pos + total]);
            }
            file_index += 1;
        } else if replace_comment
            && head_type == NEWSUB_HEAD
            && comment_block_name_is_cmt(&bytes, pos, head_size)
        {
            // A comment change replaces the existing CMT block (the new one
            // was already emitted after the main header).
        } else {
            out.extend_from_slice(&bytes[pos..pos + total]);
        }
        pos += total;
    }
    if pos != region_end {
        return Err(RarError::Format(
            "RAR4: block walk ended before the expected region end".into(),
        ));
    }

    // Append the recovery record when the plan wants one (a fresh record at
    // `percent`, or a rebuild keeping the original parity-sector strength),
    // then the tail (old record's data end onward, or ENDARC + trailing
    // bytes).
    if wants_record {
        let prefix = &out[layout.sfx_offset..];
        if prefix.is_empty() {
            return Err(RarError::Format(
                "RAR4: nothing to protect with a recovery record".into(),
            ));
        }
        let rec_sectors = match (force_rr, keep_sectors) {
            (Some(percent), _) => recovery_sector_count(prefix.len(), percent),
            (None, Some(rec)) => rec,
            (None, None) => unreachable!("wants_record implies a source"),
        };
        let block = build_legacy_recovery_block(prefix, rec_sectors)?;
        out.extend_from_slice(&block);
    }
    out.extend_from_slice(&bytes[tail_from..]);

    let tmp_path = temp_sibling_path(&archive.path);
    {
        let mut file = read_write_create(&tmp_path).map_err(RarError::Io)?;
        file.write_all(&out).map_err(RarError::Io)?;
        file.sync_all().map_err(RarError::Io)?;
    }
    if let Err(error) = replace_file(&tmp_path, &archive.path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    // Re-scan the rewritten archive so the in-memory catalog matches the
    // file and every rebuilt header CRC is validated against the new bytes.
    archive.open_read()?;
    Ok(EditSummary {
        deleted: deleted_count,
        renamed,
    })
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)] // legacy facade namelist/read kept for byte-parity checks
    use super::*;
    use crate::format::rar4::RAR4_METHOD_STORE;
    use crate::format::rar4::write::{FileHeaderParams, build_endarc, build_file_header};

    fn file_block(name: &str, payload: &[u8]) -> Vec<u8> {
        let (name_bytes, _flags) = encode_file_name(name);
        let mut h = build_file_header(&FileHeaderParams {
            flags: 0,
            packed_size: payload.len() as u32,
            unpacked_size: payload.len() as u32,
            host_os: 0,
            file_crc: crate::crc32::crc32(payload),
            file_time: 0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: &name_bytes,
            attr: 0x20,
            window_bits: 0,
            salt: None,
            ext_time: None,
        })
        .unwrap();
        h.extend_from_slice(payload);
        h
    }

    fn archive_bytes(member_blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = crate::detect::RAR4_SIGNATURE.to_vec();
        out.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
        for block in member_blocks {
            out.extend_from_slice(block);
        }
        out.extend_from_slice(&build_endarc(0));
        out
    }

    #[test]
    fn patch_main_header_sets_bits_and_keeps_crc_valid() {
        let main = crate::format::rar4::write::build_main_header(0);
        assert_eq!(main.len(), 13);
        let patched = patch_main_header(&main, MHD_LOCK | MHD_RECOVERY).unwrap();
        let flags = u16::from_le_bytes([patched[3], patched[4]]);
        assert_ne!(flags & MHD_LOCK, 0);
        assert_ne!(flags & MHD_RECOVERY, 0);
        let crc = header_crc16(&patched[2..]);
        assert_eq!(u16::from_le_bytes([patched[0], patched[1]]), crc);
        assert_eq!(&patched[5..], &main[5..]);
    }

    #[test]
    fn rename_file_header_swaps_the_name_field_only() {
        let (name_bytes, name_flags) = encode_file_name("plain.txt");
        let header = build_file_header(&FileHeaderParams {
            flags: name_flags,
            packed_size: 100,
            unpacked_size: 100,
            host_os: 0,
            file_crc: 0x1234_5678,
            file_time: 0x9abc_def0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: &name_bytes,
            attr: 0x20,
            window_bits: 0,
            salt: Some([0x11; 8]),
            ext_time: Some(&[1, 2, 3, 4]),
        })
        .unwrap();
        let renamed = rename_file_header(&header, "重命名-ünï.txt").unwrap();

        // Fields outside the name (fixed 32-byte block + salt/ext-time tail)
        // are preserved byte-for-byte; only CRC, flags, head_size and the
        // name_size/name pair change.
        assert_eq!(&renamed[7..26], &header[7..26]);
        assert_eq!(&renamed[28..32], &header[28..32]);
        let new_hs = u16::from_le_bytes([renamed[5], renamed[6]]) as usize;
        assert_eq!(new_hs, renamed.len());
        // The new name decodes back through the reader path.
        let flags = u16::from_le_bytes([renamed[3], renamed[4]]);
        assert_ne!(flags & FHD_UNICODE, 0);
        let name_size = u16::from_le_bytes([renamed[26], renamed[27]]) as usize;
        let decoded = crate::format::rar4::decode_file_name(&renamed[32..32 + name_size], flags);
        assert_eq!(decoded, "重命名-ünï.txt");
        // CRC16 covers the whole (comment-free) header body, like the reader.
        let crc = header_crc16(&renamed[2..]);
        assert_eq!(u16::from_le_bytes([renamed[0], renamed[1]]), crc);
    }

    #[test]
    fn rename_members_roundtrips_through_open_and_keeps_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rename.rar");
        let a_payload = vec![0x42; 5_000];
        let b_payload = vec![0x77; 3_000];
        std::fs::write(
            &path,
            archive_bytes(&[
                file_block("a.bin", &a_payload),
                file_block("b.txt", &b_payload),
            ]),
        )
        .unwrap();

        let archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.namelist(), ["a.bin", "b.txt"]);

        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.bin").unwrap();
        let b = editor.unique_entry("b.txt").unwrap();
        let report = editor
            .apply(
                crate::archive::editor::EditPlan::new()
                    .rename(a, "alpha.bin")
                    .rename(b, "贝塔.txt"),
            )
            .unwrap();
        assert_eq!(report.renamed(), 2);

        drop(editor);
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.namelist(), ["alpha.bin", "贝塔.txt"]);
        assert_eq!(archive.read("alpha.bin").unwrap(), a_payload);
        assert_eq!(archive.read("贝塔.txt").unwrap(), b_payload);
    }

    #[test]
    fn dir_rename_expands_to_descendants_and_rebuilds_rr() {
        // Hand-build: d/ (directory member) + d/x.txt, plus a NEWSUB `rr`
        // record over the prefix and an ENDARC, mirroring the writer's
        // layout (members, RR, ENDARC).
        let (dir_name, _) = encode_file_name("d/");
        let dir_head = build_file_header(&FileHeaderParams {
            flags: 0,
            packed_size: 0,
            unpacked_size: 0,
            host_os: 2,
            file_crc: 0,
            file_time: 0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: &dir_name,
            attr: 0x10, // FILE_ATTRIBUTE_DIRECTORY
            window_bits: 0,
            salt: None,
            ext_time: None,
        })
        .unwrap();
        let x_payload = vec![0x5a; 2_000];
        let mut prefix = crate::detect::RAR4_SIGNATURE.to_vec();
        prefix.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
        prefix.extend_from_slice(&dir_head);
        prefix.extend_from_slice(&file_block("d/x.txt", &x_payload));
        let rec = recovery_sector_count(prefix.len(), 10);
        let rr_block = build_legacy_recovery_block(&prefix, rec).unwrap();
        let mut full = prefix;
        full.extend_from_slice(&rr_block);
        full.extend_from_slice(&build_endarc(0));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dirs.rar");
        std::fs::write(&path, &full).unwrap();

        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let names: Vec<String> = editor.entries().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["d/", "d/x.txt"]);
        let d = editor.unique_entry("d/").unwrap();
        let report = editor
            .apply(crate::archive::editor::EditPlan::new().rename(d, "renamed"))
            .unwrap();
        assert_eq!(report.renamed(), 1, "one explicit pair");

        drop(editor);
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.namelist(), ["renamed/", "renamed/x.txt"]);
        assert_eq!(archive.read("renamed/x.txt").unwrap(), x_payload);

        // The archive still carries a valid, repairable recovery record
        // (rebuilt over the renamed prefix): damage a protected sector and
        // repair it back byte-for-byte.
        let rewritten = std::fs::read(&path).unwrap();
        assert!(scan_protect(&rewritten).unwrap().protect.is_some());
        let mut damaged = rewritten.clone();
        damaged[1_000..1_064].fill(0xcc);
        let damaged_path = dir.path().join("dmg.rar");
        std::fs::write(&damaged_path, &damaged).unwrap();
        let fixed_path = dir.path().join("fixed.rar");
        assert!(crate::recovery::repair_legacy_archive_path(&damaged_path, &fixed_path).unwrap());
        assert_eq!(std::fs::read(&fixed_path).unwrap(), rewritten);
    }

    /// A genuine WinRAR 6.23 RAR4 archive carrying a UTF-8 comment stored as
    /// UTF-16LE must decode back to the exact text `rar cw` would emit.
    #[test]
    fn reads_winrar_623_rar4_comment() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/rar40/comment/comment_zh.rar"
        );
        let expected = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/rar40/comment/comment.txt"
        ))
        .unwrap();
        let mut archive = RarArchive::open(fixture).unwrap();
        assert_eq!(archive.get_comment().unwrap(), Some(expected));
    }

    #[test]
    fn comment_set_replace_and_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cmt.rar");
        let payload = vec![0x44; 9_000];
        std::fs::write(&path, archive_bytes(&[file_block("a.bin", &payload)])).unwrap();

        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        // No comment yet.
        {
            let mut archive = RarArchive::open(&path).unwrap();
            assert_eq!(archive.get_comment().unwrap(), None);
        }
        // Set an ASCII comment.
        editor
            .apply(crate::archive::editor::EditPlan::new().set_comment(b"first comment"))
            .unwrap();
        {
            let mut archive = RarArchive::open(&path).unwrap();
            assert_eq!(
                archive.get_comment().unwrap(),
                Some(b"first comment".to_vec())
            );
        }
        // Replace it with a Unicode one, combined with rr in the same plan.
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.bin").unwrap();
        editor
            .apply(
                crate::archive::editor::EditPlan::new()
                    .set_comment("第二段注释 ünï".as_bytes())
                    .set_recovery(10)
                    .rename(a, "renamed.bin"),
            )
            .unwrap();
        {
            let mut archive = RarArchive::open(&path).unwrap();
            assert_eq!(archive.namelist(), ["renamed.bin"]);
            assert_eq!(
                archive.get_comment().unwrap(),
                Some("第二段注释 ünï".as_bytes().to_vec())
            );
            // The combined rewrite kept the recovery record repairable.
            let bytes = std::fs::read(&path).unwrap();
            assert!(scan_protect(&bytes).unwrap().protect.is_some());
        }
        // Empty comment removes it.
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        editor
            .apply(crate::archive::editor::EditPlan::new().set_comment(Vec::new()))
            .unwrap();
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.get_comment().unwrap(), None);
    }

    #[test]
    fn comment_encode_decode_symmetry() {
        for text in [b"plain ascii".as_slice(), "第二段注释 ünï".as_bytes(), b""] {
            let (payload, unicode) = encode_comment_text(text);
            assert_eq!(decode_comment_payload(&payload, unicode), text);
            assert_eq!(unicode, !text.is_ascii());
        }
        // A WinRAR UTF-16LE payload (attr marker set) decodes to the text.
        let utf16: Vec<u8> = "中文测试"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_comment_payload(&utf16, true), "中文测试".as_bytes());
        // Without the marker a valid-UTF-8 payload is returned untouched.
        assert_eq!(
            decode_comment_payload("中文测试".as_bytes(), false),
            "中文测试".as_bytes()
        );
    }
}

#[cfg(test)]
mod delete_tests {
    #![allow(deprecated)] // legacy facade namelist/read kept for byte-parity checks
    use super::*;
    use crate::format::rar4::RAR4_METHOD_STORE;
    use crate::format::rar4::write::{FileHeaderParams, build_endarc, build_file_header};

    fn file_block(name: &str, payload: &[u8]) -> Vec<u8> {
        let (name_bytes, _flags) = encode_file_name(name);
        let mut h = build_file_header(&FileHeaderParams {
            flags: 0,
            packed_size: payload.len() as u32,
            unpacked_size: payload.len() as u32,
            host_os: 0,
            file_crc: crate::crc32::crc32(payload),
            file_time: 0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: &name_bytes,
            attr: 0x20,
            window_bits: 0,
            salt: None,
            ext_time: None,
        })
        .unwrap();
        h.extend_from_slice(payload);
        h
    }

    fn archive_bytes(member_blocks: &[Vec<u8>], with_rr: bool) -> Vec<u8> {
        let mut prefix = crate::detect::RAR4_SIGNATURE.to_vec();
        prefix.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
        for block in member_blocks {
            prefix.extend_from_slice(block);
        }
        let mut out = prefix;
        if with_rr {
            let rec = recovery_sector_count(out.len(), 10);
            let rr = build_legacy_recovery_block(&out, rec).unwrap();
            out.extend_from_slice(&rr);
        }
        out.extend_from_slice(&build_endarc(0));
        out
    }

    fn payloads() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (vec![0x41; 3_000], vec![0x42; 2_000], vec![0x43; 1_500])
    }

    #[test]
    fn delete_members_keeps_others_and_rebuilds_rr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("del.rar");
        let (p1, p2, p3) = payloads();
        std::fs::write(
            &path,
            archive_bytes(
                &[
                    file_block("a.bin", &p1),
                    file_block("b.bin", &p2),
                    file_block("c.bin", &p3),
                ],
                true,
            ),
        )
        .unwrap();

        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let b = editor.unique_entry("b.bin").unwrap();
        let report = editor
            .apply(crate::archive::editor::EditPlan::new().delete(b))
            .unwrap();
        assert_eq!((report.deleted(), report.renamed()), (1, 0));

        drop(editor);
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.namelist(), ["a.bin", "c.bin"]);
        assert_eq!(archive.read("a.bin").unwrap(), p1);
        assert_eq!(archive.read("c.bin").unwrap(), p3);

        // The rebuilt record still protects the new prefix.
        let bytes = std::fs::read(&path).unwrap();
        assert!(scan_protect(&bytes).unwrap().protect.is_some());
        let mut damaged = bytes.clone();
        damaged[600..680].fill(0x33);
        let damaged_path = dir.path().join("dmg.rar");
        std::fs::write(&damaged_path, &damaged).unwrap();
        let fixed_path = dir.path().join("fixed.rar");
        assert!(crate::recovery::repair_legacy_archive_path(&damaged_path, &fixed_path).unwrap());
        assert_eq!(std::fs::read(&fixed_path).unwrap(), bytes);
    }

    #[test]
    fn delete_first_and_last_members() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("del2.rar");
        let (p1, p2, p3) = payloads();
        std::fs::write(
            &path,
            archive_bytes(
                &[
                    file_block("a.bin", &p1),
                    file_block("b.bin", &p2),
                    file_block("c.bin", &p3),
                ],
                false,
            ),
        )
        .unwrap();

        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.bin").unwrap();
        let c = editor.unique_entry("c.bin").unwrap();
        assert_eq!(
            editor.delete_entries(&[a, c]).unwrap(),
            2,
            "delete a.bin and c.bin"
        );
        drop(editor);
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.namelist(), ["b.bin"]);
        assert_eq!(archive.read("b.bin").unwrap(), p2);
    }

    #[test]
    fn delete_every_member_erases_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("del3.rar");
        let (p1, _, _) = payloads();
        std::fs::write(&path, archive_bytes(&[file_block("a.bin", &p1)], false)).unwrap();
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.bin").unwrap();
        assert_eq!(editor.delete_entries(&[a]).unwrap(), 1);
        assert!(!path.exists(), "deleting every member erases the archive");
        drop(editor);
        assert!(RarArchive::open(&path).is_err());
    }

    #[test]
    fn delete_conflicts_and_solid_are_refused_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("del4.rar");
        let (p1, p2, _) = payloads();
        std::fs::write(
            &path,
            archive_bytes(&[file_block("a.bin", &p1), file_block("b.bin", &p2)], false),
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.bin").unwrap();
        // Deleting and renaming the same member in one plan is rejected.
        assert!(matches!(
            editor.apply(
                crate::archive::editor::EditPlan::new()
                    .delete(a)
                    .rename(a, "x.bin")
            ),
            Err(RarError::InvalidOption(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before, "untouched");

        // Solid archives refuse deletion (stage C repack).
        let solid = dir.path().join("solid.rar");
        let mut solid_archive = crate::archive::RarArchive::create_with_options(
            &solid,
            crate::options::CreateOptions {
                compression: crate::version::ArchiveVersion::V29,
                solid: true,
                ..Default::default()
            },
        )
        .unwrap();
        solid_archive.add_bytes("m1.bin", &p1, 3).unwrap();
        solid_archive.add_bytes("m2.bin", &p2, 3).unwrap();
        solid_archive.close().unwrap();
        let mut editor = crate::archive::editor::ArchiveEditor::open(&solid).unwrap();
        let m1 = editor.unique_entry("m1.bin").unwrap();
        assert!(matches!(
            editor.delete_entries(&[m1]),
            Err(RarError::Unsupported(_))
        ));
    }
}

#[cfg(test)]
mod append_tests {
    #![allow(deprecated)] // legacy facade add_bytes/close kept for parity
    use super::*;

    fn build_rar4(path: &std::path::Path, payloads: &[(&str, &[u8])]) {
        let mut a = crate::archive::RarArchive::create_with_options(
            path,
            crate::options::CreateOptions {
                compression: crate::version::ArchiveVersion::V29,
                ..Default::default()
            },
        )
        .unwrap();
        for (name, data) in payloads {
            a.add_bytes(name, data, 0).unwrap();
        }
        a.close().unwrap();
    }

    #[test]
    fn append_adds_members_and_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ap.rar");
        let p1 = vec![0x11; 4_000];
        let p2 = vec![0x22; 3_000];
        let p3 = vec![0x33; 2_000];
        build_rar4(&path, &[("a.bin", &p1), ("b.bin", &p2)]);
        {
            let mut a = crate::archive::RarArchive::open_append(&path).unwrap();
            a.add_bytes("c.bin", &p3, 0).unwrap();
            a.close().unwrap();
        }
        let mut a = crate::archive::RarArchive::open(&path).unwrap();
        assert_eq!(a.namelist(), ["a.bin", "b.bin", "c.bin"]);
        assert_eq!(a.read("a.bin").unwrap(), p1);
        assert_eq!(a.read("b.bin").unwrap(), p2);
        assert_eq!(a.read("c.bin").unwrap(), p3);
    }

    #[test]
    fn append_rebuilds_existing_rr_over_the_new_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ap2.rar");
        let p1 = vec![0x44; 40_000];
        let p2 = vec![0x55; 30_000];
        build_rar4(&path, &[("a.bin", &p1)]);
        {
            let mut e = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
            e.apply(crate::archive::editor::EditPlan::new().set_recovery(10))
                .unwrap();
        }
        {
            let mut a = crate::archive::RarArchive::open_append(&path).unwrap();
            a.add_bytes("b.bin", &p2, 0).unwrap();
            a.close().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(scan_protect(&bytes).unwrap().protect.is_some());
        // Damage a protected sector inside the appended member: the rebuilt
        // record must restore the exact original bytes.
        let mut damaged = bytes.clone();
        let at = bytes.len() - 8_000;
        damaged[at..at + 64].fill(0x7e);
        let dmg_path = dir.path().join("dmg.rar");
        std::fs::write(&dmg_path, &damaged).unwrap();
        let fixed = dir.path().join("fixed.rar");
        assert!(crate::recovery::repair_legacy_archive_path(&dmg_path, &fixed).unwrap());
        assert_eq!(std::fs::read(&fixed).unwrap(), bytes);
        let mut a = crate::archive::RarArchive::open(&path).unwrap();
        assert_eq!(a.namelist(), ["a.bin", "b.bin"]);
        assert_eq!(a.read("b.bin").unwrap(), p2);
    }

    #[test]
    fn append_refuses_solid_locked_and_hp() {
        let dir = tempfile::tempdir().unwrap();
        let p = vec![0x66; 2_000];
        // Solid archive.
        let solid = dir.path().join("solid.rar");
        {
            let mut a = crate::archive::RarArchive::create_with_options(
                &solid,
                crate::options::CreateOptions {
                    compression: crate::version::ArchiveVersion::V29,
                    solid: true,
                    ..Default::default()
                },
            )
            .unwrap();
            a.add_bytes("m.bin", &p, 3).unwrap();
            a.close().unwrap();
        }
        assert!(matches!(
            crate::archive::RarArchive::open_append(&solid),
            Err(RarError::Unsupported(_))
        ));
        // Locked archive.
        let locked = dir.path().join("locked.rar");
        build_rar4(&locked, &[("m.bin", &p)]);
        {
            let mut e = crate::archive::editor::ArchiveEditor::open(&locked).unwrap();
            e.lock().unwrap();
        }
        assert!(matches!(
            crate::archive::RarArchive::open_append(&locked),
            Err(RarError::ArchiveLocked)
        ));
    }
}
