//! Archive comment wire format: the RAR 3.0+ `CMT` NEWSUB block *and* the
//! legacy (`unp_ver < 29`) comment that lives inside the main header.
//!
//! WinRAR 6.23 places the NEWSUB block right after the first volume's main
//! header; the payload is either STORE bytes or a RAR29-LZSS stream (`method`
//! 0x31-0x35) and carries UTF-16LE (no BOM) when it holds characters
//! outside the single-byte range. The comment is read from the set's first
//! volume regardless of which part was opened.
//!
//! RAR 1.5/2.x had no NEWSUB block: the main header itself is longer
//! (`head_size` = 13 + comment block) and carries a nested `0x75` block whose
//! `method` is STORE or a RAR15 stream. Same content, different envelope.
//!
//! The *codec* lives here because it is RAR4 block serialization; locating
//! and decrypting the block inside an open archive is an engine operation
//! and stays in `archive::rar4_edit::comment`.

use std::io::{Read, Seek};

use crate::error::{RarError, RarResult};
use crate::format::rar4::LONG_BLOCK;
use crate::format::shared::checksum::header_crc16;

/// Header byte count of the NEWSUB `CMT` archive-comment block (32 fixed
/// bytes + the 3-byte name `CMT`); the payload follows as data.
pub(crate) const CMT_HEAD_SIZE: usize = 35;

/// Header byte count of the legacy comment block (RAR 1.5/2.x): a nested
/// `0x75` header with no name, whose payload follows immediately. It sits
/// *inside* the main header, so the main header's `head_size` covers it.
pub(crate) const LEGACY_COMMENT_HEAD_SIZE: usize = 13;

/// Byte count of the fixed part of a main header, before any comment block.
pub(crate) const MAIN_HEAD_SIZE: usize = 13;

/// A legacy comment block parsed out of a (plaintext) main header.
pub(crate) struct LegacyComment<'a> {
    pub unp_ver: u8,
    pub method: u8,
    pub unpacked_size: usize,
    pub packed: &'a [u8],
}

/// Parse the comment block nested in `main_header` (`MHD_COMMENT` archives).
/// The slice must start at the main header's own first byte (the CRC) and may
/// extend past the header; `None` means "no comment block there".
///
/// Layout: `crc16, head_type 0x75, flags, head_size, unp_size, unp_ver,
/// method, comm_crc16` — 13 bytes, then the payload.
pub(crate) fn parse_legacy_comment(main_header: &[u8]) -> Option<LegacyComment<'_>> {
    let block = main_header.get(MAIN_HEAD_SIZE..)?;
    if block.len() < LEGACY_COMMENT_HEAD_SIZE || block[2] != 0x75 {
        return None;
    }
    let head_size = usize::from(u16::from_le_bytes([block[5], block[6]]));
    if head_size < LEGACY_COMMENT_HEAD_SIZE || block.len() < head_size {
        return None;
    }
    Some(LegacyComment {
        unpacked_size: usize::from(u16::from_le_bytes([block[7], block[8]])),
        unp_ver: block[9],
        method: block[10],
        packed: &block[LEGACY_COMMENT_HEAD_SIZE..head_size],
    })
}

/// Build the legacy comment block for `text`. The caller appends it to the
/// main header and grows that header's `head_size`.
///
/// Both official pre-3.0 writers compress the comment with the container's
/// own codec and declare it as method 4 — WinRAR 1.5 Beta stores an
/// Unpack15 stream, WinRAR 2.00/2.90 an Unpack20 one, even when the packed
/// form ends up *longer* than the text. The shape matters: UnRAR 2.00 reads
/// a stored comment declared `unp_ver 15` as "Comment header broken", so the
/// payload has to follow the declared generation.
pub(crate) fn build_legacy_comment_block(text: &[u8], unp_ver: u8) -> RarResult<Vec<u8>> {
    if text.len() > usize::from(u16::MAX) {
        return Err(RarError::invalid_option(
            "comment too long for a legacy (unp_ver < 29) RAR4 archive",
        ));
    }
    let unp_ver = unp_ver.min(20);
    let packed = if unp_ver < 20 {
        crate::codec::legacy::rar15_encoder::unpack15_encode(text)?
    } else {
        crate::codec::legacy::rar20_encoder::unpack20_encode_auto(text)?
    };
    let head_size = LEGACY_COMMENT_HEAD_SIZE + packed.len();
    let head_size = u16::try_from(head_size).map_err(|_| {
        RarError::invalid_option("comment too long for a legacy (unp_ver < 29) RAR4 archive")
    })?;
    let unp_size = u16::try_from(text.len()).map_err(|_| {
        RarError::invalid_option("comment too long for a legacy (unp_ver < 29) RAR4 archive")
    })?;
    let mut block = Vec::with_capacity(usize::from(head_size));
    block.extend_from_slice(&[0u8, 0]); // header CRC, filled last
    block.push(0x75); // comment header
    block.extend_from_slice(&0u16.to_le_bytes()); // flags: no LONG_BLOCK
    block.extend_from_slice(&head_size.to_le_bytes());
    block.extend_from_slice(&unp_size.to_le_bytes());
    block.push(unp_ver);
    block.push(crate::format::rar4::RAR4_METHOD_STORE + 4);
    block.extend_from_slice(&(crate::crc32::crc32(text) as u16).to_le_bytes());
    block.extend_from_slice(&packed);
    // The CRC covers only the fixed 11-byte block header (measured against
    // WinRAR 2.90: the payload is not part of it, unlike a FILE_HEAD).
    let crc = header_crc16(&block[2..LEGACY_COMMENT_HEAD_SIZE]);
    block[..2].copy_from_slice(&crc.to_le_bytes());
    Ok(block)
}

/// Decode a legacy comment block's payload to the stored comment bytes.
pub(crate) fn decode_legacy_comment(comment: &LegacyComment<'_>) -> RarResult<Vec<u8>> {
    use crate::format::rar4::RAR4_METHOD_STORE;
    if comment.method == RAR4_METHOD_STORE {
        return Ok(comment.packed.to_vec());
    }
    let mut cursor = std::io::Cursor::new(comment.packed);
    decode_comment_stream(
        &mut cursor,
        0,
        comment.packed.len() as u64,
        comment.method,
        comment.unp_ver,
        comment.unpacked_size,
    )
}

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
pub(crate) fn decode_comment_payload(payload: &[u8], unicode: bool) -> Vec<u8> {
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

/// Whether a (plaintext) NEWSUB header names the archive-comment block.
pub(crate) fn comment_block_name_is_cmt(header: &[u8]) -> bool {
    let ns = u16::from_le_bytes([header[26], header[27]]) as usize;
    header.get(32..32 + ns) == Some(b"CMT")
}

/// Decode a compressed comment payload through the shared RAR29 member
/// decoder (the payload is a plain single-chunk member stream). The packed
/// bytes are read straight from `stream` (which is left where the decoder
/// finished), so the surrounding archive is never buffered.
pub(crate) fn decode_comment_stream(
    stream: &mut (impl Read + Seek),
    data_start: u64,
    packed_size: u64,
    method: u8,
    unp_ver: u8,
    unpacked_size: usize,
) -> RarResult<Vec<u8>> {
    use crate::format::rar4::{MemberDecodeOptions, decode_member_bytes};
    use crate::model::{DataChunk, FileHeader};
    if !(0x31..=0x35).contains(&method) {
        return Err(RarError::format(
            "RAR4: unsupported comment compression method",
        ));
    }
    let hdr = FileHeader {
        unpacked_size: unpacked_size as u64,
        packed_size,
        comp_method: method.wrapping_sub(crate::format::rar4::RAR4_METHOD_STORE),
        data_offset: data_start,
        format_version: 4,
        unp_ver,
        ..Default::default()
    };
    let chunk = DataChunk {
        volume_index: 0,
        data_offset: data_start,
        packed_size,
        crc32_val: None,
        is_final: true,
        extra_data: Vec::new(),
    };
    decode_member_bytes(
        stream,
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
