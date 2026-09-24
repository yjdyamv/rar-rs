//! RAR4 header serialization and archive creation.
//!
//! This module writes the fixed-width RAR 3.x/4.x container format:
//! 7-byte signature, 13-byte main header, 32+N-byte file headers with
//! 16-bit CRC, and the 7-byte end-of-archive block.
//!
//! The member-addition orchestration is split by role: [`member`] holds the
//! member entry points and catalog bookkeeping, [`encode`] the codec and
//! cipher dispatch, [`emit`] the segment/volume emission, [`stream`] the
//! bounded-memory large-member path and [`batch`] the parallel batch (feature
//! `parallel`).

#[cfg(feature = "parallel")]
pub(crate) mod batch;
mod cbc;
pub(crate) mod emit;
pub(crate) mod encode;
pub(crate) mod member;
pub(crate) mod stream;

use crate::error::{RarError, RarResult};
use crate::format::rar4::{
    COMM_HEAD, ENDARC_HEAD, FHD_UNICODE, FILE_HEAD, LONG_BLOCK, MAIN_HEAD, RAR4_METHOD_STORE,
};
use crate::format::shared::checksum::header_crc16;
use crate::format::shared::legacy_time::epoch_to_local_civil;

/// Fixed main header size (CRC + type + flags + size + 2 reserved fields).
const MAIN_HEADER_SIZE: u16 = 13;

/// Base file header size (before name, salt, ext-time).
pub(crate) const FILE_HEADER_FIXED_SIZE: u16 = 32;

/// End-of-archive header size of a single-volume archive (the short form).
const ENDARC_HEADER_SIZE: u16 = 7;

/// End-of-archive header size of a multi-volume set. Every volume of a set
/// carries the 20-byte form (WinRAR 4.20+), whatever the naming family.
const ENDARC_VOLUME_HEADER_SIZE: u16 = 20;

/// HEAD_FLAGS base of a volume-set ENDARC_HEAD (WinRAR writes `0x400e`;
/// `EHFL_NEXTVOLUME` — bit 0 — is ORed in on every volume but the last).
const ENDARC_VOLUME_FLAGS: u16 = 0x400e;

// ── CRC16 helper ────────────────────────────────────────────────────────────

/// Patch the CRC16 at position `start` in `buf`, covering bytes `[start+2..]`.
fn patch_crc16(buf: &mut [u8], start: usize) {
    let crc = header_crc16(&buf[start + 2..]);
    buf[start..start + 2].copy_from_slice(&crc.to_le_bytes());
}

// ── Main header ─────────────────────────────────────────────────────────────

/// Build a 13-byte MAIN_HEAD block.
///
/// `flags` carries the MHD_* bits (e.g. `MHD_SOLID | MHD_PASSWORD | MHD_VOLUME`).
/// WinRAR does **not** set `LONG_BLOCK` on the main header (the block has no
/// data area and its `head_size` sits at a fixed offset) — verified byte-for-byte
/// against WinRAR 3.00–6.23 `-ma4`; we match that.
pub(crate) fn build_main_header(flags: u16) -> [u8; 13] {
    let mut buf = [0u8; 13];
    // CRC filled last.
    buf[2] = MAIN_HEAD;
    buf[3..5].copy_from_slice(&flags.to_le_bytes());
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

/// Build the 7-byte ENDARC_HEAD block of a **single-volume** archive.
///
/// For header-encrypted archives (`-hp`), this marks the end of the
/// encrypted group. For plain archives, this block is optional but
/// WinRAR writes it anyway; its flags carry WinRAR's `0x4000` base and no
/// volume fields.
pub(crate) fn build_endarc_single() -> [u8; 7] {
    let mut buf = [0u8; 7];
    buf[2] = ENDARC_HEAD;
    buf[3..5].copy_from_slice(&0x4000u16.to_le_bytes());
    buf[5..7].copy_from_slice(&ENDARC_HEADER_SIZE.to_le_bytes());
    patch_crc16(&mut buf, 0);
    buf
}

/// Build the 20-byte ENDARC_HEAD block of a multi-volume set. Layout
/// (verified byte-for-byte against WinRAR 5.91/6.23):
/// `HEAD_CRC(2) HEAD_TYPE(1)=0x7b HEAD_FLAGS(2) HEAD_SIZE(2)=0x14
///  prefix_crc32(4) volume_index(2) 0(7)`.
///
/// `prefix_crc32` is the CRC-32 of the raw volume bytes written before this
/// block (the "protected prefix", the same range the inline recovery record
/// covers); `volume_index` is the zero-based volume number; `next_volume`
/// sets `EHFL_NEXTVOLUME` on every volume but the last. The seven trailing
/// zero bytes are what makes WinRAR's `use_trailer_format` pick the trailer
/// `.rev` layout.
pub(crate) fn build_endarc(next_volume: bool, prefix_crc32: u32, volume_index: u16) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf[2] = ENDARC_HEAD;
    let flags = ENDARC_VOLUME_FLAGS | u16::from(next_volume);
    buf[3..5].copy_from_slice(&flags.to_le_bytes());
    buf[5..7].copy_from_slice(&ENDARC_VOLUME_HEADER_SIZE.to_le_bytes());
    buf[7..11].copy_from_slice(&prefix_crc32.to_le_bytes());
    buf[11..13].copy_from_slice(&volume_index.to_le_bytes());
    // bytes 13..20 stay zero (the trailer the `.rev` layout keys on).
    patch_crc16(&mut buf, 0);
    buf
}

/// On-disk size of a volume-set ENDARC block, for volume-budget arithmetic:
/// the 20 plaintext bytes, or `[8-byte salt][align16(20)]` under `-hp`
/// header encryption. Every split/defer decision must reserve this much.
pub(crate) fn endarc_volume_reserve(header_encryption: bool) -> u64 {
    let plain = u64::from(ENDARC_VOLUME_HEADER_SIZE);
    if header_encryption {
        8 + plain.next_multiple_of(16)
    } else {
        plain
    }
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

/// Convert a Unix timestamp (seconds since epoch) to the RAR4/RAR13 DOS
/// time field. The conversion is shared with RAR 1.3/1.4; see
/// [`crate::format::shared::legacy_time::unix_to_dos_time`].
pub(crate) use crate::format::shared::legacy_time::unix_to_dos_time;

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

/// `FHD_EXTTIME` record for a member of a container whose member version is
/// `unp_ver`.
///
/// RAR 1.5/2.x (`unp_ver` 15/20) predate the extended-time area: their readers
/// size a plain file header as `32 + name` (no record), so an extra one shifts
/// the data offset and the header CRC no longer covers what they expect —
/// UnRAR 2.90 reports "the file header is corrupt" on such a member (verified
/// against the reference build). Only v29 members carry the record.
pub(crate) fn build_member_ext_time(
    unp_ver: u8,
    mtime: u32,
    mtime_ns: Option<u32>,
) -> Option<Vec<u8>> {
    if crate::version::LegacyCodec::from_unp_ver(unp_ver)
        != Some(crate::version::LegacyCodec::Rar29)
    {
        return None;
    }
    build_ext_time(mtime, mtime_ns)
}

/// The member `unp_ver` a RAR4 FILE_HEAD carries for `requested_level`: WinRAR
/// writes a RAR 2.x member (`20`) when the requested level is 0 and the
/// archive's own version (`29`) otherwise — even when a level ≥ 1 member ends
/// up STORE, and even inside a `v29` container. An **encrypted** member keeps
/// `29`: its `-p` layout (the RAR29 salt + block cipher) is the v29 one, so a
/// `20` header would make readers dispatch the wrong cipher. Verified against
/// WinRAR 6.23 `-ma4` (`-m0` writes 20, `-m1`..`-m5` and `-m0 -p` write 29).
pub(crate) fn member_unp_ver(archive_unp_ver: u8, requested_level: u8, encrypted: bool) -> u8 {
    if requested_level == 0 && !encrypted && archive_unp_ver >= 29 {
        20
    } else {
        archive_unp_ver
    }
}

/// WinRAR's `FHD` dictionary/window bits for a RAR4 archive whose largest
/// member (non-solid) or whole run (solid) spans `size` uncompressed bytes:
/// `clamp(ceil_log2(size) - 16, min, 6)`, `min` 1 (128 KiB) non-solid and 4
/// (1 MiB) solid. The bits are archive-wide, not per member — verified against
/// WinRAR 6.23 `-ma4`: non-solid 60000→1, 200000→2, 500000→3, 2M→5, 8M→6 and
/// an 8 MiB + 10 KiB pair gets 6 for both; solid 60000/200000/500000→4, 2M→5,
/// 8M→6. The declared window never falls below the member (or run) size, so it
/// can never be smaller than the window the encoder actually references.
pub(crate) fn dict_bits(size: u64, solid: bool) -> u8 {
    let ceil_log2 = if size == 0 {
        0
    } else {
        64 - (size - 1).leading_zeros()
    };
    let min = if solid { 4 } else { 1 };
    ceil_log2.saturating_sub(16).clamp(min, 6) as u8
}

/// The archive-wide `FHD` window bits a RAR4 archive declares, by member
/// version. The RAR 2.x family (`unp_ver` < 29) predates the size-driven rule
/// and always declares the era's 1 MiB dictionary — WinRAR 2.90 writes 4 for
/// every archive (store/compress/solid alike), and 1 MiB is also the RAR 2.x
/// dictionary ceiling, so the size-based rule would over-declare there. RAR
/// 3.0+ (`29`) uses [`dict_bits`].
pub(crate) fn archive_dict_bits(unp_ver: u8, size: u64, solid: bool) -> u8 {
    if unp_ver < 29 {
        4
    } else {
        dict_bits(size, solid)
    }
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
    use crate::format::shared::legacy_time::local_civil_to_epoch;

    #[test]
    fn signature_is_correct_length() {
        assert_eq!(crate::detect::RAR4_SIGNATURE.len(), 7);
        assert_eq!(&crate::detect::RAR4_SIGNATURE[..5], b"Rar!\x1a");
    }

    #[test]
    fn main_header_crc16() {
        let hdr = build_main_header(0);
        assert_eq!(hdr.len(), 13);
        // Type byte at offset 2.
        assert_eq!(hdr[2], MAIN_HEAD);
        // Flags at offset 3-4: WinRAR never sets LONG_BLOCK on the main
        // header (it has no data area).
        let flags = u16::from_le_bytes([hdr[3], hdr[4]]);
        assert_eq!(flags & LONG_BLOCK, 0);
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
    fn endarc_single_block() {
        let buf = build_endarc_single();
        assert_eq!(buf.len(), 7);
        assert_eq!(buf[2], ENDARC_HEAD);
        let flags = u16::from_le_bytes([buf[3], buf[4]]);
        assert_eq!(flags, 0x4000);
        let head_size = u16::from_le_bytes([buf[5], buf[6]]);
        assert_eq!(head_size, ENDARC_HEADER_SIZE);
        // WinRAR's constant single-volume ENDARC: flags 0x4000, head_size 7.
        assert_eq!(buf, [0xc4, 0x3d, 0x7b, 0x00, 0x40, 0x07, 0x00]);
    }

    #[test]
    fn endarc_volume_block() {
        let next = build_endarc(true, 0xDEAD_BEEF, 3);
        assert_eq!(next.len(), 20);
        assert_eq!(next[2], ENDARC_HEAD);
        assert_eq!(u16::from_le_bytes([next[3], next[4]]), 0x400f);
        assert_eq!(
            u16::from_le_bytes([next[5], next[6]]),
            ENDARC_VOLUME_HEADER_SIZE
        );
        assert_eq!(
            u32::from_le_bytes(next[7..11].try_into().unwrap()),
            0xDEAD_BEEF
        );
        assert_eq!(u16::from_le_bytes([next[11], next[12]]), 3);
        assert!(next[13..].iter().all(|&byte| byte == 0), "trailing zeros");
        assert_eq!(
            u16::from_le_bytes([next[0], next[1]]),
            header_crc16(&next[2..]),
            "HEAD_CRC covers the 20-byte body"
        );

        // The final volume drops EHFL_NEXTVOLUME (0x400e).
        let last = build_endarc(false, 0, 0);
        assert_eq!(u16::from_le_bytes([last[3], last[4]]), 0x400e);
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

    /// The extended-time area is a RAR 3.0+ (`v29`) construct: RAR 1.5/2.x
    /// readers size a plain file header as `32 + name` (no record), so a
    /// record there shifts their data offset and the header CRC no longer
    /// covers what they expect (UnRAR 2.90 reported "the file header is
    /// corrupt"). Only v29 members may carry one.
    #[test]
    fn member_ext_time_is_v29_only() {
        let odd = local_civil_to_epoch(1_700_000_001); // odd local second
        let even = local_civil_to_epoch(1_700_000_002);
        assert!(build_member_ext_time(29, odd, None).is_some());
        assert!(build_member_ext_time(29, even, Some(123_456_700)).is_some());
        assert!(build_member_ext_time(29, even, None).is_none());
        for unp_ver in [15, 20, 26] {
            assert!(
                build_member_ext_time(unp_ver, odd, Some(123_456_700)).is_none(),
                "unp_ver {unp_ver} must not carry a record"
            );
        }
    }

    /// WinRAR's archive-wide `FHD` window bits (verified against 6.23 `-ma4`).
    #[test]
    fn dict_bits_follow_winrar() {
        for (size, solid, bits) in [
            (0u64, false, 1),
            (60_000, false, 1),
            (200_000, false, 2),
            (500_000, false, 3),
            (2_000_000, false, 5),
            (8_000_000, false, 6),
            (60_000, true, 4),
            (500_000, true, 4),
            (2_000_000, true, 5),
            (8_000_000, true, 6),
        ] {
            assert_eq!(dict_bits(size, solid), bits, "size={size} solid={solid}");
        }
    }

    /// The RAR 2.x family declares the era's fixed 1 MiB dictionary (WinRAR
    /// 2.90 writes 4 for every archive), independent of the member size; the
    /// size-based rule would over-declare past the RAR 2.x ceiling.
    #[test]
    fn archive_dict_bits_is_fixed_for_pre_rar3() {
        for size in [0u64, 60_000, 2_000_000, 8_000_000] {
            assert_eq!(archive_dict_bits(20, size, false), 4, "size={size}");
            assert_eq!(archive_dict_bits(15, size, false), 4, "size={size}");
        }
        assert_eq!(archive_dict_bits(29, 60_000, false), 1);
        assert_eq!(archive_dict_bits(29, 8_000_000, false), 6);
    }

    /// Level 0 writes a RAR 2.x member in a v29 container, unless encrypted
    /// (the `-p` layout is the v29 one) or the container itself is older.
    #[test]
    fn member_unp_ver_turns_level0_into_20_only_for_v29() {
        assert_eq!(member_unp_ver(29, 0, false), 20);
        assert_eq!(member_unp_ver(29, 0, true), 29);
        assert_eq!(member_unp_ver(29, 3, false), 29);
        assert_eq!(member_unp_ver(15, 0, false), 15);
        assert_eq!(member_unp_ver(20, 0, false), 20);
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
