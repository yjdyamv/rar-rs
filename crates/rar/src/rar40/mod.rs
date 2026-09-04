//! RAR 1.5–4.x container family: block scanning, file-header parsing, and
//! member decode.
//!
//! This is the legacy `Rar!\x1a\x07\x00` family (WinRAR 1.5 through 4.x),
//! distinct from the RAR5 container in [`crate::rar50`]. Headers are
//! fixed-width (not vint-encoded) and carry a 16-bit CRC over the header
//! body. Member decoding dispatches on the header's `unp_ver` (15 → Unpack15,
//! 20/26 → Unpack20, >= 29 → Unpack29) and on the `method` byte.
//!
//! Only the STORE method (0x30) is decoded today; compressed members return
//! [`RarError::Unsupported`] until the Unpack15/20/29 engines land.

mod read;
pub(crate) mod write;
use crate::archive::ArchiveEntry;
use crate::crc32;
use crate::error::{RarError, RarResult};
use crate::rar50::headers::{DataChunk, FileHeader};
use crate::rar50::*;
pub(crate) use read::{decode_member_bytes, member_crc};
use std::io::{Read, Seek, SeekFrom};

// ── Block types ────────────────────────────────────────────────────────────

pub(crate) const MARK_HEAD: u8 = 0x72;
pub(crate) const MAIN_HEAD: u8 = 0x73;
pub(crate) const FILE_HEAD: u8 = 0x74;
pub(crate) const ENDARC_HEAD: u8 = 0x7b;

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
    Rar29(crate::codec::rar29::Rar29Decoder),
    Rar20(Box<crate::codec::rar20::Rar20Decoder>),
    Rar15(Box<crate::codec::rar15::Rar15Decoder>),
}

/// RAR4 compression method value for the STORE (uncompressed) method.
pub(crate) const RAR4_METHOD_STORE: u8 = 0x30;

/// RAR4 header minimum size for the fixed fields before the variable tail.
const FILE_HEADER_FIXED: usize = 32;

/// A parsed RAR4 block envelope.
#[derive(Debug, Clone)]
struct Rar4Block {
    head_crc: u16,
    head_type: u8,
    flags: u16,
    /// Absolute offset where the block starts.
    offset: u64,
    /// Offset where the block's data area starts (block start + the header's
    /// on-disk byte count: `head_size`, or `8 + align16(head_size)` for
    /// `-hp` header-encrypted blocks).
    header_end: u64,
    /// Bytes on disk for this whole block (header + optional data area).
    total_size: u64,
    /// Header bytes (for validation / name parsing).
    header: Vec<u8>,
}

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

/// Cross-volume RAR4 block scan. A member split across volumes reappears as
/// continuation file headers (FHD_SPLIT_BEFORE) in later volumes; the scan
/// merges them into one entry with one chunk per volume segment.
#[derive(Default)]
pub(crate) struct Rar4VolumeScan {
    pending: Option<ArchiveEntry>,
    /// The main header of the first volume carried MHD_SOLID: the archive is
    /// a solid run. Members of pre-RAR3 codecs (unp_ver < 29) are chained by
    /// this archive-level flag plus position, NOT by the per-file FHD_SOLID
    /// bit (which those codecs never write); RAR3+ members use FHD_SOLID.
    pub archive_solid: bool,
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
        while let Some(block) = read_block(stream, password_bytes, header_encrypted)? {
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
                }
                FILE_HEAD => {
                    let split_before = block.flags & FHD_SPLIT_BEFORE != 0;
                    let split_after = block.flags & FHD_SPLIT_AFTER != 0;
                    let fh = parse_file_header(&block)?;
                    let chunk = DataChunk {
                        volume_index,
                        data_offset: fh.data_offset,
                        packed_size: fh.packed_size,
                        crc32_val: None,
                        is_final: !split_after,
                        extra_data: Vec::new(),
                    };
                    if split_before {
                        // Continuation of a member whose data started in an
                        // earlier volume. The first header stays canonical
                        // (name, unpacked size, method); later headers only
                        // contribute this volume's segment.
                        let Some(entry) = self.pending.as_mut() else {
                            return Err(RarError::Format(format!(
                                "RAR4: {}: split continuation without a start",
                                fh.name
                            )));
                        };
                        entry.chunks.push(chunk);
                        if !split_after {
                            // Final segment: this header carries the whole-
                            // file CRC; total packed size is the chunk sum.
                            entry.header.packed_size =
                                entry.chunks.iter().map(|c| c.packed_size).sum();
                            entry.header.crc32_val = fh.crc32_val;
                            out.push(self.pending.take().unwrap());
                        }
                    } else if split_after {
                        if self.pending.is_some() {
                            return Err(RarError::Format("RAR4: overlapping split members".into()));
                        }
                        self.pending = Some(ArchiveEntry {
                            header: fh,
                            chunks: vec![chunk],
                        });
                    } else {
                        out.push(ArchiveEntry {
                            header: fh,
                            chunks: vec![chunk],
                        });
                    }
                    if block.total_size > block.header.len() as u64 {
                        stream.seek(SeekFrom::Start(block.offset + block.total_size))?;
                    }
                }
                ENDARC_HEAD => break,
                _ => {
                    // Unknown block types (comment, protect, auth, subblock):
                    // skip over their data area when present.
                    if block.total_size > block.header.len() as u64 {
                        stream.seek(SeekFrom::Start(block.offset + block.total_size))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Finish: any member still pending is truncated (its last volume is
    /// missing).
    pub(crate) fn finish(self) -> RarResult<()> {
        if let Some(entry) = self.pending {
            return Err(RarError::Format(format!(
                "RAR4: split member {} is missing its final volume",
                entry.header.name
            )));
        }
        Ok(())
    }
}

fn read_block(
    stream: &mut (impl Read + Seek),
    password: Option<&[u8]>,
    encrypted: bool,
) -> RarResult<Option<Rar4Block>> {
    let start = stream.stream_position()?;
    if encrypted {
        return read_encrypted_block(stream, start, password);
    }
    let mut base = [0u8; 7];
    let n = read_some(stream, &mut base)?;
    if n == 0 {
        return Ok(None);
    }
    if n < 7 {
        return Err(RarError::Format("RAR4: truncated block header".into()));
    }
    let head_size = u16::from_le_bytes([base[5], base[6]]);
    if head_size < 7 {
        return Err(RarError::Format(format!(
            "RAR4: block head_size {head_size} too small"
        )));
    }

    // Read the full header.
    let mut header = base.to_vec();
    if head_size as usize > 7 {
        let mut rest = vec![0u8; head_size as usize - 7];
        read_exact(stream, &mut rest)?;
        header.extend_from_slice(&rest);
    }
    finish_block(start, header, u64::from(head_size))
}

/// Read an `-hp` encrypted block: `[8-byte salt][AES-128-CBC ciphertext]`,
/// where the ciphertext holds the whole header (7-byte prefix included)
/// padded to a 16-byte multiple. The head size only becomes known after
/// decrypting the first block.
fn read_encrypted_block(
    stream: &mut (impl Read + Seek),
    start: u64,
    password: Option<&[u8]>,
) -> RarResult<Option<Rar4Block>> {
    let mut first = [0u8; 24];
    let n = read_some(stream, &mut first)?;
    if n == 0 {
        return Ok(None);
    }
    if n < 24 {
        return Err(RarError::Format(
            "RAR4: truncated encrypted block header".into(),
        ));
    }
    let Some(password) = password else {
        return Err(RarError::Encrypted(
            "RAR4: header-encrypted archive, a password is required to list it".into(),
        ));
    };
    let salt: [u8; 8] = first[..8].try_into().unwrap();
    let mut cipher = crate::crypto::Rar30Cipher::new(password, Some(salt))
        .map_err(|e| RarError::Format(format!("RAR4 header key setup: {e}")))?;
    let mut block0: [u8; 16] = first[8..24].try_into().unwrap();
    cipher
        .decrypt_in_place(&mut block0)
        .map_err(|e| RarError::Format(format!("RAR4 header decrypt: {e}")))?;
    let head_size = u16::from_le_bytes([block0[5], block0[6]]);
    if head_size < 7 {
        return Err(RarError::Format(format!(
            "RAR4: encrypted header head_size {head_size} too small (wrong password?)"
        )));
    }
    let align16 = ((head_size as usize) + 15) & !15;
    let mut rest = vec![0u8; align16 - 16];
    read_exact(stream, &mut rest)?;
    cipher
        .decrypt_in_place(&mut rest)
        .map_err(|e| RarError::Format(format!("RAR4 header decrypt: {e}")))?;
    let mut header = block0.to_vec();
    header.extend_from_slice(&rest);
    header.truncate(head_size as usize);
    finish_block(start, header, 8 + align16 as u64)
}

/// Validate a (possibly decrypted) header and compute the block envelope.
/// `on_disk_prefix` is the number of bytes the header occupies on disk
/// (head_size for plaintext blocks, `8 + align16(head_size)` when the
/// block was stored header-encrypted).
fn finish_block(start: u64, header: Vec<u8>, on_disk_prefix: u64) -> RarResult<Option<Rar4Block>> {
    if header.len() < 7 {
        return Err(RarError::Format("RAR4: truncated block header".into()));
    }
    let head_crc = u16::from_le_bytes([header[0], header[1]]);
    let head_type = header[2];
    let flags = u16::from_le_bytes([header[3], header[4]]);
    let head_size = u16::from_le_bytes([header[5], header[6]]);
    if head_size < 7 || header.len() < head_size as usize {
        return Err(RarError::Format(format!(
            "RAR4: block head_size {head_size} too small"
        )));
    }

    // Validate header CRC (16-bit) over bytes[2..head_size], except for
    // MARK (which has no meaningful CRC), AV/SIGN (documented bad), and
    // the 0xFFFF sentinel (RAR 1.5.4-era "no CRC" marker).
    let should_check = !matches!(head_type, MARK_HEAD | 0x76 | 0x79) && head_crc != 0xFFFF;
    let crc_end = header_crc_end(&header, head_type, flags);
    if should_check {
        let actual = (crc32::crc32(&header[2..crc_end]) & 0xffff) as u16;
        if actual != head_crc {
            return Err(RarError::Crc {
                expected: head_crc as u32,
                actual: actual as u32,
                context: format!("RAR4 block type {head_type:#x} header"),
            });
        }
    }

    let add_size = if flags & LONG_BLOCK != 0 {
        if header.len() < 11 {
            return Err(RarError::Format(
                "RAR4: header missing LONG_BLOCK size".into(),
            ));
        }
        u32::from_le_bytes(header[7..11].try_into().unwrap()) as u64
    } else {
        0
    };

    let total_size = on_disk_prefix + add_size;
    Ok(Some(Rar4Block {
        head_crc,
        head_type,
        flags,
        offset: start,
        header_end: start + on_disk_prefix,
        total_size,
        header,
    }))
}

/// Where the header CRC coverage ends: some block types with a nested
/// comment stop before the comment (which has its own CRC).
fn header_crc_end(header: &[u8], head_type: u8, flags: u16) -> usize {
    match head_type {
        MAIN_HEAD if flags & 0x0002 != 0 => 13.min(header.len()),
        FILE_HEAD if flags & FHD_COMMENT != 0 => file_header_crc_end(header),
        _ => header.len(),
    }
}

fn file_header_crc_end(header: &[u8]) -> usize {
    // Named blocks end at name (+ salt + large high sizes), before the
    // trailing comment block.
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
    end.min(header.len())
}

/// Parse a RAR4 FILE_HEAD block body into a RAR5-style `FileHeader`, mapping
/// fields to the common model (`format_version: 4`). File data offsets are
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

    // File comment: a nested COMM_HEAD block inside the header. Its own
    // 16-bit CRC covers the comment data; the outer header CRC stops before
    // it (handled by `header_crc_end`). We skip over the comment bytes here;
    // they are not exposed in the common model.
    if block.flags & FHD_COMMENT != 0 && pos + 2 <= head_end {
        pos += 2;
        // Comment data runs to the end of the header (before ext_time).
    }

    // Extended time: four nibbles (mtime, ctime, atime, arctime) with
    // sub-second precision. Only mtime is decoded; ctime/atime are stored
    // in `extra_data` for potential future use.
    let ext_time = if block.flags & FHD_EXTTIME != 0 {
        h[pos..head_end].to_vec()
    } else {
        Vec::new()
    };

    let mtime_ns = extract_mtime_refinement(&ext_time);

    let is_directory = match host_os {
        // MS-DOS / Windows: FILE_ATTRIBUTE_DIRECTORY in the low attribute word.
        0 | 2 => attr & 0x10 != 0,
        // Unix: high attribute word holds the mode bits; S_IFDIR = 0o040000.
        3 => (attr >> 16) & 0o170000 == 0o040000,
        _ => false,
    };

    // RAR4 host OS: 0 = MS-DOS, 1 = OS/2, 2 = Windows, 3 = Unix, 4 = Mac.
    // Map to the shared OS constants (0 = Windows, 1 = Unix).
    let host_os_u64 = match host_os {
        0 | 2 => OS_WINDOWS,
        _ => OS_UNIX,
    };

    // Data offset = where this block's data area starts on disk: past the
    // header's real on-disk size (head_size for plaintext blocks, or
    // 8 + align16(head_size) when the block was stored header-encrypted).
    let data_offset = block.header_end;

    let fh = FileHeader {
        name,
        unpacked_size: unp_size,
        packed_size: pack_size,
        attributes: attr as u64,
        mtime: dos_time_to_unix(file_time),
        crc32_val: Some(file_crc),
        hash_type: HASH_NONE,
        hash_value: None,
        // RAR4 methods are 0x30..=0x35 on disk; the shared model uses 0..=5.
        comp_method: method.wrapping_sub(RAR4_METHOD_STORE),
        comp_version: 0,
        comp_solid: block.flags & FHD_SOLID != 0,
        comp_dict_size: 0,
        host_os: host_os_u64,
        flags: block.flags as u64,
        file_flags: FILE_FLAG_CRC32,
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
        return String::from_utf8_lossy(&raw[..end]).into_owned();
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

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
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

/// Whether a RAR4 member's payload uses the STORE method.
pub(crate) fn is_stored(comp_method: u8) -> bool {
    comp_method == 0
}

/// Extract the mtime sub-second refinement from the RAR4 extended-time
/// field.  The flags word holds four nibbles (mtime first at bits 15-12),
/// each encoding PRESENT (0x8), ADD_SECOND (0x4), and a 0-3 byte count.
/// Sub-second bytes arrive high-end first into a 24-bit accumulator.
fn extract_mtime_refinement(ext_time: &[u8]) -> Option<u32> {
    const PRESENT: u8 = 0x8;
    const TICK_NANOSECONDS: u32 = 100;

    let flags = u16::from_le_bytes(ext_time.get(..2)?.try_into().ok()?);
    let rmode = ((flags >> 12) & 0xf) as u8;
    if rmode & PRESENT == 0 {
        return None;
    }
    let mut ticks = 0u32;
    for &byte in ext_time.get(2..2 + usize::from(rmode & 0x3))? {
        ticks = (u32::from(byte) << 16) | (ticks >> 8);
    }
    Some(ticks * TICK_NANOSECONDS)
}
