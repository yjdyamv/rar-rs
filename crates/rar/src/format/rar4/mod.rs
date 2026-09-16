//! RAR 1.5–4.x container family: block scanning, file-header parsing, and
//! member decode.
//!
//! This is the legacy `Rar!\x1a\x07\x00` family (WinRAR 1.5 through 4.x),
//! distinct from the RAR5 container in [`crate::format::rar5`]. Headers are
//! fixed-width (not vint-encoded) and carry a 16-bit CRC over the header
//! body. Member decoding dispatches on the member's [`LegacyCodec`]
//! (folded from `unp_ver`: 15 → Unpack15, 20/26 → Unpack20, 29/36 →
//! Unpack29) and on the `method` byte.
//!
//! [`LegacyCodec`]: crate::version::LegacyCodec
//!
//! STORE members pass through directly; compressed members dispatch to the
//! implemented Unpack15, Unpack20, Unpack29, and PPMd-compatible paths.

mod envelope;
mod extract;
mod read;
pub(crate) mod write;
use crate::archive::ArchiveEntry;
use crate::error::{RarError, RarResult};
use crate::format::decode_system_ansi;
use crate::format::shared::legacy_time::days_from_civil;
use crate::format::shared::split::{SplitMerge, SplitMergeError};
use crate::model::{DataChunk, FileHeader};
pub(crate) use envelope::{EnvelopePolicy, Rar4Block, read_block};
pub(crate) use read::{
    MemberDecodeOptions, decode_member_bytes, decode_member_bytes_to, member_crc,
};
use std::io::{Read, Seek};

// ── Block types ────────────────────────────────────────────────────────────

pub(crate) const MARK_HEAD: u8 = 0x72;
pub(crate) const MAIN_HEAD: u8 = 0x73;
pub(crate) const FILE_HEAD: u8 = 0x74;
pub(crate) const ENDARC_HEAD: u8 = 0x7b;

/// NEWSUB: sub-block (the RAR 3.x/4.x recovery record `RR` and the archive
/// comment `CMT` are both NEWSUB blocks).
pub(crate) const NEWSUB_HEAD: u8 = 0x7a;

/// COMM_HEAD: a per-member comment block. RAR 3.x/4.x stores it as a
/// standalone block after the member data; RAR 1.5–2.9 nested it inside the
/// `FILE_HEAD` (flagged `FHD_COMMENT`). Distinct from `FILE_HEAD` (0x74) and
/// `NEWSUB_HEAD` (0x7a).
pub(crate) const COMM_HEAD: u8 = 0x75;

// ── Header flags ───────────────────────────────────────────────────────────

pub(crate) const LONG_BLOCK: u16 = 0x8000;

pub(crate) const FHD_PASSWORD: u16 = 0x0004;
pub(crate) const FHD_COMMENT: u16 = 0x0008;
pub(crate) const FHD_SOLID: u16 = 0x0010;
pub(crate) const FHD_LARGE: u16 = 0x0100;
pub(crate) const FHD_UNICODE: u16 = 0x0200;
pub(crate) const FHD_SALT: u16 = 0x0400;
pub(crate) const FHD_EXTTIME: u16 = 0x1000;

/// Persistent decoder state for legacy solid chains.
///
/// RAR 2.x and 1.5 decoders retain their window/predictor state across
/// members; a STORE member does not advance the window but does not break
/// the chain either (the decoder is simply not called).
#[allow(clippy::large_enum_variant)]
pub(crate) enum LegacyDecoder {
    Rar29(crate::codec::legacy::rar29::Rar29Decoder),
    Rar20(Box<crate::codec::legacy::rar20::Rar20Decoder>),
    Rar15(Box<crate::codec::legacy::rar15::Rar15Decoder>),
}

impl LegacyDecoder {
    /// The codec this carrier holds: the variant *is* the codec identity.
    pub(crate) fn codec(&self) -> crate::version::LegacyCodec {
        match self {
            LegacyDecoder::Rar29(_) => crate::version::LegacyCodec::Rar29,
            LegacyDecoder::Rar20(_) => crate::version::LegacyCodec::Rar20,
            LegacyDecoder::Rar15(_) => crate::version::LegacyCodec::Rar15,
        }
    }

    /// A fresh decoder for `codec`, used when a solid chain changes codec
    /// generation (the chain keeps its own instance otherwise).
    pub(crate) fn new_for(codec: crate::version::LegacyCodec) -> Self {
        use crate::version::LegacyCodec;
        match codec {
            LegacyCodec::Rar29 => {
                LegacyDecoder::Rar29(crate::codec::legacy::rar29::Rar29Decoder::new())
            }
            LegacyCodec::Rar20 => LegacyDecoder::Rar20(Box::default()),
            LegacyCodec::Rar15 => LegacyDecoder::Rar15(Box::default()),
        }
    }
}

/// RAR4 compression method value for the STORE (uncompressed) method.
pub(crate) const RAR4_METHOD_STORE: u8 = 0x30;

// Normalized model values previously imported from `rar50`; these local
// compatibility constants preserve the existing RAR4 mapping without coupling
// the legacy format implementation to RAR5 wire definitions.
const MODEL_HASH_NONE: u8 = 0;
const MODEL_FILE_FLAG_CRC32: u64 = 0x0004;

/// RAR4 header minimum size for the fixed fields before the variable tail.
const FILE_HEADER_FIXED: usize = 32;

/// File header flag: data continues from the previous volume (SPLIT_BEFORE)
/// or into the next volume (SPLIT_AFTER).
pub(crate) const FHD_SPLIT_BEFORE: u16 = 0x0001;
pub(crate) const FHD_SPLIT_AFTER: u16 = 0x0002;

/// Main header flag: every block after the main header is encrypted (the
/// legacy `-hp` header encryption of RAR 3.x/4.x). Each encrypted block is
/// `[8-byte salt][AES-128-CBC ciphertext of head_size bytes, padded to a
/// 16-byte multiple]`; the 7-byte block prefix lives inside the ciphertext.
pub(crate) const MHD_PASSWORD: u16 = 0x0080;

/// Main header flag: this is a multi-volume archive.
#[allow(dead_code)]
pub(crate) const MHD_VOLUME: u16 = 0x0001;

/// Main header flag: this is the first volume of a multi-volume set.
#[allow(dead_code)]
pub(crate) const MHD_FIRSTVOLUME: u16 = 0x0100;

/// Main header flag: archive is solid (all members share one LZ window).
#[allow(dead_code)]
pub(crate) const MHD_SOLID: u16 = 0x0008;

/// Main header flag: the archive is locked (read-only); any edit is
/// refused until the flag is cleared (which never happens — `rar k` is
/// irreversible).
pub(crate) const MHD_LOCK: u16 = 0x0004;

/// Main header flag: the archive carries an inline recovery record (the
/// NEWSUB `RR` block written before the end-of-archive block). WinRAR's
/// repair looks for this bit before scanning for the record.
pub(crate) const MHD_RECOVERY: u16 = 0x0040;
/// Main header flag: the volume set uses the newer `.partN.rar` numbering
/// (WinRAR's `-vn`); only affects the listing's volume annotations.
pub(crate) const MHD_NEWNUMBERING: u16 = 0x0010;

/// Cross-volume RAR4 block scan. A member split across volumes reappears as
/// continuation file headers (FHD_SPLIT_BEFORE) in later volumes; the scan
/// merges them into one entry with one chunk per volume segment
/// ([`SplitMerge`] owns the ordering guards and the completion fields).
#[derive(Default)]
pub(crate) struct Rar4VolumeScan {
    merge: SplitMerge,
    /// The main header of the first volume carried MHD_SOLID: the archive is
    /// a solid run. Members of pre-RAR3 codecs (anything but the RAR29 codec)
    /// this archive-level flag plus position, NOT by the per-file FHD_SOLID
    /// bit (which those codecs never write); RAR3+ members use FHD_SOLID.
    pub archive_solid: bool,
    /// The main header of the first volume carried MHD_NEWNUMBERING (a
    /// `.partN.rar` legacy set).
    pub new_numbering: bool,
}

/// Map the shared merge error to the RAR4 texts.
fn rar4_split_error(error: SplitMergeError) -> RarError {
    RarError::Format(match error {
        SplitMergeError::ContinuationWithoutStart { fragment } => {
            format!("RAR4: {fragment}: split continuation without a start")
        }
        SplitMergeError::Overlapping { .. } => "RAR4: overlapping split members".into(),
        SplitMergeError::Interrupted { pending } => {
            format!("RAR4: {pending}: split member is interrupted by a regular entry")
        }
        SplitMergeError::MissingFinal { pending } => {
            format!("RAR4: split member {pending} is missing its final volume")
        }
        SplitMergeError::PackedSizeOverflow { pending } => {
            format!("RAR4: {pending}: split packed size overflow")
        }
    })
}

impl Rar4VolumeScan {
    /// Scan one volume. `stream` must be positioned right after the 7-byte
    /// signature (the caller handles the first volume's SFX offset and
    /// signature; later volumes open fresh). Completed entries are pushed to
    /// `out`; an entry whose data continues into the next volume stays
    /// pending here. `password` decrypts `-hp` encrypted headers.
    pub(crate) fn scan_volume(
        &mut self,
        stream: &mut (impl Read + Seek),
        volume_index: usize,
        password: Option<&str>,
        out: &mut Vec<ArchiveEntry>,
    ) -> RarResult<()> {
        // Set when this volume's main header carries MHD_PASSWORD: every
        // later block is header-encrypted. Resets per volume (each volume
        // starts with its own plaintext marker + main header).
        let mut header_encrypted = false;
        let mut password_bytes: Option<&[u8]> = None;
        while let Some(block) = read_block(
            stream,
            header_encrypted,
            password_bytes,
            EnvelopePolicy::SCAN,
        )? {
            match block.head_type {
                MARK_HEAD | MAIN_HEAD => {
                    if block.head_type == MAIN_HEAD && block.flags & MHD_PASSWORD != 0 {
                        header_encrypted = true;
                        let Some(password) = password else {
                            return Err(RarError::Encrypted(
                                "RAR4: header-encrypted archive, a password is required to list it"
                                    .into(),
                            ));
                        };
                        password_bytes = Some(password.as_bytes());
                    }
                    if block.head_type == MAIN_HEAD && block.flags & MHD_SOLID != 0 {
                        self.archive_solid = true;
                    }
                    if block.head_type == MAIN_HEAD && block.flags & MHD_NEWNUMBERING != 0 {
                        self.new_numbering = true;
                    }
                }
                FILE_HEAD => {
                    let split_before = block.flags & FHD_SPLIT_BEFORE != 0;
                    let split_after = block.flags & FHD_SPLIT_AFTER != 0;
                    let fh = parse_file_header(&block)?;
                    let chunk = DataChunk {
                        volume_index,
                        data_offset: fh.data_offset,
                        packed_size: fh.packed_size,
                        crc32_val: fh.crc32_val,
                        is_final: !split_after,
                        extra_data: Vec::new(),
                    };
                    let entry = ArchiveEntry {
                        header: fh,
                        chunks: vec![chunk],
                    };
                    if let Some(done) = self
                        .merge
                        .push(entry, split_before, split_after)
                        .map_err(rar4_split_error)?
                    {
                        out.push(done);
                    }
                }
                ENDARC_HEAD => break,
                COMM_HEAD => {
                    // Standalone per-member comment block (RAR 3.x/4.x
                    // layout): it follows the member's data. Attach it to
                    // the entry that was just completed, or to the pending
                    // split member when a volume ends mid-member.
                    if let (Some(comment), _) = parse_file_comment(&block.header, 0) {
                        let entry = self.merge.pending_mut().or_else(|| out.last_mut());
                        if let Some(entry) = entry
                            && entry.header.comment.is_none()
                        {
                            entry.header.comment = Some(comment);
                        }
                    }
                }
                _ => {
                    // Unknown block types (comment, protect, auth, subblock):
                    // skip over their data area when present.
                }
            }
        }
        Ok(())
    }

    /// Finish: any member still pending is truncated (its last volume is
    /// missing).
    pub(crate) fn finish(self) -> RarResult<()> {
        self.merge.finish().map_err(rar4_split_error)
    }
}

/// Deterministic byte length of a `FHD_EXTTIME` area (not counting the
/// caller's fixed/name/salt prefix). The area opens with a 16-bit flags
/// word whose four nibbles — mtime, ctime, atime, arctime from the top —
/// each carry a PRESENT bit (`0x8`) and a 0..=3 sub-second byte count in
/// the low bits. This mirrors unrar's `ReadExtTime`: mtime reuses the
/// header's own DOS field, ctime and atime carry a 4-byte DOS time before
/// their bytes, and arctime is never materialized (`tbl[3] == NULL`).
/// Returns `None` when the flags word itself does not fit.
fn ext_time_area_len(ext: &[u8]) -> Option<usize> {
    let flags = ext
        .get(..2)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_le_bytes)?;
    let mut len = 2;
    for (index, shift) in [12u32, 8, 4, 0].into_iter().enumerate() {
        let nibble = ((flags >> shift) & 0xf) as usize;
        if nibble & 0x8 == 0 {
            continue;
        }
        len += match index {
            // mtime reuses the fixed DOS field; arctime is never stored.
            0 | 3 => nibble & 0x3,
            // ctime/atime: 4-byte DOS time, then the sub-second bytes.
            _ => 4 + (nibble & 0x3),
        };
    }
    Some(len)
}

/// End offset of the `FHD_EXTTIME` area starting at `ext_start`, capped at
/// `head_end`. Without a parseable flags word the area is empty.
fn ext_time_area_end(header: &[u8], ext_start: usize, head_end: usize) -> usize {
    match ext_time_area_len(&header[ext_start..head_end]) {
        Some(len) => (ext_start + len).min(head_end),
        None => ext_start,
    }
}

/// Parse the nested `COMM_HEAD` (0x75) comment envelope at `start`
/// (header-relative), returning `(block_start, payload_start, payload_end)`.
/// `None` when the bytes there are not a comment envelope that fits inside
/// `header`.
fn comment_block_at(header: &[u8], start: usize) -> Option<(usize, usize, usize)> {
    let tail = header.get(start..)?;
    if tail.len() < 7 || tail[2] != COMM_HEAD {
        return None;
    }
    let flags = u16::from_le_bytes([tail[3], tail[4]]);
    let head_size = u16::from_le_bytes([tail[5], tail[6]]) as usize;
    // CommentHeader body (unp_size, unp_ver, method, comm_crc) is 6 bytes
    // after the 7-byte block prefix; a LONG_BLOCK inserts a 4-byte ADD_SIZE
    // before it (verified against a genuine comment block: HEAD_SIZE 38 with
    // a 10-byte payload).
    let (data_start, data_end) = if flags & LONG_BLOCK != 0 {
        if tail.len() < 11 {
            return None;
        }
        let add = u32::from_le_bytes(tail[7..11].try_into().unwrap()) as usize;
        (start + 17, start + 17 + add)
    } else {
        (start + 13, start + head_size)
    };
    (data_start <= data_end && data_end <= header.len()).then_some((start, data_start, data_end))
}

/// Where the header CRC coverage ends for a FILE_HEAD: the fixed/name/salt
/// prefix plus the extended-time area, stopping at the nested comment
/// subblock when one is present (the comment has its own CRC).
///
/// The extended-time length is derived from its flags word and the comment
/// envelope is parsed at that exact offset — no byte scanning: ext-time
/// tick bytes are arbitrary and can contain the `0x75` type byte, which a
/// scanner would mistake for the comment start and shift the CRC coverage.
pub(crate) fn file_header_crc_end(header: &[u8]) -> usize {
    // Named blocks end at name (+ salt + large high sizes), before the
    // trailing extended-time/comment area.
    let mut end = FILE_HEADER_FIXED;
    if header.len() < FILE_HEADER_FIXED {
        return header.len();
    }
    let flags = u16::from_le_bytes([header[3], header[4]]);
    if flags & FHD_LARGE != 0 {
        end += 8;
    }
    let name_size = u16::from_le_bytes([header[26], header[27]]) as usize;
    end += name_size;
    if flags & FHD_SALT != 0 {
        end += 8;
    }
    if end > header.len() {
        return header.len();
    }
    let ext_end = if flags & FHD_EXTTIME != 0 {
        ext_time_area_end(header, end, header.len())
    } else {
        end
    };
    // With a trailing comment the covered region runs up to where the
    // comment subblock starts: immediately after the fixed/ext area. When
    // no comment envelope is there, fall back to the whole fixed/ext area
    // (tolerant: a missing comment must not reject the header).
    if flags & FHD_COMMENT != 0 && comment_block_at(header, ext_end).is_some() {
        return ext_end;
    }
    ext_end
}

/// Parse a RAR4 FILE_HEAD block body into the format-neutral `FileHeader`,
/// mapping fields to the common model (`format_version: 4`). File data offsets are
/// absolute within the stream.
fn parse_file_header(block: &Rar4Block) -> RarResult<FileHeader> {
    let h = &block.header;
    let start = 0usize;
    let head_end = h.len();
    if head_end < FILE_HEADER_FIXED {
        return Err(RarError::Format("RAR4: file header too short".into()));
    }
    if block.flags & LONG_BLOCK == 0 {
        return Err(RarError::Format(
            "RAR4: file header missing data size".into(),
        ));
    }

    let pack_low = u32::from_le_bytes(h[start + 7..start + 11].try_into().unwrap()) as u64;
    let unp_low = u32::from_le_bytes(h[start + 11..start + 15].try_into().unwrap()) as u64;
    let host_os = h[start + 15];
    let file_crc = u32::from_le_bytes(h[start + 16..start + 20].try_into().unwrap());
    let file_time = u32::from_le_bytes(h[start + 20..start + 24].try_into().unwrap());
    let unp_ver = h[start + 24];
    let method = h[start + 25];
    let name_size = u16::from_le_bytes([h[start + 26], h[start + 27]]) as usize;
    let attr = u32::from_le_bytes(h[start + 28..start + 32].try_into().unwrap());
    let mut pos = start + 32;

    let (pack_size, unp_size) = if block.flags & FHD_LARGE != 0 {
        // The 64-bit high halves extend the fixed area; a crafted header can
        // set FHD_LARGE while head_size stops at the fixed 32 bytes, so check
        // the extent before slicing instead of relying on `name_end` below.
        if pos + 8 > head_end {
            return Err(RarError::Format(
                "RAR4: FHD_LARGE header too short for 64-bit sizes".into(),
            ));
        }
        let high_pack = u32::from_le_bytes(h[pos..pos + 4].try_into().unwrap()) as u64;
        let high_unp = u32::from_le_bytes(h[pos + 4..pos + 8].try_into().unwrap()) as u64;
        pos += 8;
        ((high_pack << 32) | pack_low, (high_unp << 32) | unp_low)
    } else {
        (pack_low, unp_low)
    };

    let name_end = pos
        .checked_add(name_size)
        .ok_or_else(|| RarError::Format("RAR4: name size overflow".into()))?;
    if name_end > head_end {
        return Err(RarError::Format(
            "RAR4: file name extends past header".into(),
        ));
    }
    let name = decode_file_name(&h[pos..name_end], block.flags);
    pos = name_end;

    let salt = if block.flags & FHD_SALT != 0 {
        let salt_end = pos
            .checked_add(8)
            .ok_or_else(|| RarError::Format("RAR4: salt overflow".into()))?;
        if salt_end > head_end {
            return Err(RarError::Format("RAR4: salt extends past header".into()));
        }
        let s: [u8; 8] = h[pos..salt_end].try_into().unwrap();
        pos = salt_end;
        Some(s)
    } else {
        None
    };

    // The extended-time area's deterministic extent marks where the nested
    // comment must start; no scanning (ext-time tick bytes can contain the
    // COMM_HEAD type byte).
    let ext_end = if block.flags & FHD_EXTTIME != 0 {
        ext_time_area_end(h, pos, head_end)
    } else {
        pos
    };

    // File comment: a nested COMM_HEAD (0x75) subblock set by the `FHD_COMMENT`
    // flag, sitting immediately after the extended-time area. Its own 16-bit
    // CRC covers the comment data and the outer file-header CRC stops before
    // it (see `header_crc_end`). The payload is decoded as UTF-8; an
    // even-length non-UTF-8 payload is read as UTF-16LE. `pos` is left
    // unchanged: the extended-time region is read from `pos` below.
    let comment = if block.flags & FHD_COMMENT != 0 {
        parse_file_comment(h, ext_end).0
    } else {
        None
    };

    // Extended time: four nibbles (mtime, ctime, atime, arctime) with
    // sub-second precision. Only mtime is decoded; ctime/atime are stored
    // in `extra_data` for potential future use.
    let ext_time = if block.flags & FHD_EXTTIME != 0 {
        h[pos..head_end].to_vec()
    } else {
        Vec::new()
    };

    let (add_second, mtime_ns) = extract_mtime_refinement(&ext_time);
    // The DOS field has 2-second resolution; an odd second is flagged in the
    // ext-time word.
    let mtime = dos_time_to_unix(file_time).wrapping_add(u32::from(add_second));

    let is_directory = match host_os {
        // MS-DOS / Windows: FILE_ATTRIBUTE_DIRECTORY in the low attribute word.
        0 | 2 => attr & 0x10 != 0,
        // Unix: high attribute word holds the mode bits; S_IFDIR = 0o040000.
        3 => (attr >> 16) & 0o170000 == 0o040000,
        _ => false,
    };

    // RAR4 host OS: 0 = MS-DOS, 1 = OS/2, 2 = Windows, 3 = Unix, 4 = Mac.
    // The model keeps the raw byte so display can distinguish DOS/OS/2 from
    // Windows; `ArchiveEntry::host_os` normalizes it onto the shared axis.
    let host_os_u64 = u64::from(host_os);

    // Data offset = where this block's data area starts on disk: past the
    // header's real on-disk size (head_size for plaintext blocks, or
    // 8 + align16(head_size) when the block was stored header-encrypted).
    let data_offset = block.header_end;

    let fh = FileHeader {
        name,
        unpacked_size: unp_size,
        packed_size: pack_size,
        attributes: attr as u64,
        mtime,
        crc32_val: Some(file_crc),
        hash_type: MODEL_HASH_NONE,
        hash_value: None,
        // RAR4 methods are 0x30..=0x35 on disk; the shared model uses 0..=5.
        comp_method: method.wrapping_sub(RAR4_METHOD_STORE),
        comp_version: 0,
        comp_solid: block.flags & FHD_SOLID != 0,
        // Window-bits field (bits 5–7 of the flags word): `log2(window/64 KiB)`,
        // with 7 marking a directory block.
        comp_dict_size: ((block.flags >> 5) & 7) as u8,
        host_os: host_os_u64,
        flags: block.flags as u64,
        file_flags: MODEL_FILE_FLAG_CRC32,
        extra_data: ext_time,
        is_directory,
        data_offset,
        format_version: 4,
        dict_size_bytes: None,
        mtime_ns,
        ctime: None,
        atime: None,
        owner: None,
        group: None,
        version: None,
        unp_ver,
        salt,
        legacy_head_crc: Some(block.head_crc),
        comment,
    };
    Ok(fh)
}

/// Decode a RAR4 member name, honoring the `FHD_UNICODE` extension when the
/// name carries the legacy-encoded UTF-16 payload.
pub(crate) fn decode_file_name(raw: &[u8], flags: u16) -> String {
    if flags & FHD_UNICODE == 0 {
        let end = raw
            .iter()
            .rposition(|b| *b != 0)
            .map(|i| i + 1)
            .unwrap_or(0);
        let bytes = &raw[..end];
        // Valid UTF-8 keeps the historical byte-identical pass-through;
        // otherwise Windows decodes with the system ANSI code page (e.g.
        // CP936) before falling back to the lossy replacement text.
        return match std::str::from_utf8(bytes) {
            Ok(text) => text.to_string(),
            Err(_) => decode_system_ansi(bytes)
                .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
        };
    }

    let Some(zero_pos) = raw.iter().position(|b| *b == 0) else {
        return String::from_utf8_lossy(raw).into_owned();
    };
    if zero_pos + 1 >= raw.len() {
        return String::from_utf8_lossy(&raw[..zero_pos]).into_owned();
    }

    let fallback = &raw[..zero_pos];
    let high_byte = raw[zero_pos + 1];
    let encoded = &raw[zero_pos + 2..];
    let mut pos = 0usize;
    let mut flag_byte = 0u8;
    let mut flag_bits = 0u8;
    let mut dst_pos = 0usize;
    let mut units = Vec::new();

    while pos < encoded.len() {
        if flag_bits == 0 {
            flag_byte = encoded[pos];
            pos += 1;
            flag_bits = 8;
        }
        let mode = flag_byte >> 6;
        flag_byte <<= 2;
        flag_bits -= 2;

        match mode {
            0 => {
                let Some(&low) = encoded.get(pos) else {
                    return String::from_utf8_lossy(raw).into_owned();
                };
                pos += 1;
                units.push(u16::from(low));
                dst_pos += 1;
            }
            1 => {
                let Some(&low) = encoded.get(pos) else {
                    return String::from_utf8_lossy(raw).into_owned();
                };
                pos += 1;
                units.push((u16::from(high_byte) << 8) | u16::from(low));
                dst_pos += 1;
            }
            2 => {
                let Some((&low, &high)) = encoded.get(pos).zip(encoded.get(pos + 1)) else {
                    return String::from_utf8_lossy(raw).into_owned();
                };
                pos += 2;
                units.push((u16::from(high) << 8) | u16::from(low));
                dst_pos += 1;
            }
            3 => {
                let Some(&length_byte) = encoded.get(pos) else {
                    return String::from_utf8_lossy(raw).into_owned();
                };
                pos += 1;
                let (count, correction, high) = if length_byte & 0x80 != 0 {
                    let Some(&correction) = encoded.get(pos) else {
                        return String::from_utf8_lossy(raw).into_owned();
                    };
                    pos += 1;
                    ((length_byte & 0x7f) as usize + 2, correction, high_byte)
                } else {
                    (length_byte as usize + 2, 0, 0)
                };
                for _ in 0..count {
                    let low = fallback
                        .get(dst_pos)
                        .copied()
                        .unwrap_or(b'?')
                        .wrapping_add(correction);
                    units.push((u16::from(high) << 8) | u16::from(low));
                    dst_pos += 1;
                }
            }
            _ => unreachable!("2-bit filename mode"),
        }
    }

    char::decode_utf16(units)
        .map(|u| u.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Convert a RAR4 MS-DOS date/time (10/6/6 packed fields) to a Unix
/// timestamp (seconds). Best effort: DOS times predate the Unix epoch only
/// for pre-1980, so the result is a near-epoch non-negative value there.
pub(crate) fn dos_time_to_unix(dos: u32) -> u32 {
    let year = ((dos >> 25) & 0x7f) as i64 + 1980;
    let month = (dos >> 21) & 0x0f;
    let day = (dos >> 16) & 0x1f;
    let hour = (dos >> 11) & 0x1f;
    let minute = (dos >> 5) & 0x3f;
    let second = (dos & 0x1f) * 2;

    let days_since_epoch = days_from_civil(year, month, day);
    let secs = days_since_epoch * 86400
        + (i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second));
    secs.clamp(0, u32::MAX as i64) as u32
}

/// Whether a RAR4 member's payload uses the STORE method.
pub(crate) fn is_stored(comp_method: u8) -> bool {
    comp_method == 0
}

/// Extract the mtime refinement from the RAR4 extended-time field: the
/// `ADD_SECOND` flag (the DOS field has 2-second resolution) and the
/// sub-second ticks. The flags word holds four nibbles (mtime first at bits
/// 15-12), each encoding PRESENT (0x8), ADD_SECOND (0x4) and a 0-3 byte
/// count. Sub-second bytes arrive high-end first into a 24-bit accumulator.
fn extract_mtime_refinement(ext_time: &[u8]) -> (bool, Option<u32>) {
    const PRESENT: u8 = 0x8;
    const ADD_SECOND: u8 = 0x4;
    const TICK_NANOSECONDS: u32 = 100;

    let Some(flags) = ext_time
        .get(..2)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_le_bytes)
    else {
        return (false, None);
    };
    let rmode = ((flags >> 12) & 0xf) as u8;
    if rmode & PRESENT == 0 {
        return (false, None);
    }
    let add_second = rmode & ADD_SECOND != 0;
    let Some(bytes) = ext_time.get(2..2 + usize::from(rmode & 0x3)) else {
        return (add_second, None);
    };
    let mut ticks = 0u32;
    for &byte in bytes {
        ticks = (u32::from(byte) << 16) | (ticks >> 8);
    }
    (add_second, Some(ticks * TICK_NANOSECONDS))
}

/// Locate the nested `COMM_HEAD` (0x75) file-comment subblock in a `FILE_HEAD`'s
/// trailing area, byte by byte. `tail` must start at the first byte that can
/// begin a comment block (i.e. the extended-time region). Returns
/// `(block_start, data_start, data_end)` as offsets relative to `tail`, or
/// `None` when no comment block is present.
///
/// The reader no longer uses this: the comment sits immediately after the
/// extended-time area, whose length is derived from its flags word (see
/// [`file_header_crc_end`]). This scan remains for the edit path, where a
/// header is rewritten in place and the comment may sit anywhere.
pub(crate) fn find_comment_block_start(tail: &[u8]) -> Option<(usize, usize, usize)> {
    let mut i = 0;
    while i + 7 <= tail.len() {
        if tail[i + 2] == COMM_HEAD
            && let Some(found) = comment_block_at(tail, i)
        {
            return Some(found);
        }
        i += 1;
    }
    None
}

/// Locate and decode a RAR 3.x/4.x per-file comment (`FHD_COMMENT`). The
/// comment envelope must begin at `start`, the byte just past the
/// extended-time area. Returns the decoded text and the byte length the
/// comment block occupies (`0` when absent).
fn parse_file_comment(header: &[u8], start: usize) -> (Option<Vec<u8>>, usize) {
    match comment_block_at(header, start) {
        Some((i, data_start, data_end)) => {
            // WinRAR stores file comments uncompressed (method 0x30); other
            // methods are rare and best-effort (raw bytes) here.
            let raw = &header[data_start..data_end];
            (Some(decode_comment_text(raw)), data_end - i)
        }
        None => (None, 0),
    }
}

/// Decode a comment payload to raw text bytes. UTF-8 is kept as-is; an
/// even-length non-UTF-8 payload is treated as UTF-16LE (WinRAR's comment
/// encoding for non-ASCII content).
fn decode_comment_text(payload: &[u8]) -> Vec<u8> {
    if std::str::from_utf8(payload).is_ok() || !payload.len().is_multiple_of(2) {
        return payload.to_vec();
    }
    let (units, _) = payload.as_chunks::<2>();
    let units: Vec<u16> = units
        .iter()
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    if units.iter().any(|u| (0xD800..=0xDFFF).contains(u)) {
        return payload.to_vec();
    }
    String::from_utf16_lossy(&units).into_bytes()
}

pub(crate) mod create;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a COMM_HEAD (0x75) file-comment subblock wrapping `payload`.
    /// Layout verified against a genuine RAR2 comment block.
    fn comm_block(payload: &[u8]) -> Vec<u8> {
        let head_size = 13 + payload.len();
        let mut b = vec![0u8; 13];
        b[2] = COMM_HEAD; // head type
        b[5..7].copy_from_slice(&(head_size as u16).to_le_bytes());
        b[7..9].copy_from_slice(&(payload.len() as u16).to_le_bytes()); // unp_size
        b[9] = 29; // unp ver
        b[10] = 0x30; // method (store)
        b.extend_from_slice(payload);
        b
    }

    /// Valid UTF-8 names stay byte-identical through the decoder (the ANSI
    /// or lossy fallbacks must not engage).
    #[test]
    fn decode_file_name_passes_valid_utf8_through() {
        let name = "目录/中文文件.txt";
        assert_eq!(decode_file_name(name.as_bytes(), 0), name);
    }

    #[test]
    fn extract_mtime_refinement_applies_add_second() {
        // PRESENT|ADD_SECOND, one 100-ns tick.
        let ext = [0x00u8, 0xF0, 0x01, 0x00, 0x00];
        let (add_second, ns) = extract_mtime_refinement(&ext);
        assert!(add_second);
        assert_eq!(ns, Some(100));

        // ADD_SECOND without ticks is still a present refinement.
        let (add_second, ns) = extract_mtime_refinement(&[0x00, 0xC0]);
        assert!(add_second);
        assert_eq!(ns, Some(0));

        // No PRESENT bit: nothing to apply.
        assert_eq!(extract_mtime_refinement(&[0x00, 0x00]), (false, None));
    }

    #[test]
    fn parses_ascii_file_comment() {
        let tail = comm_block(b"release notes");
        let (c, len) = parse_file_comment(&tail, 0);
        assert_eq!(c.unwrap(), b"release notes");
        assert_eq!(len, tail.len());
    }

    #[test]
    fn parses_utf16_file_comment() {
        let payload: Vec<u8> = "注".encode_utf16().fold(Vec::new(), |mut v, u| {
            v.extend_from_slice(&u.to_le_bytes());
            v
        });
        let tail = comm_block(&payload);
        let (c, _) = parse_file_comment(&tail, 0);
        assert_eq!(c.unwrap(), "注".as_bytes());
    }

    #[test]
    fn no_comment_returns_none() {
        // A FILE_HEAD (0x74) with no trailing COMM_HEAD subblock.
        let tail = [0u8, 1, FILE_HEAD, 0, 0, 0, 0];
        assert!(parse_file_comment(&tail, 0).0.is_none());
    }

    #[test]
    fn long_block_comment_is_located() {
        // LONG_BLOCK: prefix + 4-byte add_size, then the 6-byte comment body.
        let payload = b"long form";
        let add_size = payload.len() as u32;
        let head_size = 17 + payload.len();
        let mut b = vec![0u8; 11];
        b[2] = COMM_HEAD;
        b[3..5].copy_from_slice(&LONG_BLOCK.to_le_bytes());
        b[5..7].copy_from_slice(&(head_size as u16).to_le_bytes());
        b[7..11].copy_from_slice(&add_size.to_le_bytes());
        b.extend_from_slice(&[0, 0, 29, 0x30, 0, 0]); // unp_size, unp_ver, method, comm_crc
        b.extend_from_slice(payload);
        let (c, len) = parse_file_comment(&b, 0);
        assert_eq!(c.unwrap(), payload);
        assert_eq!(len, b.len());
    }

    #[test]
    fn ext_time_length_follows_the_nibble_word() {
        // mtime PRESENT + 3 tick bytes.
        assert_eq!(ext_time_area_len(&[0x00, 0xF0, 0, 0, 0]), Some(5));
        // mtime PRESENT + 0 tick bytes (ADD_SECOND only).
        assert_eq!(ext_time_area_len(&[0x00, 0xC0]), Some(2));
        // mtime + ctime (4-byte DOS time + 2 tick bytes).
        assert_eq!(ext_time_area_len(&[0x00, 0xBA, 0, 0, 0, 0, 0, 0]), Some(11));
        // No nibbles set: just the flags word.
        assert_eq!(ext_time_area_len(&[0x00, 0x00]), Some(2));
        // An absent flags word has no length.
        assert_eq!(ext_time_area_len(&[0x00]), None);
    }

    /// An ext-time area whose tick bytes contain `0x75` at the offset the old
    /// byte scanner keyed on must not shift the comment boundary: the extent
    /// comes from the flags nibbles, so the header CRC (which covers the
    /// ext-time area) still validates and the real comment is found after it.
    #[test]
    fn ext_time_0x75_byte_does_not_shift_the_comment_boundary() {
        let name = b"t.bin";
        let flags = LONG_BLOCK | FHD_EXTTIME | FHD_COMMENT;
        let mut header = vec![0u8; FILE_HEADER_FIXED];
        header[2] = FILE_HEAD;
        header[3..5].copy_from_slice(&flags.to_le_bytes());
        header[26..28].copy_from_slice(&(name.len() as u16).to_le_bytes());
        header.extend_from_slice(name);
        // Ext-time flags word 0xB800: mtime PRESENT with 3 tick bytes and
        // ctime PRESENT with none -> 2 + 3 + 4 = 9 bytes. The first mtime
        // tick byte is 0x75 and the ctime DOS bytes [13, 0] look like a
        // plausible non-LONG_BLOCK comment at offset 0 to the old scanner.
        let ext = [0x00u8, 0xB8, 0x75, 0x00, 0x00, 0x0D, 0x00, 0x00, 0x00];
        header.extend_from_slice(&ext);
        let ext_end = header.len();
        let comment = comm_block(b"real comment");
        header.extend_from_slice(&comment);
        let head_size = header.len() as u16;
        header[5..7].copy_from_slice(&head_size.to_le_bytes());
        // HEAD_CRC covers fixed + name + ext-time, stopping at the comment.
        let crc = (crate::crc32::crc32(&header[2..ext_end]) & 0xffff) as u16;
        header[..2].copy_from_slice(&crc.to_le_bytes());

        assert_eq!(
            file_header_crc_end(&header),
            ext_end,
            "the ext-time nibbles locate the comment, not the 0x75 byte"
        );
        let block = envelope::read_envelope(0, header, u64::from(head_size), true)
            .expect("valid envelope and header CRC");
        let fh = parse_file_header(&block).expect("parse");
        assert_eq!(fh.comment.as_deref(), Some(&b"real comment"[..]));
    }

    /// The shared envelope reader rejects a bad HEAD_CRC when asked to
    /// verify, and a `head_size`-7 block with LONG_BLOCK (no room for the
    /// 4-byte ADD_SIZE) either way instead of slicing past the buffer.
    #[test]
    fn read_envelope_rejects_bad_crc_and_short_long_block() {
        // A well-formed 13-byte MAIN header with its CRC corrupted.
        let mut main = crate::format::rar4::write::build_main_header(0).to_vec();
        main[0] ^= 0xff;
        let err = envelope::read_envelope(0, main, 13, true).unwrap_err();
        assert!(matches!(err, RarError::Crc { .. }), "got {err}");

        // head_size 7 + LONG_BLOCK: the ADD_SIZE field is out of bounds.
        let mut short = vec![0u8; 7];
        short[0..2].copy_from_slice(&0xFFFFu16.to_le_bytes()); // "no CRC" sentinel
        short[2] = FILE_HEAD;
        short[3..5].copy_from_slice(&LONG_BLOCK.to_le_bytes());
        short[5..7].copy_from_slice(&7u16.to_le_bytes());
        for verify in [true, false] {
            let err = envelope::read_envelope(0, short.clone(), 7, verify).unwrap_err();
            assert!(
                matches!(err, RarError::Format(_)),
                "verify={verify}: expected a format error, got {err}"
            );
        }
    }

    /// A crafted FILE_HEAD can set FHD_LARGE while `head_size` stops at the
    /// fixed 32 bytes; the parser must reject it instead of slicing past the
    /// header for the 64-bit high halves.
    #[test]
    fn fhd_large_header_without_high_sizes_is_rejected() {
        let flags = LONG_BLOCK | FHD_LARGE;
        let mut header = vec![0u8; FILE_HEADER_FIXED];
        header[2] = FILE_HEAD;
        header[3..5].copy_from_slice(&flags.to_le_bytes());
        header[5..7].copy_from_slice(&(FILE_HEADER_FIXED as u16).to_le_bytes());
        let block = Rar4Block {
            head_crc: 0,
            head_type: FILE_HEAD,
            flags,
            offset: 0,
            header_end: FILE_HEADER_FIXED as u64,
            total_size: FILE_HEADER_FIXED as u64,
            add_size: 0,
            header,
            raw_header: None,
        };
        let err = parse_file_header(&block).unwrap_err();
        assert!(
            matches!(err, RarError::Format(_)),
            "expected a format error, got {err}"
        );
    }

    /// A wrong password (or a crafted block) can decrypt to `head_size = 7`
    /// with LONG_BLOCK set: there is no room for the data-size field, so the
    /// helper must reject the block instead of slicing `[7..11]`.
    #[test]
    fn encrypted_header_with_long_block_but_short_head_size_is_rejected() {
        let mut header = vec![0u8; 7];
        header[2] = FILE_HEAD;
        header[3..5].copy_from_slice(&LONG_BLOCK.to_le_bytes());
        header[5..7].copy_from_slice(&7u16.to_le_bytes());
        let (encrypted, _) =
            crate::format::rar4::write::encrypt_block_header(&header, "pw").unwrap();

        let err = read_block(
            &mut std::io::Cursor::new(encrypted),
            true,
            Some(b"pw"),
            EnvelopePolicy::REPAIR,
        )
        .unwrap_err();
        assert!(
            matches!(err, RarError::Format(_)),
            "expected a format error, got {err}"
        );
    }
}
