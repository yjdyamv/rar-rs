/// RAR4 header parsing.
///
/// Clean-room implementation based on the header layout documented in
/// libarchive's archive_read_support_format_rar.c (BSD-2-Clause).
use std::io::{self, Read, Seek, SeekFrom};

use crate::error::{RarError, RarResult};
use crate::headers::{DataChunk, FileHeader};

use super::constants::*;

// ── Common header ─────────────────────────────────────────────────────────

/// The 7-byte common header present in every RAR4 block.
#[derive(Debug, Clone)]
pub struct Rar4CommonHeader {
    pub crc16: u16,
    pub header_type: u8,
    pub flags: u16,
    pub header_size: u16,
    /// Data payload size after the header (from ADD_SIZE or 0).
    pub add_size: u32,
}

impl Rar4CommonHeader {
    /// Read a RAR4 common header from the stream.
    pub fn read_from<R: Read + Seek>(stream: &mut R) -> RarResult<Self> {
        let mut buf = [0u8; 7];
        stream.read_exact(&mut buf).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                e.into()
            } else {
                RarError::Io(e)
            }
        })?;

        let crc16 = u16::from_le_bytes([buf[0], buf[1]]);
        let header_type = buf[2];
        let flags = u16::from_le_bytes([buf[3], buf[4]]);
        let header_size = u16::from_le_bytes([buf[5], buf[6]]);

        let add_size = if flags & HD_FLAG_ADD_SIZE != 0 {
            // For FILE_HEAD and NEWSUB_HEAD, the add_size (packed data size)
            // is embedded inside the extended header, not after the 7-byte
            // common header. We'll read it from the extended header instead.
            // For other block types, read 4 bytes after the common header.
            if header_type == RAR4_HEAD_FILE || header_type == RAR4_HEAD_NEWSUB {
                0 // will be set from extended header parsing
            } else {
                let mut add = [0u8; 4];
                stream.read_exact(&mut add)?;
                u32::from_le_bytes(add)
            }
        } else {
            0
        };

        Ok(Rar4CommonHeader {
            crc16,
            header_type,
            flags,
            header_size,
            add_size,
        })
    }

    /// Verify the header CRC16 (lower 16 bits of CRC32 of header bytes
    /// after the CRC field).
    pub fn verify_crc<R: Read + Seek>(&self, stream: &mut R, header_start: u64) -> RarResult<()> {
        let crc_data_len = self.header_size as usize - 2; // exclude CRC16 field
        if crc_data_len == 0 {
            return Ok(());
        }
        let saved_pos = stream.stream_position()?;
        stream.seek(SeekFrom::Start(header_start + 2))?;
        let mut buf = vec![0u8; crc_data_len];
        stream.read_exact(&mut buf)?;
        stream.seek(SeekFrom::Start(saved_pos))?;

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&buf);
        let computed = hasher.finalize() & 0xFFFF;
        if computed as u16 != self.crc16 {
            return Err(RarError::Crc {
                expected: self.crc16 as u32,
                actual: computed,
                context: format!("RAR4 header type {:#04x}", self.header_type),
            });
        }
        Ok(())
    }
}

// ── Main archive header ───────────────────────────────────────────────────

/// Parsed RAR4 main archive header (0x73).
#[derive(Debug, Clone)]
pub struct Rar4MainHeader {
    pub flags: u16,
    pub is_solid: bool,
    pub is_volume: bool,
    pub is_encrypted: bool,
    pub has_new_numbering: bool,
}

impl Rar4MainHeader {
    pub fn parse<R: Read + Seek>(
        common: &Rar4CommonHeader,
        stream: &mut R,
        header_start: u64,
    ) -> RarResult<Self> {
        // Main header has 6 extra bytes after the common 7:
        // 2 bytes: AV_POS (unused)
        // 4 bytes: AV_SIZE (unused)
        // For encrypted archives, there may be additional data.
        // We just need the flags from the common header.

        // Skip to end of header
        let header_end = header_start + common.header_size as u64;
        stream.seek(SeekFrom::Start(header_end))?;

        Ok(Rar4MainHeader {
            flags: common.flags,
            is_solid: common.flags & MHD_SOLID != 0,
            is_volume: common.flags & MHD_VOLUME != 0,
            is_encrypted: common.flags & MHD_PASSWORD != 0,
            has_new_numbering: common.flags & MHD_NEWNUMBERING != 0,
        })
    }
}

// ── File header parsing ───────────────────────────────────────────────────

/// Parse a RAR4 FILE_HEAD (0x74) or NEWSUB_HEAD (0x7A) into a FileHeader.
///
/// `header_start` is the stream position where the 7-byte common header began.
/// The stream should be positioned right after the 7-byte common header.
pub fn parse_rar4_file_header<R: Read + Seek>(
    common: &Rar4CommonHeader,
    stream: &mut R,
    header_start: u64,
) -> RarResult<(FileHeader, DataChunk)> {
    // Read the extended header bytes (everything after the 7-byte common header
    // up to header_size).
    let ext_len = common.header_size as usize - 7;
    if ext_len < 25 {
        return Err(RarError::Format("RAR4 file header too short".into()));
    }
    let mut ext = vec![0u8; ext_len];
    stream.read_exact(&mut ext)?;

    let mut pos = 0;

    // 4 bytes: packed_size (low 32 bits)
    let packed_low = u32::from_le_bytes([ext[pos], ext[pos + 1], ext[pos + 2], ext[pos + 3]]);
    pos += 4;

    // 4 bytes: unpacked_size (low 32 bits)
    let unpacked_low = u32::from_le_bytes([ext[pos], ext[pos + 1], ext[pos + 2], ext[pos + 3]]);
    pos += 4;

    // 1 byte: host_os
    let host_os = ext[pos];
    pos += 1;

    // 4 bytes: file CRC32
    let file_crc = u32::from_le_bytes([ext[pos], ext[pos + 1], ext[pos + 2], ext[pos + 3]]);
    pos += 4;

    // 4 bytes: ftime (DOS timestamp)
    let ftime = u32::from_le_bytes([ext[pos], ext[pos + 1], ext[pos + 2], ext[pos + 3]]);
    pos += 4;

    // 1 byte: unp_ver (version needed to extract)
    let _unp_ver = ext[pos];
    pos += 1;

    // 1 byte: method (0x30-0x35)
    let method = ext[pos];
    pos += 1;

    // 2 bytes: name_size
    let name_size = u16::from_le_bytes([ext[pos], ext[pos + 1]]) as usize;
    pos += 2;

    // 4 bytes: file_attr
    let file_attr = u32::from_le_bytes([ext[pos], ext[pos + 1], ext[pos + 2], ext[pos + 3]]);
    pos += 4;

    // If FHD_LARGE: 8 more bytes for high parts of packed/unpacked sizes
    let mut packed_size: u64 = packed_low as u64;
    let mut unpacked_size: u64 = unpacked_low as u64;

    if common.flags & FHD_LARGE != 0 {
        if pos + 8 > ext.len() {
            return Err(RarError::Format("RAR4 FHD_LARGE: header too short".into()));
        }
        let high_packed = u32::from_le_bytes([ext[pos], ext[pos + 1], ext[pos + 2], ext[pos + 3]]);
        pos += 4;
        let high_unpacked =
            u32::from_le_bytes([ext[pos], ext[pos + 1], ext[pos + 2], ext[pos + 3]]);
        pos += 4;
        packed_size |= (high_packed as u64) << 32;
        unpacked_size |= (high_unpacked as u64) << 32;
    }

    // Filename
    if pos + name_size > ext.len() {
        return Err(RarError::Format(
            "RAR4 file header: filename extends past header".into(),
        ));
    }
    let name_bytes = &ext[pos..pos + name_size];
    pos += name_size;

    let name = if common.flags & FHD_UNICODE != 0 {
        decode_rar4_unicode_name(name_bytes)?
    } else {
        // Plain ASCII/Latin-1
        String::from_utf8_lossy(name_bytes).into_owned()
    };

    // Normalize path separators
    let name = name.replace('\\', "/");

    // Salt (if FHD_SALT flag set) — 8 bytes, skip for now
    if common.flags & FHD_SALT != 0 && pos + 8 <= ext.len() {
        pos += 8;
    }

    // Extended time (if FHD_EXTTIME flag set) — variable, skip
    // We already have ftime from the DOS timestamp
    let _ = pos;

    // Determine if directory
    let is_directory =
        if host_os == RAR4_OS_UNIX {
            file_attr & (RAR4_ATTR_UNIX_DIR << 16) != 0
        } else {
            file_attr & RAR4_ATTR_DIRECTORY != 0
        } || (method == RAR4_METHOD_STORE && unpacked_size == 0 && name.ends_with('/'));

    // Map RAR4 method (0x30-0x35) to normalized method (0-5)
    let comp_method = method.wrapping_sub(RAR4_METHOD_STORE);

    // Data starts right after the header
    let data_offset = header_start + common.header_size as u64;

    // Convert DOS timestamp to Unix
    let mtime = parse_dos_time(ftime);

    let fh = FileHeader {
        name,
        unpacked_size,
        packed_size,
        attributes: file_attr as u64,
        mtime,
        crc32_val: Some(file_crc),
        comp_method,
        comp_version: 0,
        comp_solid: common.flags & FHD_SOLID != 0,
        comp_dict_size: 5, // RAR4 default: 4MB = 128KB * 2^5
        host_os: host_os as u64,
        flags: common.flags as u64,
        file_flags: common.flags as u64,
        extra_data: Vec::new(),
        is_directory,
        data_offset,
        format_version: 4,
    };

    let chunk = DataChunk {
        volume_index: 0,
        data_offset,
        packed_size,
        crc32_val: Some(file_crc),
        is_final: common.flags & FHD_SPLIT_AFTER == 0,
        extra_data: Vec::new(),
    };

    Ok((fh, chunk))
}

// ── Unicode name decoding ─────────────────────────────────────────────────

/// Decode RAR4's compact Unicode filename encoding.
///
/// The name area contains a null-terminated ASCII base name followed by
/// a compact encoding that represents UTF-16 code units as differences
/// from the ASCII base.
fn decode_rar4_unicode_name(data: &[u8]) -> RarResult<String> {
    // Find null terminator separating ASCII base from Unicode encoding
    let null_pos = data.iter().position(|&b| b == 0);
    let (ascii_part, encoded) = match null_pos {
        Some(p) => (&data[..p], &data[p + 1..]),
        None => {
            // No Unicode encoding, just ASCII
            return Ok(String::from_utf8_lossy(data).into_owned());
        }
    };

    if encoded.is_empty() {
        return Ok(String::from_utf8_lossy(ascii_part).into_owned());
    }

    let mut utf16_chars: Vec<u16> = Vec::with_capacity(ascii_part.len());
    let mut enc_pos = 0;
    let mut ascii_pos = 0;
    let mut high_byte: u8 = 0;

    while enc_pos < encoded.len() {
        let flags_byte = encoded[enc_pos];
        enc_pos += 1;

        // Process 4 pairs of 2-bit flags per byte
        for shift in (0..8).step_by(2).rev() {
            if enc_pos >= encoded.len() {
                break;
            }
            let flag = (flags_byte >> shift) & 0x03;

            match flag {
                0 => {
                    // Use next byte as-is (low byte, high=0)
                    if enc_pos >= encoded.len() {
                        break;
                    }
                    let lo = encoded[enc_pos] as u16;
                    enc_pos += 1;
                    utf16_chars.push(lo);
                    ascii_pos += 1;
                }
                1 => {
                    // Use next byte as low byte, previous high byte
                    if enc_pos >= encoded.len() {
                        break;
                    }
                    let lo = encoded[enc_pos] as u16;
                    enc_pos += 1;
                    utf16_chars.push((high_byte as u16) << 8 | lo);
                    ascii_pos += 1;
                }
                2 => {
                    // Read high byte, then low byte
                    if enc_pos + 1 >= encoded.len() {
                        break;
                    }
                    high_byte = encoded[enc_pos];
                    enc_pos += 1;
                    let lo = encoded[enc_pos] as u16;
                    enc_pos += 1;
                    utf16_chars.push((high_byte as u16) << 8 | lo);
                    ascii_pos += 1;
                }
                3 => {
                    // Copy N+1 bytes from ASCII base
                    if enc_pos >= encoded.len() {
                        break;
                    }
                    let count = encoded[enc_pos] as usize + 1;
                    enc_pos += 1;
                    for _ in 0..count {
                        if ascii_pos < ascii_part.len() {
                            utf16_chars.push(ascii_part[ascii_pos] as u16);
                        }
                        ascii_pos += 1;
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    String::from_utf16(&utf16_chars)
        .map_err(|_| RarError::Format("invalid Unicode filename in RAR4 header".into()))
}

// ── DOS timestamp conversion ──────────────────────────────────────────────

/// Convert a 32-bit DOS timestamp to Unix epoch seconds.
///
/// DOS timestamp format:
/// - Bits  0-4:  seconds / 2
/// - Bits  5-10: minutes
/// - Bits 11-15: hours
/// - Bits 16-20: day (1-31)
/// - Bits 21-24: month (1-12)
/// - Bits 25-31: year - 1980
pub fn parse_dos_time(ftime: u32) -> u32 {
    let sec = ((ftime & 0x1F) * 2) as u32;
    let min = ((ftime >> 5) & 0x3F) as u32;
    let hour = ((ftime >> 11) & 0x1F) as u32;
    let day = ((ftime >> 16) & 0x1F) as u32;
    let month = ((ftime >> 21) & 0x0F) as u32;
    let year = ((ftime >> 25) & 0x7F) as u32 + 1980;

    // Convert to Unix timestamp using a simplified algorithm
    // Days from epoch (1970-01-01) to the given date
    let mut days: i64 = 0;

    // Years
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }

    // Months
    let month_days: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[(m - 1) as usize] as i64;
        if m == 2 && is_leap_year(year) {
            days += 1;
        }
    }

    // Days
    days += (day.max(1) - 1) as i64;

    let secs = days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
    secs.max(0) as u32
}

fn is_leap_year(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
