//! Archive comment (`CMT` NEWSUB block) reading and writing.
//!
//! WinRAR 6.23 places the block right after the first volume's main header;
//! the payload is either STORE bytes or a RAR29-LZSS stream (`method`
//! 0x31-0x35) and carries UTF-16LE (no BOM) when it holds characters
//! outside the single-byte range.

use std::fs;

use crate::archive::RarArchive;
use crate::error::{RarError, RarResult};
use crate::format::rar4::{ENDARC_HEAD, LONG_BLOCK, MAIN_HEAD, MHD_PASSWORD, NEWSUB_HEAD};

use super::header_crc16;
use super::layout::{header_password, main_flags, read_block_view};

/// Encode comment text for storage, mirroring WinRAR 6.23's convention:
/// the CMT block's `attr` bit 0 marks a UTF-16LE payload (no BOM); pure
/// ASCII comments are stored as raw bytes with the bit clear.
/// Returns `(payload, is_utf16)`.
pub(crate) fn encode_comment_text(bytes: &[u8]) -> (Vec<u8>, bool) {
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
pub(super) fn decode_comment_payload(payload: &[u8], unicode: bool) -> Vec<u8> {
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
pub(crate) fn build_comment_block(payload: &[u8], unicode: bool) -> Vec<u8> {
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
/// On a `-hp` archive the block's header is decrypted with the archive
/// password first (only the header is encrypted; the payload is not).
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
    let mut hp: Option<&[u8]> = None;
    while pos + 7 <= bytes.len() {
        let start = pos;
        let view = read_block_view(&bytes, pos, hp)?;
        if view.head_type == MAIN_HEAD && !saw_main {
            saw_main = true;
            if main_flags(&view.header)? & MHD_PASSWORD != 0 {
                let password = header_password(archive).ok_or_else(|| {
                    RarError::Encrypted(
                        "reading the comment of a header-encrypted (-hp) RAR4 archive requires its password".into(),
                    )
                })?;
                hp = Some(password.as_bytes());
            }
        } else if view.head_type == NEWSUB_HEAD
            && view.header.len() >= 32
            && comment_block_name_is_cmt(&view.header)
        {
            let method = view.header[25];
            let unp = u32::from_le_bytes(view.header[11..15].try_into().unwrap());
            let unicode = u32::from_le_bytes(view.header[28..32].try_into().unwrap()) & 1 != 0;
            let data_start = start + view.on_disk_header;
            let data = &bytes[data_start..data_start + view.add_size];
            let payload = if method == crate::format::rar4::RAR4_METHOD_STORE {
                data.to_vec()
            } else {
                decode_comment_stream(&bytes, data_start, view.add_size, method, unp as usize)?
            };
            return Ok(Some(decode_comment_payload(&payload, unicode)));
        }
        pos = start + view.total;
        if view.head_type == ENDARC_HEAD {
            break;
        }
    }
    Ok(None)
}

/// Whether a (plaintext) NEWSUB header names the archive-comment block.
pub(super) fn comment_block_name_is_cmt(header: &[u8]) -> bool {
    let ns = u16::from_le_bytes([header[26], header[27]]) as usize;
    header.get(32..32 + ns) == Some(b"CMT")
}

/// Decode a compressed comment payload through the shared RAR29 member
/// decoder (the payload is a plain single-chunk member stream).
fn decode_comment_stream(
    bytes: &[u8],
    data_start: usize,
    packed_size: usize,
    method: u8,
    unpacked_size: usize,
) -> RarResult<Vec<u8>> {
    use crate::format::rar4::{MemberDecodeOptions, decode_member_bytes};
    use crate::model::{DataChunk, FileHeader};
    if !(0x31..=0x35).contains(&method) {
        return Err(RarError::Format(
            "RAR4: unsupported comment compression method".into(),
        ));
    }
    let data_offset = data_start as u64;
    let packed_size = packed_size as u64;
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
