//! RAR4 header serialization and archive creation.
//!
//! This module writes the fixed-width RAR 3.x/4.x container format:
//! 7-byte signature, 13-byte main header, 32+N-byte file headers with
//! 16-bit CRC, and the 7-byte end-of-archive block.
//!
//! The member-addition orchestration (encoder dispatch, member encryption,
//! volume splitting and the parallel batch) lives in [`pipeline`].

mod cbc;
mod pipeline;

use crate::crc32;
use crate::error::{RarError, RarResult};
use crate::format::rar4::{
    COMM_HEAD, ENDARC_HEAD, FHD_UNICODE, FILE_HEAD, LONG_BLOCK, MAIN_HEAD, RAR4_METHOD_STORE,
};

/// RAR 1.5–4.x signature (7 bytes, not a real block header).
pub(crate) const RAR4_SIGNATURE: &[u8; 7] = b"Rar!\x1a\x07\x00";

/// Fixed main header size (CRC + type + flags + size + 2 reserved fields).
const MAIN_HEADER_SIZE: u16 = 13;

/// Base file header size (before name, salt, ext-time).
pub(crate) const FILE_HEADER_FIXED_SIZE: u16 = 32;

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
    /// Host OS (0 = DOS, 2 = Windows, 3 = Unix).
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
    /// On-disk file attributes: `0x20` (archive) for regular files,
    /// `0x10` (FILE_ATTRIBUTE_DIRECTORY) for directory members.
    pub attr: u32,
    /// Window-bits value (flags bits 5–7): 0..=6 encode a 64 KiB – 4 MiB
    /// dictionary for compressed members; 7 marks a directory member
    /// (see [`DIRECTORY_WINDOW_BITS`]).
    pub window_bits: u8,
    /// Optional 8-byte salt for encrypted members.
    pub salt: Option<[u8; 8]>,
    /// Optional extended time field (FHD_EXTTIME).
    pub ext_time: Option<&'a [u8]>,
}

/// Build a FILE_HEAD block. Returns the serialized header bytes (without the
/// data payload).
pub(crate) fn build_file_header(p: &FileHeaderParams<'_>) -> RarResult<Vec<u8>> {
    let name_len = p.name.len();
    let salt_len = if p.salt.is_some() { 8 } else { 0 };
    let ext_len = p.ext_time.map_or(0, |e| e.len());
    let head_size = FILE_HEADER_FIXED_SIZE as usize + name_len + salt_len + ext_len;

    let dict_flags = u16::from(p.window_bits) << 5;
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
    // file_attr (0x20 = archive bit for files, 0x10 = directory)
    buf.extend_from_slice(&p.attr.to_le_bytes());
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

/// Build a RAR4 per-file comment block (`COMM_HEAD` 0x75): the caller emits
/// it either standalone after the member data (RAR 3.x/4.x layout) or nested
/// after the extended-time area of a `FHD_COMMENT` `FILE_HEAD` (RAR 1.5–2.9
/// layout). The comment is stored uncompressed (method `0x30`); the payload
/// is the raw text bytes (UTF-8). Layout:
/// `HEAD_CRC(2) HEAD_TYPE(1)=0x75 HEAD_FLAGS(2) HEAD_SIZE(2) UNP_SIZE(2)
///  UNP_VER(1) METHOD(1)=0x30 COMM_CRC(2) payload`.
pub(crate) fn build_file_comment_block(comment: &[u8]) -> Vec<u8> {
    // Layout verified against a genuine RAR2 archive comment
    // (`tests/fixtures/rar40/rar2/comment_nopsw.rar`): HEAD_CRC(2) +
    // HEAD_TYPE(1) + HEAD_FLAGS(2) + HEAD_SIZE(2) + UNP_SIZE(2) + UNP_VER(1)
    // + METHOD(1) + COMM_CRC(2), then the payload. UnRAR rejected the previous
    // 12-byte variant (no UNP_SIZE) with "file header is corrupt".
    let head_size = 13 + comment.len();
    let mut buf = Vec::with_capacity(head_size);
    buf.extend_from_slice(&[0u8; 2]); // HEAD_CRC placeholder
    buf.push(COMM_HEAD);
    buf.extend_from_slice(&0u16.to_le_bytes()); // HEAD_FLAGS
    buf.extend_from_slice(&(head_size as u16).to_le_bytes()); // HEAD_SIZE
    buf.extend_from_slice(&(comment.len() as u16).to_le_bytes()); // UNP_SIZE
    buf.push(29); // UNP_VER (RAR4)
    buf.push(RAR4_METHOD_STORE); // METHOD (store)
    buf.extend_from_slice(&header_crc16(comment).to_le_bytes()); // COMM_CRC
    buf.extend_from_slice(comment);
    // HEAD_CRC covers the 11-byte subblock header body only; the comment
    // payload follows the covered region (verified against a genuine block:
    // its stored CRC matching `crc32(body[2:13])`, not `[2:head_size]`).
    let crc = header_crc16(&buf[2..13]);
    buf[..2].copy_from_slice(&crc.to_le_bytes());
    buf
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

/// Encrypt a RAR4 block header for a `-hp` header-encrypted archive.
///
/// Every block after the main header (file headers, end-of-archive) is
/// stored on disk as `[8-byte salt][AES-128-CBC ciphertext]`, where the
/// ciphertext is the full block header (7-byte prefix + body) zero-padded
/// to a 16-byte multiple. This mirrors the read side (`read_encrypted_block`
/// in `format/rar4/mod.rs`), which reads the 8-byte salt, derives the key, and
/// decrypts `align16(head_size)` bytes back into the header.
///
/// Returns the bytes to write and their on-disk length (`8 + align16`).
pub(crate) fn encrypt_block_header(header: &[u8], password: &str) -> RarResult<(Vec<u8>, u64)> {
    let align16 = (header.len() + 15) & !15;
    let mut plain = header.to_vec();
    plain.resize(align16, 0);
    let mut salt = [0u8; 8];
    rand::fill(&mut salt);
    let mut cipher = crate::crypto::Rar30Cipher::new(password.as_bytes(), Some(salt))
        .map_err(|e| RarError::Format(format!("RAR4 header key setup: {e:?}")))?;
    cipher
        .encrypt_in_place(&mut plain)
        .map_err(|e| RarError::Format(format!("RAR4 header encrypt: {e:?}")))?;
    let mut out = Vec::with_capacity(8 + plain.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&plain);
    let on_disk_len = 8u64 + align16 as u64;
    Ok((out, on_disk_len))
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
    // Encode as RAR4 Unicode: null-terminated ASCII fallback + Unicode
    // extension. The extension is a byte-aligned stream of 2-bit mode codes
    // (4 per flag byte, MSB first) interleaved with each code's data:
    // mode 0 = single byte (`unit < 0x100`), mode 2 = two bytes LE.
    // The decoder (rar40::decode_file_name) reads a flag byte, then the
    // data bytes of the up-to-four codes it describes, in order.
    let ascii_fallback: Vec<u8> = name
        .chars()
        .map(|c| if c.is_ascii() { c as u8 } else { b'?' })
        .chain(std::iter::once(0))
        .collect();

    let utf16: Vec<u16> = name.encode_utf16().collect();
    let mut ext = Vec::new();
    // High byte shared by mode-1 codes (units in the 0xFFxx range). We only
    // emit mode 2 for units >= 0x100, so this value is inert; 0xFF is the
    // conventional WinRAR value.
    ext.push(0xFF);

    let mut modes = [0u8; 4];
    let mut data = Vec::<u8>::new();
    let mut group = 0usize;
    for &unit in &utf16 {
        if unit <= 0xFF {
            // Mode 0: single byte.
            modes[group] = 0;
            data.push(unit as u8);
        } else {
            // Mode 2: two bytes, low first.
            modes[group] = 2;
            data.push((unit & 0xFF) as u8);
            data.push((unit >> 8) as u8);
        }
        group += 1;
        if group == 4 {
            ext.push(encode_flag_byte(&modes[..4]));
            ext.extend_from_slice(&data);
            data.clear();
            group = 0;
        }
    }
    if group > 0 {
        ext.push(encode_flag_byte(&modes[..group]));
        ext.extend_from_slice(&data);
    }

    let mut result = ascii_fallback;
    result.extend_from_slice(&ext);
    (result, FHD_UNICODE)
}

/// Pack up to four 2-bit mode codes into one flag byte (MSB first); unused
/// low slots are zero-padded.
fn encode_flag_byte(modes: &[u8]) -> u8 {
    debug_assert!(modes.len() <= 4 && modes.iter().all(|&m| m < 4));
    let mut flag = 0u8;
    for &m in modes {
        flag = (flag << 2) | m;
    }
    flag <<= 2 * (4 - modes.len());
    flag
}

// ── DOS time encoding ───────────────────────────────────────────────────────

/// Seconds east of UTC for the local time zone, at "now" (minute precision;
/// targets without a local-time API report UTC).
fn local_offset_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    #[cfg(windows)]
    {
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st) };
        let civil = crate::format::rar4::days_from_civil(
            i64::from(st.wYear),
            u32::from(st.wMonth),
            u32::from(st.wDay),
        ) * 86_400
            + i64::from(st.wHour) * 3_600
            + i64::from(st.wMinute) * 60
            + i64::from(st.wSecond);
        civil - utc
    }
    #[cfg(unix)]
    {
        let secs = utc as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };
        let civil = crate::format::rar4::days_from_civil(
            i64::from(tm.tm_year) + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
        ) * 86_400
            + i64::from(tm.tm_hour) * 3_600
            + i64::from(tm.tm_min) * 60
            + i64::from(tm.tm_sec);
        civil - utc
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = utc;
        0
    }
}

/// Convert a Unix instant to the "local civil" seconds the legacy catalog
/// stores (the encoding [`crate::format::rar4::dos_time_to_unix`] produces).
pub(crate) fn epoch_to_local_civil(secs: u32) -> u32 {
    (i64::from(secs) + local_offset_secs()).clamp(0, u32::MAX as i64) as u32
}

/// Convert a legacy "local civil" time back to a Unix instant.
pub(crate) fn local_civil_to_epoch(secs: u32) -> u32 {
    (i64::from(secs) - local_offset_secs()).clamp(0, u32::MAX as i64) as u32
}

/// Convert a Unix timestamp (seconds since epoch) to the RAR4/RAR13 DOS
/// time field. The field stores *local* wall-clock time (WinRAR's
/// convention); pre-1980 years wrap like the official writers instead of
/// underflowing.
pub(crate) fn unix_to_dos_time(secs: u32) -> u32 {
    let local = epoch_to_local_civil(secs);
    let days = local / 86_400;
    let time_of_day = local % 86_400;
    let hour = time_of_day / 3_600;
    let minute = (time_of_day % 3_600) / 60;
    let second = time_of_day % 60;

    // Howard Hinnant's civil_from_days: 1970-01-01 = day 0.
    let z = i64::from(days) + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = y + i64::from(month <= 2);

    // Pack into DOS format: Y(7) M(4) D(5) H(5) M(6) S(5/2). A pre-1980
    // year wraps into the 7-bit field exactly like WinRAR's writers.
    let year_bits = ((year - 1980) & 0x7F) as u32;
    (year_bits << 25) | (month << 21) | (day << 16) | (hour << 11) | (minute << 5) | (second / 2)
}

// ── Extended time field ─────────────────────────────────────────────────────

/// Build the FHD_EXTTIME field for a file header.
///
/// The ext-time field carries sub-second precision and the DOS field's
/// odd-second flag for mtime. RAR4 stores the 100-ns tick count as three
/// bytes, least significant first, with a 2-byte flags word whose bits 12-15
/// encode PRESENT (`0x8`), ADD_SECOND (`0x4`) and the byte count. This
/// matches what WinRAR writes and what `extract_mtime_refinement` decodes.
pub(crate) fn build_ext_time(mtime: u32, mtime_ns: Option<u32>) -> Option<Vec<u8>> {
    // DOS seconds have 2-second resolution; an odd local second is
    // recovered through ADD_SECOND.
    let add_second = epoch_to_local_civil(mtime) % 2 == 1;
    let ticks = mtime_ns.unwrap_or(0) / 100; // 100-ns ticks.
    if ticks == 0 && !add_second {
        return None;
    }

    let byte_count: u16 = if ticks == 0 { 0 } else { 3 };
    let mut ext = Vec::with_capacity(2 + byte_count as usize);
    let flags: u16 = (0x8 | if add_second { 0x4 } else { 0 } | byte_count) << 12;
    ext.extend_from_slice(&flags.to_le_bytes());
    if ticks != 0 {
        ext.push((ticks & 0xFF) as u8);
        ext.push(((ticks >> 8) & 0xFF) as u8);
        ext.push((ticks >> 16) as u8);
    }
    Some(ext)
}

/// Encode a dictionary size (in bytes) into the upper bits of the FILE_HEAD
/// flags word (bits 5–7). Test-only: production code passes the 3-bit
/// `window_bits` straight to [`build_file_header`], so this pins the
/// encoding against the shifted flags-mask representation.
#[cfg(test)]
fn dictionary_flags(size: usize) -> RarResult<u16> {
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

/// The flags value for a RAR4 directory member (bits 5–7 all set), used by
/// the tests to check the `window_bits == 7` encoding.
#[cfg(test)]
const DIRECTORY_WINDOW_BITS: u16 = 0x00E0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::rar4::RAR4_METHOD_STORE;

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
            attr: 0x20,
            salt: None,
            ext_time: None,
            window_bits: 6, // 4 MiB dictionary
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
        // 2024-01-15 12:30:44 UTC; the DOS field stores the *local* wall
        // clock with 2-second resolution.
        let secs = 19_737u32 * 86_400 + 45_044;
        let dos = unix_to_dos_time(secs);
        let local = epoch_to_local_civil(secs);
        let round = crate::format::rar4::dos_time_to_unix(dos);
        assert_eq!(round, local - local % 2, "fields round-trip to local time");
        let year = ((dos >> 25) & 0x7f) + 1980;
        assert_eq!(year, 2024, "year stays on the same local day");
    }

    #[test]
    fn dos_time_pre_1980_wraps_without_panicking() {
        // 1970-01-01 is before the DOS epoch; WinRAR's writers wrap the
        // 7-bit year field instead of failing.
        let dos = unix_to_dos_time(0);
        let year = ((dos >> 25) & 0x7f) + 1980;
        assert!((1980..=2107).contains(&year), "wrapped year {year}");
        let local = epoch_to_local_civil(0);
        assert_eq!((dos >> 11) & 0x1f, (local / 3600) % 24, "hour preserved");
    }

    #[test]
    fn ext_time_marks_odd_local_seconds() {
        let odd_epoch = local_civil_to_epoch(1_700_000_001); // odd second
        let ext = build_ext_time(odd_epoch, None).expect("odd second needs a record");
        assert_eq!(ext, vec![0x00, 0xC0], "PRESENT|ADD_SECOND, 0 tick bytes");

        let even_epoch = local_civil_to_epoch(1_700_000_002);
        assert!(build_ext_time(even_epoch, None).is_none());

        let ext = build_ext_time(even_epoch, Some(123_456_700)).expect("ticks need a record");
        assert_eq!(
            u16::from_le_bytes([ext[0], ext[1]]) >> 12,
            0xB,
            "PRESENT + 3 tick bytes"
        );
    }

    #[test]
    fn dictionary_flags_sizes() {
        // Window bits 0..=6 encode 64 KiB ..= 4 MiB dictionaries.
        for (size, bits) in [
            (0x1_0000usize, 0u16),
            (0x2_0000, 1),
            (0x4_0000, 2),
            (0x8_0000, 3),
            (0x10_0000, 4),
            (0x20_0000, 5),
            (0x40_0000, 6),
        ] {
            assert_eq!(dictionary_flags(size).unwrap(), bits << 5);
        }
        assert!(dictionary_flags(0x80_0000).is_err());
        // Bit 7 (all window bits set) marks a directory member.
        assert_eq!(DIRECTORY_WINDOW_BITS, 7 << 5);
        assert_eq!(
            build_file_header(&FileHeaderParams {
                flags: 0,
                packed_size: 0,
                unpacked_size: 0,
                host_os: 0,
                file_crc: 0,
                file_time: 0,
                unp_ver: 20,
                method: RAR4_METHOD_STORE,
                name: b"d",
                attr: 0x10,
                window_bits: 7,
                salt: None,
                ext_time: None,
            })
            .unwrap()[3..5],
            (LONG_BLOCK | DIRECTORY_WINDOW_BITS).to_le_bytes()
        );
    }
}
