//! Write-side serialization of RAR5 block/header envelopes and the
//! extra-record / service-block builders.

#[cfg(unix)]
use crate::format::rar5::EXTRA_FILE_OWNER;
use crate::format::rar5::headers::{ArchiveHeader, EndOfArchiveHeader, FileHeader};
use crate::format::rar5::{
    ARCHIVE_FLAG_VOLUME, ARCHIVE_FLAG_VOLUME_NUM, BLOCK_FLAG_DATA_AREA, BLOCK_FLAG_EXTRA_DATA,
    BLOCK_FLAG_SKIP_IF_UNKNOWN, BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_TYPE_END_ARCHIVE,
    BLOCK_TYPE_FILE_HEADER, BLOCK_TYPE_SERVICE_HEADER, COMP_INFO_DICT_SHIFT,
    COMP_INFO_METHOD_SHIFT, COMP_INFO_SOLID_BIT, EXTRA_FILE_HASH, EXTRA_FILE_TIME, FILE_FLAG_CRC32,
    FILE_FLAG_DIRECTORY, FILE_FLAG_TIME_UNIX, OS_UNIX,
};
use crate::vint;

/// Frame a plaintext RAR5 block body as the on-disk envelope
/// `[CRC32 LE][header size vint][body]`, with the CRC taken over the stored
/// size vint bytes plus the body. The single writer of the RAR5 block
/// envelope: every header builder below frames through it, and the
/// surgical-rewrite paths call it for rebuilt headers.
pub(crate) fn frame_block(body: &[u8]) -> Vec<u8> {
    let size_bytes = vint::encode(body.len() as u64);
    let mut content = Vec::with_capacity(size_bytes.len() + body.len());
    content.extend(&size_bytes);
    content.extend(body);
    let crc = crc32fast::hash(&content);
    let mut out = Vec::with_capacity(4 + content.len());
    out.extend(crc.to_le_bytes());
    out.extend(content);
    out
}

/// Encode `value` as a vint occupying at least `min` bytes. WinRAR writes the
/// member/service header size fields (`data_size`, `unpacked_size`,
/// `comp_info`) with a two-byte minimum, so a small value carries a redundant
/// zero continuation byte (`11` renders as `8b 00`); larger values keep their
/// natural width. The other header vints (`attributes`, `host_os`, the name
/// length, the extra-area size) stay minimal. (Measured against WinRAR 7.23.)
pub(crate) fn vint_at_least(value: u64, min: usize) -> Vec<u8> {
    let mut out = vint::encode(value);
    while out.len() < min {
        if let Some(last) = out.last_mut() {
            *last |= 0x80;
        }
        out.push(0);
    }
    out
}

/// The reserved field width WinRAR uses for an "STM" record's `data_size` and
/// `unpacked_size`: it emits the stream header before the stream's packed size
/// is known and reserves room for `unpacked_size << 12`, never fewer than two
/// bytes, then patches the real values in. (Measured against WinRAR 7.23: a
/// 4-byte stream reserves three bytes, 512 bytes four, 64 KiB five.) The
/// same archive's file headers use the two-byte minimum, not this estimate.
fn stream_size_field_width(unpacked_size: u64) -> usize {
    vint::encoded_size(unpacked_size.saturating_mul(1 << 12)).max(2)
}

impl ArchiveHeader {
    /// Serialize to RAR5 binary format (including CRC).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_ARCHIVE_HEADER));

        // Block-level flags (not archive-level flags). WinRAR marks the main
        // header skippable-if-unknown (0x04) as well, for every archive shape.
        let mut block_flags = BLOCK_FLAG_SKIP_IF_UNKNOWN;
        if !self.extra_data.is_empty() {
            block_flags |= BLOCK_FLAG_EXTRA_DATA;
        }
        body.extend(vint::encode(block_flags));

        if !self.extra_data.is_empty() {
            body.extend(vint::encode(self.extra_data.len() as u64));
        }

        // Archive-level flags
        let mut arch_flags = self.flags & 0xFFFF;
        if self.volume_number.is_some() {
            arch_flags |= ARCHIVE_FLAG_VOLUME | ARCHIVE_FLAG_VOLUME_NUM;
        }
        body.extend(vint::encode(arch_flags));

        // Volume number follows arch_flags when VOLUME_NUM is set
        if let Some(vol_num) = self.volume_number {
            body.extend(vint::encode(vol_num));
        }

        body.extend(&self.extra_data);

        frame_block(&body)
    }
}

impl FileHeader {
    /// Serialize to RAR5 binary format (including CRC).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_FILE_HEADER));

        let mut eff_file_flags = self.file_flags;
        if self.is_directory {
            eff_file_flags |= FILE_FLAG_DIRECTORY;
        }
        if self.crc32_val.is_none() {
            eff_file_flags &= !FILE_FLAG_CRC32;
        }

        let mut eff_block_flags = self.flags;
        if !self.extra_data.is_empty() {
            eff_block_flags |= BLOCK_FLAG_EXTRA_DATA;
        }
        if self.packed_size > 0 && !self.is_directory {
            eff_block_flags |= BLOCK_FLAG_DATA_AREA;
        }

        body.extend(vint::encode(eff_block_flags));

        if !self.extra_data.is_empty() {
            body.extend(vint::encode(self.extra_data.len() as u64));
        }
        if eff_block_flags & BLOCK_FLAG_DATA_AREA != 0 {
            body.extend(vint_at_least(self.packed_size, 2));
        }

        body.extend(vint::encode(eff_file_flags));
        body.extend(vint_at_least(self.unpacked_size, 2));
        body.extend(vint::encode(self.attributes));

        if eff_file_flags & FILE_FLAG_TIME_UNIX != 0 {
            body.extend(self.mtime.to_le_bytes());
        }
        if eff_file_flags & FILE_FLAG_CRC32 != 0
            && let Some(crc) = self.crc32_val
        {
            body.extend(crc.to_le_bytes());
        }

        // Compression info
        let mut comp_info: u64 = (self.comp_version as u64) & 0x3F;
        if self.comp_solid {
            comp_info |= COMP_INFO_SOLID_BIT;
        }
        comp_info |= ((self.comp_method as u64) & 0x07) << COMP_INFO_METHOD_SHIFT;
        if let Some(bytes) = self.dict_size_bytes {
            // RAR7 (v70): 5-bit dict field (bits 10-14) + 1/32 increment
            // (bits 15-19) encode non-power-of-two sizes up to 126 GiB;
            // the compression version is forced to 1.
            let mut n = 0u32;
            while (0x20000u64 << (n + 1)) <= bytes && n < 19 {
                n += 1;
            }
            let base = 0x20000u64 << n;
            let inc = ((bytes - base) * 32 / base).min(31);
            comp_info |= 1;
            comp_info |= (n as u64 & 0x1F) << COMP_INFO_DICT_SHIFT;
            comp_info |= (inc & 0x1F) << 15;
        } else {
            comp_info |= ((self.comp_dict_size as u64) & 0x0F) << COMP_INFO_DICT_SHIFT;
        }
        body.extend(vint_at_least(comp_info, 2));
        body.extend(vint::encode(self.host_os));

        let name_bytes = self.name.as_bytes();
        body.extend(vint::encode(name_bytes.len() as u64));
        body.extend(name_bytes);

        body.extend(&self.extra_data);

        frame_block(&body)
    }
}

/// Serialize a BLAKE2sp hash extra record for file headers.
pub fn hash_extra_record(value: [u8; 32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(1 + 32);
    body.extend(vint::encode(0u64)); // hash type: BLAKE2sp
    body.extend_from_slice(&value);
    let type_bytes = vint::encode(EXTRA_FILE_HASH);
    let rec_size = type_bytes.len() + body.len();
    let mut out = Vec::with_capacity(rec_size + 1 + body.len());
    out.extend(vint::encode(rec_size as u64));
    out.extend(type_bytes);
    out.extend(body);
    out
}

impl EndOfArchiveHeader {
    /// Serialize to RAR5 binary format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_END_ARCHIVE));
        // Block-level flags. This is a real block-flags field: 7-Zip
        // parses it as such, so the endarc flags must NOT be placed here
        // (a next-volume flag of 1 reads as HFL_EXTRA, making 7-Zip
        // consume the endarc flags as an extra-area size and fail
        // multi-volume sets with "data after the end of archive").
        // WinRAR writes HFL_SKIP_IF_UNKNOWN (0x04) here.
        body.extend(vint::encode(BLOCK_FLAG_SKIP_IF_UNKNOWN));
        body.extend(vint::encode(self.flags));

        frame_block(&body)
    }
}

/// Serialize a file redirection (EXTRA_FILE_REDIRECT) extra record.
pub(crate) fn redirect_extra_bytes(redir_type: u64, target: &str) -> Vec<u8> {
    let mut record = Vec::new();
    record.extend(vint::encode(redir_type));
    record.extend(vint::encode(0u64)); // flags
    record.extend(vint::encode(target.len() as u64));
    record.extend_from_slice(target.as_bytes());
    let mut out = Vec::new();
    out.extend(vint::encode((1 + record.len()) as u64));
    out.extend(vint::encode(0x05u64)); // EXTRA_FILE_REDIRECT
    out.extend(record);
    out
}
/// Serialize a FILE_TIME (HTIME) extra record, matching the official `rar`
/// format: `[flags vint][per present time: sec u32][if ns: per present
/// time: ns u32]`. Flag bits: 0x01 unix format, 0x02 mtime, 0x04 ctime,
/// 0x08 atime, 0x10 nanosecond precision. All present times share one
/// precision, so all-zero ns selects the 1-second form.
pub(crate) fn file_time_extra_record(
    mtime: Option<(u64, u32)>,
    ctime: Option<(u64, u32)>,
    atime: Option<(u64, u32)>,
) -> Vec<u8> {
    let ns_precision = mtime.is_some_and(|(_, ns)| ns != 0)
        || ctime.is_some_and(|(_, ns)| ns != 0)
        || atime.is_some_and(|(_, ns)| ns != 0);
    let mut flags = 0x0001u64; // unix format
    if ns_precision {
        flags |= 0x0010;
    }
    if mtime.is_some() {
        flags |= 0x0002;
    }
    if ctime.is_some() {
        flags |= 0x0004;
    }
    if atime.is_some() {
        flags |= 0x0008;
    }
    let mut record = Vec::with_capacity(13);
    record.extend(vint::encode(flags));
    // Segment layout (like WinRAR): all second fields first, then all
    // nanosecond fields, in mtime/ctime/atime order.
    for (secs, _) in [mtime, ctime, atime].into_iter().flatten() {
        record.extend_from_slice(&(secs as u32).to_le_bytes());
    }
    if ns_precision {
        for (_, ns) in [mtime, ctime, atime].into_iter().flatten() {
            record.extend_from_slice(&ns.to_le_bytes());
        }
    }

    let mut out = Vec::with_capacity(12 + record.len());
    out.extend(vint::encode((1 + record.len()) as u64));
    out.extend(vint::encode(EXTRA_FILE_TIME));
    out.extend(record);
    out
}

/// The same FILE_TIME record in WinRAR's Windows form: the `unix format` and
/// `nanosecond precision` bits are clear and every present time is one 8-byte
/// Windows FILETIME (100 ns ticks since 1601), in mtime/ctime/atime order.
/// WinRAR on Windows writes this whenever it is not truncating times to whole
/// seconds (`-ts1`), and clears `FILE_FLAG_TIME_UNIX` on the header so the
/// 4-byte Unix mtime field is absent — the record is the only time carrier.
pub(crate) fn file_time_extra_record_windows(
    mtime: Option<(u64, u32)>,
    ctime: Option<(u64, u32)>,
    atime: Option<(u64, u32)>,
) -> Vec<u8> {
    let mut flags = 0u64;
    if mtime.is_some() {
        flags |= 0x0002;
    }
    if ctime.is_some() {
        flags |= 0x0004;
    }
    if atime.is_some() {
        flags |= 0x0008;
    }
    let mut record = Vec::with_capacity(1 + 24);
    record.extend(vint::encode(flags));
    for (secs, ns) in [mtime, ctime, atime].into_iter().flatten() {
        record.extend_from_slice(&unix_to_filetime(secs, ns).to_le_bytes());
    }

    let mut out = Vec::with_capacity(12 + record.len());
    out.extend(vint::encode((1 + record.len()) as u64));
    out.extend(vint::encode(EXTRA_FILE_TIME));
    out.extend(record);
    out
}

/// A Unix time (seconds + nanoseconds) as a Windows FILETIME: 100 ns ticks
/// since 1601-01-01, the epoch NTFS and WinRAR's FILE_TIME record use.
fn unix_to_filetime(secs: u64, ns: u32) -> u64 {
    /// Seconds from 1601-01-01 to 1970-01-01.
    const EPOCH_DELTA_SECS: u64 = 11_644_473_600;
    (secs + EPOCH_DELTA_SECS) * 10_000_000 + u64::from(ns / 100)
}

/// Serialize an OWNER extra record (`EXTRA_FILE_OWNER`) with owner and
/// group names: `[flags][owner len][owner][group len][group]`. Flag bits
/// 0x01 = owner present, 0x02 = group present (mirrors the parser).
#[cfg(unix)]
pub(crate) fn build_owner_extra_record(owner: &str, group: &str) -> Vec<u8> {
    let mut flags = 0u64;
    if !owner.is_empty() {
        flags |= 0x01;
    }
    if !group.is_empty() {
        flags |= 0x02;
    }
    let mut body = Vec::with_capacity(4 + owner.len() + group.len());
    body.extend(vint::encode(flags));
    if !owner.is_empty() {
        body.extend(vint::encode(owner.len() as u64));
        body.extend(owner.as_bytes());
    }
    if !group.is_empty() {
        body.extend(vint::encode(group.len() as u64));
        body.extend(group.as_bytes());
    }
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend(vint::encode((1 + body.len()) as u64));
    out.extend(vint::encode(EXTRA_FILE_OWNER));
    out.extend(body);
    out
}
/// Serialize a "CMT" archive comment service block (type 3, name "CMT",
/// comment bytes in the data area), matching the official `rar c` format.
///
/// Returns only the block header frame; the comment bytes are the block's
/// data area and must be written separately right after it. Splitting them
/// is what lets header encryption (`-hp`) wrap only the header — the data
/// area stays plaintext, like every other block.
pub(crate) fn build_comment_block(comment: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(vint::encode(BLOCK_TYPE_SERVICE_HEADER));
    body.extend(vint::encode(BLOCK_FLAG_DATA_AREA));
    body.extend(vint_at_least(comment.len() as u64, 2)); // data size
    body.extend(vint::encode(FILE_FLAG_CRC32));
    body.extend(vint_at_least(comment.len() as u64, 2)); // unpacked size
    body.extend(vint::encode(0u64)); // attributes
    body.extend(crc32fast::hash(comment).to_le_bytes());
    body.extend(vint_at_least(0, 2)); // compression info (store)
    body.extend(vint::encode(OS_UNIX));
    body.extend(vint::encode(3u64)); // name length
    body.extend(b"CMT");

    frame_block(&body)
}
/// Serialize a "QO"/"RR"/"STM"-style service block: type 3, the given
/// name, an extra area holding the service-data record (`subdata`),
/// `data_size` bytes of payload following the header, plus extra block
/// flags (`BLOCK_FLAG_SKIP_IF_UNKNOWN` for "QO"/"RR",
/// `BLOCK_FLAG_DEPENDS_PREV` for "STM" stream records).
pub(crate) fn build_service_block(
    name: &str,
    subdata: &[u8],
    data_size: u64,
    extra_flags: u64,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(vint::encode(BLOCK_TYPE_SERVICE_HEADER));
    body.extend(vint::encode(
        BLOCK_FLAG_EXTRA_DATA | BLOCK_FLAG_DATA_AREA | extra_flags,
    ));
    body.extend(vint::encode(subdata.len() as u64)); // extra area size
    body.extend(vint_at_least(data_size, 2)); // data size
    body.extend(vint::encode(0u64)); // file flags
    body.extend(vint_at_least(data_size, 2)); // unpacked size
    body.extend(vint::encode(0u64)); // attributes
    body.extend(vint_at_least(0, 2)); // compression info (store)
    body.extend(vint::encode(OS_UNIX));
    body.extend(vint::encode(name.len() as u64));
    body.extend(name.as_bytes());
    body.extend(subdata);

    frame_block(&body)
}
/// Serialize an "STM" NTFS-stream service block: the same envelope as
/// [`build_service_block`], but carrying the plaintext CRC32 over the
/// decoded stream bytes, the stream's compression info, and the full
/// extra area (optional encryption record plus the SUBDATA stream name).
pub(crate) fn build_stream_block(
    packed_size: u64,
    unpacked_size: u64,
    stream_crc32: u32,
    method: u8,
    dict_log: u8,
    extra: &[u8],
) -> Vec<u8> {
    use crate::format::rar5::{BLOCK_FLAG_DEPENDS_PREV, OS_WINDOWS};

    let mut body = Vec::new();
    body.extend(vint::encode(BLOCK_TYPE_SERVICE_HEADER));
    body.extend(vint::encode(
        BLOCK_FLAG_EXTRA_DATA | BLOCK_FLAG_DATA_AREA | BLOCK_FLAG_DEPENDS_PREV,
    ));
    body.extend(vint::encode(extra.len() as u64)); // extra area size
    let size_width = stream_size_field_width(unpacked_size);
    body.extend(vint_at_least(packed_size, size_width)); // data size
    body.extend(vint::encode(FILE_FLAG_CRC32)); // file flags
    body.extend(vint_at_least(unpacked_size, size_width));
    body.extend(vint::encode(0u64)); // attributes
    body.extend(stream_crc32.to_le_bytes());
    body.extend(vint_at_least(
        (u64::from(dict_log) << COMP_INFO_DICT_SHIFT)
            | (u64::from(method) << COMP_INFO_METHOD_SHIFT),
        2,
    ));
    body.extend(vint::encode(OS_WINDOWS));
    body.extend(vint::encode(3u64)); // name length
    body.extend(b"STM");
    body.extend_from_slice(extra);

    frame_block(&body)
}

/// Encode `value` as a fixed `width`-byte RAR5 vint (LSB-first, continuation
/// bit on every byte except the last). WinRAR preallocates the locator offset
/// fields this way (see [`crate::format::rar5::headers::locator`]); the caller
/// must ensure `value` fits in `7 * width` bits, since a wrapped value would
/// name arbitrary bytes.
pub(crate) fn vint_fixed(value: u64, width: usize) -> Vec<u8> {
    let mut out = vec![0x80u8; width];
    let mut v = value;
    for (i, byte) in out.iter_mut().enumerate() {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if i + 1 < width {
            b |= 0x80;
        }
        *byte = b;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rar7_max_dictionary_roundtrips_exactly() {
        let header = FileHeader {
            name: "max-dict.bin".into(),
            comp_version: 1,
            dict_size_bytes: Some(crate::options::MAX_RAR7_DICTIONARY_BYTES),
            ..Default::default()
        };

        let raw = crate::format::rar5::headers::parse_block_bytes(&header.to_bytes()).unwrap();
        let parsed = FileHeader::from_raw(&raw, raw.data_offset).unwrap();
        assert_eq!(
            parsed.dict_size_bytes,
            Some(crate::options::MAX_RAR7_DICTIONARY_BYTES)
        );
    }

    #[test]
    fn frame_block_emits_the_read_side_envelope() {
        let body = [1u8, 2, 3, 4, 5];
        let framed = frame_block(&body);

        let raw = crate::format::rar5::headers::parse_block_bytes(&framed).unwrap();
        assert_eq!(raw.header_data, body, "the body must round-trip verbatim");
        assert_eq!(raw.data_offset, framed.len() as u64);

        // The CRC covers the stored size vint plus the body, and nothing
        // else (no trailing data area in this call).
        let content = &framed[4..];
        assert_eq!(
            u32::from_le_bytes(framed[..4].try_into().unwrap()),
            crc32fast::hash(content)
        );
        assert_eq!(content[0] as usize, body.len());
    }

    /// Decode one vint and advance the cursor.
    fn next(body: &[u8], off: &mut usize) -> (u64, usize) {
        let (v, n) = vint::decode_from_slice(body, *off).unwrap();
        *off += n;
        (v, n)
    }

    #[test]
    fn vint_at_least_pads_small_values() {
        assert_eq!(vint_at_least(0, 2), vec![0x80, 0x00]);
        assert_eq!(vint_at_least(11, 2), vec![0x8b, 0x00]);
        assert_eq!(vint_at_least(127, 2), vec![0xff, 0x00]);
        assert_eq!(vint_at_least(128, 2), vec![0x80, 0x01]);
        // Already two bytes or wider: the natural width is kept.
        assert_eq!(vint_at_least(128, 2), vint::encode(128));
        assert_eq!(vint_at_least(16384, 2), vint::encode(16384));
        for v in [0u64, 1, 11, 127, 128, 16383, 16384, u32::MAX as u64] {
            let encoded = vint_at_least(v, 2);
            let (decoded, n) = vint::decode_from_slice(&encoded, 0).unwrap();
            assert_eq!(decoded, v);
            assert!(n >= 2, "value {v} must occupy at least two bytes");
        }
    }

    #[test]
    fn stream_size_field_width_matches_winrar() {
        assert_eq!(stream_size_field_width(0), 2);
        assert_eq!(stream_size_field_width(1), 2);
        assert_eq!(stream_size_field_width(3), 2);
        assert_eq!(stream_size_field_width(4), 3);
        assert_eq!(stream_size_field_width(511), 3);
        assert_eq!(stream_size_field_width(512), 4);
        assert_eq!(stream_size_field_width(65535), 4);
        assert_eq!(stream_size_field_width(65536), 5);
        assert_eq!(stream_size_field_width(1 << 23), 6);
        // Saturates rather than wrapping for absurd sizes.
        assert_eq!(stream_size_field_width(u64::MAX), 10);
    }

    #[test]
    fn member_size_fields_use_a_two_byte_minimum() {
        let header = FileHeader {
            name: "a.txt".into(),
            unpacked_size: 11,
            packed_size: 11,
            crc32_val: Some(0x1234_5678),
            ..Default::default()
        };
        let raw = crate::format::rar5::headers::parse_block_bytes(&header.to_bytes()).unwrap();
        let body = &raw.header_data;
        let mut off = 0usize;
        let (_ty, _) = next(body, &mut off);
        let (flags, _) = next(body, &mut off);
        assert_eq!(flags & crate::format::rar5::BLOCK_FLAG_EXTRA_DATA, 0);
        let (ds, ds_n) = next(body, &mut off);
        let (ff, _) = next(body, &mut off);
        let (us, us_n) = next(body, &mut off);
        let (_at, _) = next(body, &mut off);
        if ff & crate::format::rar5::FILE_FLAG_TIME_UNIX != 0 {
            off += 4;
        }
        if ff & crate::format::rar5::FILE_FLAG_CRC32 != 0 {
            off += 4;
        }
        let (ci, ci_n) = next(body, &mut off);
        assert_eq!((ds, ds_n), (11, 2), "data_size must occupy two bytes");
        assert_eq!((us, us_n), (11, 2), "unpacked_size must occupy two bytes");
        assert_eq!((ci, ci_n), (0, 2), "comp_info must occupy two bytes");
        // ...and the padded header still parses back to the same values.
        let parsed = FileHeader::from_raw(&raw, raw.data_offset).unwrap();
        assert_eq!(parsed.packed_size, 11);
        assert_eq!(parsed.unpacked_size, 11);
        assert_eq!(parsed.crc32_val, Some(0x1234_5678));
    }

    #[test]
    fn service_block_size_fields_use_a_two_byte_minimum() {
        let framed = build_service_block("QO", &[1, 0x07], 5, 0);
        let raw = crate::format::rar5::headers::parse_block_bytes(&framed).unwrap();
        let body = &raw.header_data;
        let mut off = 0usize;
        let (_ty, _) = next(body, &mut off);
        let (_flags, _) = next(body, &mut off);
        let (_esz, _) = next(body, &mut off); // extra area size (present)
        let (ds, ds_n) = next(body, &mut off);
        let (_ff, _) = next(body, &mut off);
        let (us, us_n) = next(body, &mut off);
        let (_at, _) = next(body, &mut off);
        let (ci, ci_n) = next(body, &mut off);
        assert_eq!((ds, ds_n), (5, 2));
        assert_eq!((us, us_n), (5, 2));
        assert_eq!((ci, ci_n), (0, 2));
    }

    #[test]
    fn stream_block_size_fields_use_the_reserved_width() {
        let framed = build_stream_block(4, 4, 0, 0, 0, &[]);
        let raw = crate::format::rar5::headers::parse_block_bytes(&framed).unwrap();
        let body = &raw.header_data;
        let mut off = 0usize;
        let (_ty, _) = next(body, &mut off);
        let (_flags, _) = next(body, &mut off);
        let (_esz, _) = next(body, &mut off); // extra area size
        let (ds, ds_n) = next(body, &mut off);
        let (ff, _) = next(body, &mut off);
        let (us, us_n) = next(body, &mut off);
        let (_at, _) = next(body, &mut off);
        if ff & crate::format::rar5::FILE_FLAG_CRC32 != 0 {
            off += 4;
        }
        let (ci, ci_n) = next(body, &mut off);
        assert_eq!((ds, ds_n), (4, 3), "a 4-byte stream reserves three bytes");
        assert_eq!((us, us_n), (4, 3));
        assert_eq!((ci, ci_n), (0, 2));
    }
}
