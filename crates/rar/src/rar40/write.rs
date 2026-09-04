//! RAR4 header serialization and archive creation.
//!
//! This module writes the fixed-width RAR 3.x/4.x container format:
//! 7-byte signature, 13-byte main header, 32+N-byte file headers with
//! 16-bit CRC, and the 7-byte end-of-archive block.

#![allow(dead_code)]

use crate::crc32;
use crate::error::{RarError, RarResult};
use crate::rar40::{ENDARC_HEAD, FHD_UNICODE, FILE_HEAD, LONG_BLOCK, MAIN_HEAD};

/// RAR 1.5–4.x signature (7 bytes, not a real block header).
pub(crate) const RAR4_SIGNATURE: &[u8; 7] = b"Rar!\x1a\x07\x00";

/// Fixed main header size (CRC + type + flags + size + 2 reserved fields).
const MAIN_HEADER_SIZE: u16 = 13;

/// Base file header size (before name, salt, ext-time).
const FILE_HEADER_FIXED_SIZE: u16 = 32;

/// End-of-archive header size.
const ENDARC_HEADER_SIZE: u16 = 7;

// ── CRC16 helper ────────────────────────────────────────────────────────────

/// Compute the RAR4 header CRC: standard CRC-32 truncated to 16 bits.
fn header_crc16(body: &[u8]) -> u16 {
    (crc32::crc32(body) & 0xFFFF) as u16
}

/// Patch the CRC16 at position `start` in `buf`, covering bytes `[start+2..]`.
fn patch_crc16(buf: &mut [u8], start: usize) {
    let crc = header_crc16(&buf[start + 2..]);
    buf[start..start + 2].copy_from_slice(&crc.to_le_bytes());
}

// ── Signature ───────────────────────────────────────────────────────────────

/// Write the 7-byte RAR4 signature.
pub(crate) fn write_signature(out: &mut impl std::io::Write) -> RarResult<()> {
    out.write_all(RAR4_SIGNATURE)?;
    Ok(())
}

// ── Main header ─────────────────────────────────────────────────────────────

/// Build a 13-byte MAIN_HEAD block.
///
/// `flags` carries the MHD_* bits (e.g. `MHD_SOLID | MHD_PASSWORD | MHD_VOLUME`).
/// The `LONG_BLOCK` bit is always set (required for readers to parse the
/// head_size field).
pub(crate) fn build_main_header(flags: u16) -> [u8; 13] {
    let mut buf = [0u8; 13];
    // CRC filled last.
    buf[2] = MAIN_HEAD;
    let flags_with_long = flags | LONG_BLOCK;
    buf[3..5].copy_from_slice(&flags_with_long.to_le_bytes());
    buf[5..7].copy_from_slice(&MAIN_HEADER_SIZE.to_le_bytes());
    // reserved1 (2 bytes) + reserved2 (4 bytes) stay zero.
    patch_crc16(&mut buf, 0);
    buf
}

/// Write the main header to the output stream.
pub(crate) fn write_main_header(out: &mut impl std::io::Write, flags: u16) -> RarResult<()> {
    out.write_all(&build_main_header(flags))?;
    Ok(())
}

// ── Dictionary size flags ───────────────────────────────────────────────────

/// Encode a dictionary size (in bytes) into the upper bits of the FILE_HEAD
/// flags word (bits 5–7). Returns the flags-with-dict value.
///
/// Accepted sizes: 64 KiB – 4 MiB (powers of two).
/// Returns `Err` for unsupported sizes.
pub(crate) fn dictionary_flags(size: usize) -> RarResult<u16> {
    let bits: u16 = match size {
        0x1_0000 => 0,  // 64 KiB
        0x2_0000 => 1,  // 128 KiB
        0x4_0000 => 2,  // 256 KiB
        0x8_0000 => 3,  // 512 KiB
        0x10_0000 => 4, // 1 MiB
        0x20_0000 => 5, // 2 MiB
        0x40_0000 => 6, // 4 MiB
        _ => {
            return Err(RarError::Format(format!(
                "unsupported RAR4 dictionary size: {size} bytes"
            )));
        }
    };
    Ok(bits << 5)
}

// ── File header ─────────────────────────────────────────────────────────────

/// Parameters for building a FILE_HEAD block.
pub(crate) struct FileHeaderParams<'a> {
    /// FHD_* flags (without LONG_BLOCK, without dict bits — those are added
    /// automatically).
    pub flags: u16,
    /// Compressed data size on disk (after encryption padding).
    pub packed_size: u32,
    /// Original uncompressed size.
    pub unpacked_size: u32,
    /// Host OS (0 = DOS, 3 = Unix).
    pub host_os: u8,
    /// CRC-32 of the uncompressed data.
    pub file_crc: u32,
    /// DOS-format modification time (10/6/6 packed fields).
    pub file_time: u32,
    /// Compression version (15, 20, 26, or 29).
    pub unp_ver: u8,
    /// Method byte (0x30 = store, 0x31–0x35 = m1–m5).
    pub method: u8,
    /// File name bytes (already encoded; use `encode_file_name` for Unicode).
    pub name: &'a [u8],
    /// Optional 8-byte salt for encrypted members.
    pub salt: Option<[u8; 8]>,
    /// Optional extended time field (FHD_EXTTIME).
    pub ext_time: Option<&'a [u8]>,
    /// Dictionary size in bytes (encoded into flags bits 5–7).
    pub dict_size: usize,
}

/// Build a FILE_HEAD block. Returns the serialized header bytes (without the
/// data payload).
pub(crate) fn build_file_header(p: &FileHeaderParams<'_>) -> RarResult<Vec<u8>> {
    let name_len = p.name.len();
    let salt_len = if p.salt.is_some() { 8 } else { 0 };
    let ext_len = p.ext_time.map_or(0, |e| e.len());
    let head_size = FILE_HEADER_FIXED_SIZE as usize + name_len + salt_len + ext_len;

    let dict_flags = dictionary_flags(p.dict_size)?;
    let flags = p.flags | dict_flags | LONG_BLOCK;

    let mut buf = Vec::with_capacity(head_size);
    // CRC placeholder (2 bytes)
    buf.extend_from_slice(&[0u8; 2]);
    // head_type
    buf.push(FILE_HEAD);
    // flags
    buf.extend_from_slice(&flags.to_le_bytes());
    // head_size
    buf.extend_from_slice(&(head_size as u16).to_le_bytes());
    // packed_size
    buf.extend_from_slice(&p.packed_size.to_le_bytes());
    // unpacked_size
    buf.extend_from_slice(&p.unpacked_size.to_le_bytes());
    // host_os
    buf.push(p.host_os);
    // file_crc
    buf.extend_from_slice(&p.file_crc.to_le_bytes());
    // file_time
    buf.extend_from_slice(&p.file_time.to_le_bytes());
    // unp_ver
    buf.push(p.unp_ver);
    // method
    buf.push(p.method);
    // name_size
    buf.extend_from_slice(&(name_len as u16).to_le_bytes());
    // file_attr (Unix attr: 0x20 = archive bit for files)
    buf.extend_from_slice(&0x20u32.to_le_bytes());
    // name
    buf.extend_from_slice(p.name);
    // salt
    if let Some(salt) = &p.salt {
        buf.extend_from_slice(salt);
    }
    // ext_time
    if let Some(ext) = p.ext_time {
        buf.extend_from_slice(ext);
    }

    // Patch CRC16.
    patch_crc16(&mut buf, 0);
    Ok(buf)
}

/// Write a FILE_HEAD block to the output stream.
pub(crate) fn write_file_header(
    out: &mut impl std::io::Write,
    p: &FileHeaderParams<'_>,
) -> RarResult<()> {
    let buf = build_file_header(p)?;
    out.write_all(&buf)?;
    Ok(())
}

// ── End-of-archive ──────────────────────────────────────────────────────────

/// Build a 7-byte ENDARC_HEAD block.
///
/// For header-encrypted archives (`-hp`), this marks the end of the
/// encrypted group. For plain archives, this block is optional but
/// WinRAR writes it anyway.
pub(crate) fn build_endarc(flags: u16) -> [u8; 7] {
    let mut buf = [0u8; 7];
    buf[2] = ENDARC_HEAD;
    buf[3..5].copy_from_slice(&flags.to_le_bytes());
    buf[5..7].copy_from_slice(&ENDARC_HEADER_SIZE.to_le_bytes());
    patch_crc16(&mut buf, 0);
    buf
}

/// Write the end-of-archive block.
pub(crate) fn write_endarc(out: &mut impl std::io::Write, flags: u16) -> RarResult<()> {
    out.write_all(&build_endarc(flags))?;
    Ok(())
}

// ── Filename encoding ───────────────────────────────────────────────────────

/// Encode a filename for the RAR4 FILE_HEAD.
///
/// Returns `(encoded_name, flags)` where `flags` includes `FHD_UNICODE` if
/// the name was encoded using the RAR4 Unicode extension.
pub(crate) fn encode_file_name(name: &str) -> (Vec<u8>, u16) {
    // If the name is pure ASCII, store it as-is (no FHD_UNICODE).
    if name.is_ascii() {
        return (name.as_bytes().to_vec(), 0);
    }
    // Encode as RAR4 Unicode: null-terminated ASCII fallback + Unicode extension.
    let ascii_fallback: Vec<u8> = name
        .chars()
        .map(|c| if c.is_ascii() { c as u8 } else { b'?' })
        .chain(std::iter::once(0))
        .collect();

    let utf16: Vec<u16> = name.encode_utf16().collect();
    let mut ext = Vec::new();
    // High byte for the Unicode extension (usually 0xFF for Latin-1 supplement).
    let high_byte = 0xFFu8;
    ext.push(high_byte);

    // Simple encoding: store each code unit as 2 bytes (mode 2).
    let mut flag_byte = 0u8;
    let mut flag_bits = 0u8;

    for &unit in &utf16 {
        if unit <= 0xFF {
            // Mode 0: single byte.
            if flag_bits == 6 {
                ext.push(flag_byte);
                flag_byte = 0;
                flag_bits = 0;
            }
            flag_byte <<= 2;
            flag_bits += 2;
            ext.push(unit as u8);
        } else {
            // Mode 2: two bytes.
            if flag_bits == 6 {
                ext.push(flag_byte);
                flag_byte = 0;
                flag_bits = 0;
            }
            flag_byte = (flag_byte << 2) | 2;
            flag_bits += 2;
            ext.push((unit & 0xFF) as u8);
            ext.push((unit >> 8) as u8);
        }
    }
    // Pad flag byte if needed.
    if flag_bits > 0 {
        flag_byte <<= 6 - flag_bits;
        ext.push(flag_byte);
    }

    let mut result = ascii_fallback;
    result.extend_from_slice(&ext);
    (result, FHD_UNICODE)
}

// ── DOS time encoding ───────────────────────────────────────────────────────

/// Convert a Unix timestamp (seconds since epoch) to RAR4 DOS format time.
pub(crate) fn unix_to_dos_time(secs: u32) -> u32 {
    // Compute date components from Unix timestamp.
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;

    // Convert days since 1970-01-01 to year/month/day.
    // civil_from_days from Howard Hinnant (https://howardhinnant.github.io/date_algorithms.html).
    // The algorithm takes days from epoch as input where 1970-01-01 = 0.
    // Our `days` is secs/86400 which is the number of complete days since epoch.
    // Hinnant's civil_from_days uses 0-based day count; our `days` already
    // matches that convention (1970-01-01 = day 0).
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]

    let year = y + u32::from(m <= 2);
    let month = m;
    let day = d;

    // Pack into DOS format: Y(7) M(4) D(5) H(5) M(6) S(5/2).
    ((year - 1980) << 25)
        | (month << 21)
        | (day << 16)
        | (hour << 11)
        | (minute << 5)
        | (second / 2)
}

// ── Extended time field ─────────────────────────────────────────────────────

/// Build the FHD_EXTTIME field for a file header.
///
/// The ext-time field carries sub-second precision for mtime (and optionally
/// ctime, atime). Format: 2-byte flags word + up to 4 bytes of tick count.
pub(crate) fn build_ext_time(mtime_ns: Option<u32>) -> Option<Vec<u8>> {
    let ns = mtime_ns?;
    let ticks = ns / 100; // Convert nanoseconds to 100-ns ticks.
    if ticks == 0 {
        return None;
    }

    // Encode ticks as 1–3 bytes (big-endian, high bytes first).
    let mut tick_bytes = Vec::new();
    if ticks <= 0xFF {
        tick_bytes.push(ticks as u8);
    } else if ticks <= 0xFFFF {
        tick_bytes.extend_from_slice(&(ticks as u16).to_be_bytes());
    } else {
        tick_bytes.push((ticks >> 16) as u8);
        tick_bytes.push(((ticks >> 8) & 0xFF) as u8);
        tick_bytes.push((ticks & 0xFF) as u8);
    }

    // mtime flags: PRESENT (0x8) + byte count (1-3).
    let mtime_flags: u16 = 0x8 | (tick_bytes.len() as u16);
    let flags = mtime_flags << 12;

    let mut ext = Vec::with_capacity(2 + tick_bytes.len());
    ext.extend_from_slice(&flags.to_le_bytes());
    ext.extend_from_slice(&tick_bytes);
    Some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rar40::RAR4_METHOD_STORE;

    #[test]
    fn signature_is_correct_length() {
        assert_eq!(RAR4_SIGNATURE.len(), 7);
        assert_eq!(&RAR4_SIGNATURE[..5], b"Rar!\x1a");
    }

    #[test]
    fn main_header_crc16() {
        let hdr = build_main_header(0);
        assert_eq!(hdr.len(), 13);
        // Type byte at offset 2.
        assert_eq!(hdr[2], MAIN_HEAD);
        // Flags at offset 3-4 (LONG_BLOCK always set).
        let flags = u16::from_le_bytes([hdr[3], hdr[4]]);
        assert_ne!(flags & LONG_BLOCK, 0);
        // head_size at offset 5-6.
        let head_size = u16::from_le_bytes([hdr[5], hdr[6]]);
        assert_eq!(head_size, MAIN_HEADER_SIZE);
        // CRC should be non-zero (unless header body happens to CRC to 0).
        let crc = u16::from_le_bytes([hdr[0], hdr[1]]);
        let expected_crc = header_crc16(&hdr[2..]);
        assert_eq!(crc, expected_crc);
    }

    #[test]
    fn file_header_roundtrip_fields() {
        let name = b"test.txt";
        let params = FileHeaderParams {
            flags: 0,
            packed_size: 100,
            unpacked_size: 200,
            host_os: 0,
            file_crc: 0xDEADBEEF,
            file_time: 0x12345678,
            unp_ver: 29,
            method: RAR4_METHOD_STORE,
            name,
            salt: None,
            ext_time: None,
            dict_size: 0x40_0000, // 4 MiB
        };
        let buf = build_file_header(&params).unwrap();
        // 32 fixed + 8 name + 0 salt + 0 ext = 40.
        assert_eq!(buf.len(), 40);
        // head_type
        assert_eq!(buf[2], FILE_HEAD);
        // packed_size at offset 7
        assert_eq!(u32::from_le_bytes(buf[7..11].try_into().unwrap()), 100);
        // unpacked_size at offset 11
        assert_eq!(u32::from_le_bytes(buf[11..15].try_into().unwrap()), 200);
        // unp_ver at offset 24
        assert_eq!(buf[24], 29);
        // method at offset 25
        assert_eq!(buf[25], RAR4_METHOD_STORE);
        // name_size at offset 26
        assert_eq!(u16::from_le_bytes(buf[26..28].try_into().unwrap()), 8);
        // name starts at offset 32
        assert_eq!(&buf[32..40], name);
    }

    #[test]
    fn endarc_block() {
        let buf = build_endarc(0x4000);
        assert_eq!(buf.len(), 7);
        assert_eq!(buf[2], ENDARC_HEAD);
        let flags = u16::from_le_bytes([buf[3], buf[4]]);
        assert_eq!(flags, 0x4000);
        let head_size = u16::from_le_bytes([buf[5], buf[6]]);
        assert_eq!(head_size, ENDARC_HEADER_SIZE);
    }

    #[test]
    fn encode_ascii_name_no_unicode_flag() {
        let (encoded, flags) = encode_file_name("hello.txt");
        assert_eq!(&encoded, b"hello.txt");
        assert_eq!(flags & FHD_UNICODE, 0);
    }

    #[test]
    fn encode_unicode_name_sets_flag() {
        let (encoded, flags) = encode_file_name("café.txt");
        assert_ne!(flags & FHD_UNICODE, 0);
        // Should contain the ASCII fallback.
        let fallback_end = encoded.iter().position(|&b| b == 0).unwrap();
        assert_eq!(&encoded[..fallback_end], b"caf?.txt");
    }

    #[test]
    fn dos_time_roundtrip() {
        // 2024-01-15 12:30:44 UTC (DOS time has 2-second resolution).
        // days from epoch to 2024-01-15 = 19737; secs = 19737*86400 + 45044
        let secs = 19_737u32 * 86_400 + 45_044;
        let dos = unix_to_dos_time(secs);
        let year = ((dos >> 25) & 0x7f) + 1980;
        let month = (dos >> 21) & 0x0f;
        let day = (dos >> 16) & 0x1f;
        let hour = (dos >> 11) & 0x1f;
        let minute = (dos >> 5) & 0x3f;
        let second = (dos & 0x1f) * 2;
        assert_eq!(year, 2024);
        assert_eq!(month, 1);
        assert_eq!(day, 15);
        assert_eq!(hour, 12);
        assert_eq!(minute, 30);
        assert_eq!(second, 44);
    }

    #[test]
    fn dictionary_flags_sizes() {
        assert_eq!(dictionary_flags(0x1_0000).unwrap(), 0 << 5);
        assert_eq!(dictionary_flags(0x40_0000).unwrap(), 6 << 5);
        assert!(dictionary_flags(0x80_0000).is_err());
    }
}
