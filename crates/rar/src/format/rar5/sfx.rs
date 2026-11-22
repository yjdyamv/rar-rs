//! Reproduce WinRAR's `rar s` re-serialization of the embedded archive.
//!
//! `rar a` writes member size fields at WinRAR's *reserved* width, which is
//! wider than the value needs above ~7.5 KiB. Official `rar s` does not merely
//! prepend the SFX module: it re-serializes the embedded archive —
//! - every member header carries the *shortest* `data_size`/`unpacked_size`
//!   vints,
//! - the quick-open catalog is **re-decided** by the automatic rule (only
//!   members whose packed data run exceeds 4096 bytes are cached; the record is
//!   written iff at least one qualifies),
//! - the inline recovery record is dropped (an SFX carries none),
//! - the main-header locator width is re-derived from the archive size and its
//!   quick-open offset recomputed.
//!
//! This module does the same, so our `s` output is byte-identical to WinRAR's.
//! Anything exotic (header encryption, a volume set) is left untouched: the
//! caller then prepends the module to the archive unchanged, still a valid SFX.

use crate::error::{RarError, RarResult};
use crate::vint;

use super::headers::locator::{locator_offset_width, patch_locator_fields};
use super::headers::quick_open::encode_entry;
use super::headers::{build_main_header, build_service_block, frame_block};
use super::{
    ARCHIVE_FLAG_RECOVERY, ARCHIVE_FLAG_VOLUME, BLOCK_FLAG_DATA_AREA, BLOCK_FLAG_EXTRA_DATA,
    BLOCK_FLAG_SKIP_IF_UNKNOWN, BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_TYPE_ENCRYPT_HEADER,
    BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_FILE_HEADER, BLOCK_TYPE_SERVICE_HEADER, RAR5_SIGNATURE,
};

/// A member earns a quick-open entry only when its packed data run exceeds this
/// (the automatic rule; matches `archive::create::QUICK_OPEN_AUTO_THRESHOLD`).
const QUICK_OPEN_AUTO_THRESHOLD: usize = 4096;

/// Rewrite `payload` (a standalone RAR5 archive, no SFX stub) the way official
/// `rar s` does before embedding it. A non-RAR5 or unsupported archive is
/// returned unchanged, so the caller can always prepend the module.
pub fn reserialize_for_sfx(payload: &[u8]) -> Vec<u8> {
    try_reserialize(payload).unwrap_or_else(|_| payload.to_vec())
}

struct Block<'a> {
    block_type: u64,
    /// Framed header bytes: `[CRC32][size vint][body]`.
    framed: &'a [u8],
    body: &'a [u8],
    /// Data area (file data / service payload), empty for header-only blocks.
    data: &'a [u8],
}

fn walk(payload: &[u8]) -> RarResult<Vec<Block<'_>>> {
    let mut off = RAR5_SIGNATURE.len();
    let mut blocks = Vec::new();
    while off + 5 <= payload.len() {
        let block_start = off;
        let stored_crc = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        let (hsize, vint_len) = vint::decode_from_slice(payload, off + 4)
            .map_err(|e| RarError::Format(e.to_string()))?;
        let body_start = off + 4 + vint_len;
        let body_end = body_start
            .checked_add(usize::try_from(hsize).map_err(|_| RarError::format("block size"))?)
            .ok_or_else(|| RarError::format("block size overflow"))?;
        if body_end > payload.len() {
            return Err(RarError::format("truncated block body"));
        }
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&payload[off + 4..body_end]);
        if hasher.finalize() != stored_crc {
            return Err(RarError::format("block header CRC mismatch"));
        }
        let body = &payload[body_start..body_end];
        let (block_type, n) =
            vint::decode_from_slice(body, 0).map_err(|e| RarError::Format(e.to_string()))?;
        let (flags, m) =
            vint::decode_from_slice(body, n).map_err(|e| RarError::Format(e.to_string()))?;
        let mut p = n + m;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (_e, en) =
                vint::decode_from_slice(body, p).map_err(|e| RarError::Format(e.to_string()))?;
            p += en;
        }
        let data_size = if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (d, _dn) =
                vint::decode_from_slice(body, p).map_err(|e| RarError::Format(e.to_string()))?;
            usize::try_from(d).map_err(|_| RarError::format("data size"))?
        } else {
            0
        };
        let data_end = body_end
            .checked_add(data_size)
            .ok_or_else(|| RarError::format("data size overflow"))?;
        if data_end > payload.len() {
            return Err(RarError::format("truncated block data"));
        }
        blocks.push(Block {
            block_type,
            framed: &payload[block_start..body_end],
            body,
            data: &payload[body_end..data_end],
        });
        off = data_end;
        if block_type == BLOCK_TYPE_END_ARCHIVE {
            break;
        }
    }
    Ok(blocks)
}

/// Main-header layout: `[type][flags][extra_size?][archive_flags][extra...]`.
fn parse_main_body(body: &[u8]) -> RarResult<(u64, &[u8])> {
    let (_t, tn) = vint::decode_from_slice(body, 0).map_err(|e| RarError::Format(e.to_string()))?;
    let (block_flags, fn_) =
        vint::decode_from_slice(body, tn).map_err(|e| RarError::Format(e.to_string()))?;
    let mut p = tn + fn_;
    let mut extra_size = 0usize;
    if block_flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        let (v, n) =
            vint::decode_from_slice(body, p).map_err(|e| RarError::Format(e.to_string()))?;
        extra_size = usize::try_from(v).map_err(|_| RarError::format("main extra size"))?;
        p += n;
    }
    let (archive_flags, n) =
        vint::decode_from_slice(body, p).map_err(|e| RarError::Format(e.to_string()))?;
    p += n;
    let extra_end = p
        .checked_add(extra_size)
        .ok_or_else(|| RarError::format("main extra overflow"))?;
    if extra_end > body.len() {
        return Err(RarError::format("truncated main extra"));
    }
    Ok((archive_flags, &body[p..extra_end]))
}

/// The main header's non-locator extra records, verbatim (the locator is
/// rebuilt).
fn main_non_locator_extra(extra: &[u8]) -> RarResult<Vec<u8>> {
    const LOCATOR_TYPE: u64 = 0x01;
    let mut rest = Vec::new();
    let mut off = 0usize;
    while off < extra.len() {
        let (rec_size, n) =
            vint::decode_from_slice(extra, off).map_err(|e| RarError::Format(e.to_string()))?;
        let rec_start = off + n;
        let (rec_type, tn) = vint::decode_from_slice(extra, rec_start)
            .map_err(|e| RarError::Format(e.to_string()))?;
        let rec_end = rec_start
            .checked_add(usize::try_from(rec_size).map_err(|_| RarError::format("rec size"))?)
            .ok_or_else(|| RarError::format("rec overflow"))?;
        if rec_type != LOCATOR_TYPE {
            if rec_end > extra.len() || rec_end <= rec_start {
                return Err(RarError::format("malformed main extra"));
            }
            rest.extend_from_slice(&extra[off..rec_end]);
        }
        off = rec_end.max(rec_start + tn);
    }
    Ok(rest)
}

/// Byte ranges of a FILE body's `data_size` and `unpacked_size` vints.
struct SizeFields {
    data_size: Option<(usize, u64)>,
    file_flags: (usize, usize),
    /// `(start, value, original vint length)`.
    unpacked: (usize, u64, usize),
}

fn parse_size_fields(body: &[u8]) -> RarResult<SizeFields> {
    let (_t, tn) = vint::decode_from_slice(body, 0).map_err(|e| RarError::Format(e.to_string()))?;
    let (flags, fn_) =
        vint::decode_from_slice(body, tn).map_err(|e| RarError::Format(e.to_string()))?;
    let mut p = tn + fn_;
    if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        let (_e, en) =
            vint::decode_from_slice(body, p).map_err(|e| RarError::Format(e.to_string()))?;
        p += en;
    }
    let data_size = if flags & BLOCK_FLAG_DATA_AREA != 0 {
        let (d, dn) =
            vint::decode_from_slice(body, p).map_err(|e| RarError::Format(e.to_string()))?;
        let at = (p, d);
        p += dn;
        Some(at)
    } else {
        None
    };
    let ff_start = p;
    let (_ff, ffn) =
        vint::decode_from_slice(body, p).map_err(|e| RarError::Format(e.to_string()))?;
    let ff_end = ff_start + ffn;
    let (unpacked, usn) =
        vint::decode_from_slice(body, ff_end).map_err(|e| RarError::Format(e.to_string()))?;
    Ok(SizeFields {
        data_size,
        file_flags: (ff_start, ff_end),
        unpacked: (ff_end, unpacked, usn),
    })
}

/// The packed data size of a FILE header (its `data_size` field).
fn file_data_size(body: &[u8]) -> RarResult<usize> {
    let sf = parse_size_fields(body)?;
    Ok(sf.data_size.map_or(0, |(_, v)| v as usize))
}

/// Re-emit one framed FILE header with the two size fields at their natural
/// (shortest) width, recomputing the header size vint and CRC via [`frame_block`].
fn minimize_file_header(framed: &[u8]) -> RarResult<Vec<u8>> {
    let (_hsize, vint_len) =
        vint::decode_from_slice(framed, 4).map_err(|e| RarError::Format(e.to_string()))?;
    let body = &framed[4 + vint_len..];
    let sf = parse_size_fields(body)?;
    let prefix_end = sf.data_size.map_or(sf.file_flags.0, |(at, _)| at);
    let usize_end = sf.unpacked.0 + sf.unpacked.2;
    let mut new_body = Vec::with_capacity(body.len());
    new_body.extend_from_slice(&body[..prefix_end]);
    if let Some((_, value)) = sf.data_size {
        new_body.extend(vint::encode(value));
    }
    new_body.extend_from_slice(&body[sf.file_flags.0..sf.file_flags.1]);
    new_body.extend(vint::encode(sf.unpacked.1));
    new_body.extend_from_slice(&body[usize_end..]);
    Ok(frame_block(&new_body))
}

fn service_name(body: &[u8]) -> &[u8] {
    // [type][flags][extra_size?][data_size][file_flags][unpacked][attrs]
    // [comp][host][name_len][name]...
    let mut p = 0usize;
    let (_type, tn) = match vint::decode_from_slice(body, p) {
        Ok(v) => v,
        Err(_) => return &[],
    };
    p += tn;
    let (flags, fn_) = match vint::decode_from_slice(body, p) {
        Ok(v) => v,
        Err(_) => return &[],
    };
    p += fn_;
    if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        match vint::decode_from_slice(body, p) {
            Ok((_, n)) => p += n,
            Err(_) => return &[],
        }
    }
    // data_size, file_flags, unpacked_size, attributes, comp_info, host_os.
    for _ in 0..6 {
        match vint::decode_from_slice(body, p) {
            Ok((_, n)) => p += n,
            Err(_) => return &[],
        }
    }
    let (name_len, n) = match vint::decode_from_slice(body, p) {
        Ok(v) => v,
        Err(_) => return &[],
    };
    p += n;
    let end = p.saturating_add(name_len as usize);
    body.get(p..end).unwrap_or(&[])
}

fn try_reserialize(payload: &[u8]) -> RarResult<Vec<u8>> {
    if !payload.starts_with(RAR5_SIGNATURE) {
        return Ok(payload.to_vec());
    }
    let blocks = walk(payload)?;
    // Unsupported shapes: leave the archive as-is.
    if blocks
        .iter()
        .any(|b| b.block_type == BLOCK_TYPE_ENCRYPT_HEADER)
    {
        return Ok(payload.to_vec());
    }
    let Some(main) = blocks
        .iter()
        .find(|b| b.block_type == BLOCK_TYPE_ARCHIVE_HEADER)
    else {
        return Ok(payload.to_vec());
    };
    if !blocks
        .iter()
        .any(|b| b.block_type == BLOCK_TYPE_END_ARCHIVE)
    {
        return Ok(payload.to_vec());
    }
    let (archive_flags, extra) = parse_main_body(main.body)?;
    if archive_flags & ARCHIVE_FLAG_VOLUME != 0 {
        return Ok(payload.to_vec());
    }
    let rest = main_non_locator_extra(extra)?;
    let clean_flags = archive_flags & !ARCHIVE_FLAG_RECOVERY;
    let offset_width = locator_offset_width(payload.len() as u64);
    // The locator is always written (official `s` re-adds it even for a `-qo-`
    // source); its quick-open offset is patched once the record position is known.
    let (main_hdr, qo_field, rr_field) =
        build_main_header(clean_flags, &rest, true, false, None, offset_width);

    let mut out = Vec::with_capacity(payload.len());
    out.extend_from_slice(RAR5_SIGNATURE);
    out.extend_from_slice(&main_hdr);
    // (new position, packed data size, minimized header) per emitted member.
    let mut members: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    let mut qo_new_pos: Option<usize> = None;

    // Emit everything in order, dropping the source QO/RR records; the QO
    // catalog is regenerated just before the end block.
    let emit_qo =
        |out: &mut Vec<u8>, members: &[(usize, usize, Vec<u8>)], pos: &mut Option<usize>| {
            let start = out.len();
            let mut qo_payload = Vec::new();
            for (member_pos, data_size, header) in members {
                if *data_size > QUICK_OPEN_AUTO_THRESHOLD {
                    let rel = start - member_pos;
                    qo_payload.extend(encode_entry(rel as u64, header));
                }
            }
            if !qo_payload.is_empty() {
                *pos = Some(start);
                let hdr = build_service_block(
                    "QO",
                    &[],
                    qo_payload.len() as u64,
                    BLOCK_FLAG_SKIP_IF_UNKNOWN,
                );
                out.extend_from_slice(&hdr);
                out.extend_from_slice(&qo_payload);
            }
        };

    let mut ended = false;
    for block in &blocks {
        match block.block_type {
            BLOCK_TYPE_ARCHIVE_HEADER => {}
            BLOCK_TYPE_FILE_HEADER => {
                let new_hdr = minimize_file_header(block.framed)?;
                let pos = out.len();
                members.push((pos, file_data_size(block.body)?, new_hdr.clone()));
                out.extend_from_slice(&new_hdr);
                out.extend_from_slice(block.data);
            }
            BLOCK_TYPE_SERVICE_HEADER => {
                let name = service_name(block.body);
                if name == b"RR" || name == b"QO" {
                    continue;
                }
                out.extend_from_slice(block.framed);
                out.extend_from_slice(block.data);
            }
            BLOCK_TYPE_END_ARCHIVE => {
                emit_qo(&mut out, &members, &mut qo_new_pos);
                out.extend_from_slice(block.framed);
                ended = true;
            }
            _ => {
                out.extend_from_slice(block.framed);
                out.extend_from_slice(block.data);
            }
        }
    }
    if !ended {
        return Ok(payload.to_vec());
    }

    let qo_offset = qo_new_pos.map(|p| p as u64);
    let mut main_slice = out[8..8 + main_hdr.len()].to_vec();
    if patch_locator_fields(&mut main_slice, qo_offset, None, qo_field, rr_field, 8)? {
        out[8..8 + main_hdr.len()].copy_from_slice(&main_slice);
    }
    Ok(out)
}
