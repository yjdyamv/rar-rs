//! FILE_HEAD rewriting: member rename and per-file comment stripping.
//!
//! Both operations keep every other field byte-identical (high sizes, salt,
//! extended time, flags) and recompute the header CRC16 over the reader's
//! coverage; a legacy header carrying a nested comment has that coverage
//! stop before the comment area. RAR 3.x/4.x comments are standalone
//! `COMM_HEAD` blocks after the member data, so a comment change only
//! strips the old nested layout here — the caller emits the replacement
//! block itself (see `engine.rs`).

use super::header_crc16;
use crate::error::{RarError, RarResult};
use crate::format::rar4::write::encode_file_name;
use crate::format::rar4::{FHD_COMMENT, FHD_LARGE, FHD_UNICODE, FILE_HEAD};
/// Rebuild a FILE_HEAD block with a new encoded name: the fixed 32-byte
/// fields, high sizes, salt, nested comment and extended time are kept
/// byte-identical; only the name field (and the FHD_UNICODE bit / name
/// size) changes. The header CRC16 is recomputed over the reader's
/// coverage — for FILE_HEAD with a nested comment the coverage stops
/// before the comment, otherwise it covers the whole header body.
pub(super) fn rename_file_header(header: &[u8], new_name: &str) -> RarResult<Vec<u8>> {
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

    // With a nested comment the covered region runs up to the comment start
    // (fixed fields + name + salt + extended time); without one the whole
    // header body is covered.
    let crc_end = if new_flags & FHD_COMMENT != 0 {
        crate::format::rar4::file_header_crc_end(&out)
    } else {
        out.len()
    };
    let crc = header_crc16(&out[2..crc_end]);
    out[..2].copy_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// Decode a FILE_HEAD block's member name (honoring `FHD_UNICODE`), used by
/// the name-keyed multi-volume rename.
pub(super) fn file_header_name(header: &[u8]) -> RarResult<String> {
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
    let name_size = u16::from_le_bytes([header[26], header[27]]) as usize;
    let name_end = name_start + name_size;
    if name_end > header.len() {
        return Err(RarError::Format(
            "RAR4: file name extends past header".into(),
        ));
    }
    Ok(crate::format::rar4::decode_file_name(
        &header[name_start..name_end],
        flags,
    ))
}

/// Rebuild a `FILE_HEAD` block, optionally renaming it and/or dropping a
/// legacy nested comment. Used by the non-solid edit path when a member's
/// name or comment changes: the compressed payload is copied as-is, so only
/// the header is rewritten. `strip_comment` truncates a legacy nested
/// `FHD_COMMENT` subblock and clears the flag; the replacement comment is a
/// standalone `COMM_HEAD` block the caller emits after the member data
/// (RAR 3.x/4.x layout).
///
/// Everything except the name and the trailing comment area is preserved
/// byte-identically (high sizes, salt, extended time, flags), matching the
/// stage-A rename.
pub(super) fn rebuild_rar4_header(
    header: &[u8],
    new_name: Option<&str>,
    strip_comment: bool,
) -> RarResult<Vec<u8>> {
    let mut out = match new_name {
        Some(name) => rename_file_header(header, name)?,
        None => header.to_vec(),
    };
    if strip_comment {
        out = strip_rar4_comment(&out);
    }
    Ok(out)
}

/// Remove a `FILE_HEAD`'s nested comment subblock (`FHD_COMMENT`): truncate the
/// block, clear the flag and recompute the header CRC over the shortened header.
fn strip_rar4_comment(header: &[u8]) -> Vec<u8> {
    let mut out = header.to_vec();
    if out.len() < 7 {
        return out;
    }
    let flags = u16::from_le_bytes([out[3], out[4]]);
    if flags & FHD_COMMENT == 0 {
        return out;
    }
    // The comment is the last thing in the header; it starts at or after the
    // end of the name/salt area (the extended-time area sits in between).
    let scan_from = crate::format::rar4::file_header_crc_end(&out).min(out.len());
    if let Some((start, _, _)) = crate::format::rar4::find_comment_block_start(&out[scan_from..]) {
        out.truncate(scan_from + start);
    }
    let new_flags = flags & !FHD_COMMENT;
    let new_len = out.len() as u16;
    out[3..5].copy_from_slice(&new_flags.to_le_bytes());
    out[5..7].copy_from_slice(&new_len.to_le_bytes());
    let crc = header_crc16(&out[2..]);
    out[0..2].copy_from_slice(&crc.to_le_bytes());
    out
}
