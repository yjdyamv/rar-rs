//! RAR 1.3/1.4 container read and write: signature `RE~^`, fixed
//! little-endian headers without a header CRC, 16-bit rolling member
//! checksums and the RAR13 additive stream cipher.
//!
//! The member payload uses the same `Unpack15` codec as the RAR 1.5
//! family; the read/write sides are ported from `rars`' `rar13.rs`
//! (MIT OR Apache-2.0; see NOTICE).

pub(crate) mod create;
pub(crate) mod extract;
pub(crate) mod write;

use std::io::{Read, Seek, SeekFrom};

use crate::detect::RAR13_SIGNATURE;
use crate::engine::ArchiveEntry;
use crate::error::{RarError, RarResult};
use crate::format::shared::extract::{MAX_CATALOG_ENTRIES, check_entry_cap};
use crate::model::{DataChunk, FileHeader};

/// Main header flag: always set (the reference writer stamps `0x80`).
pub(crate) const MHD_ALWAYS_SET: u8 = 0x80;
/// Main header flag: the archive is part of a multi-volume set (every
/// volume's main header carries it).
pub(crate) const MHD_VOLUME: u8 = 0x01;
/// Main header flag: the main-header extension holds an archive comment.
pub(crate) const MHD_COMMENT: u8 = 0x02;
/// Main header flag: solid archive (`Unpack15` window shared by members).
pub(crate) const MHD_SOLID: u8 = 0x08;
/// Main header flag: the archive comment is compressed.
pub(crate) const MHD_PACK_COMMENT: u8 = 0x10;

/// File header flag: the fragment continues a member from the previous
/// volume.
pub(crate) const LHD_SPLIT_BEFORE: u8 = 0x01;
/// File header flag: the member continues in the next volume.
pub(crate) const LHD_SPLIT_AFTER: u8 = 0x02;
/// File header flag: the payload is RAR13-cipher encrypted.
pub(crate) const LHD_PASSWORD: u8 = 0x04;
/// File header flag: the header extension holds a member comment.
pub(crate) const LHD_COMMENT: u8 = 0x08;
/// File header flag: the member continues a solid chain (reference writers
/// stamp it; readers chain by `MHD_SOLID` + position).
pub(crate) const LHD_SOLID: u8 = 0x10;
pub(crate) const METHOD_STORE: u8 = 0;
pub(crate) const METHOD_BEST: u8 = 5;
pub(crate) const DEFAULT_UNP_VER: u8 = 2;

pub(crate) const MAIN_HEAD_SIZE: usize = 7;
pub(crate) const FILE_HEAD_BASE_SIZE: usize = 21;

/// One parsed volume: main-header flags/extension plus its entries.
pub(crate) struct Volume {
    pub flags: u8,
    pub extra: Vec<u8>,
    pub entries: Vec<ArchiveEntry>,
}

/// Decode a DOS-era member name: valid UTF-8 passes through, otherwise the
/// Windows system ANSI code page (e.g. CP936) is tried before the lossy
/// fallback — the same policy as RAR4's `decode_file_name`.
fn decode_name(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => crate::format::decode_system_ansi(bytes)
            .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// Parse one RAR 1.3/1.4 volume starting at `offset` (an SFX stub may
/// precede the first volume's signature).
pub(crate) fn parse_volume(
    stream: &mut (impl Read + Seek),
    offset: u64,
    file_len: u64,
) -> RarResult<Volume> {
    stream.seek(SeekFrom::Start(offset))?;
    let mut main = [0u8; MAIN_HEAD_SIZE];
    stream.read_exact(&mut main).map_err(RarError::Io)?;
    if &main[..RAR13_SIGNATURE.len()] != RAR13_SIGNATURE {
        return Err(RarError::format("RAR 1.3: volume signature mismatch"));
    }
    let head_size = usize::from(u16::from_le_bytes([main[4], main[5]]));
    let flags = main[6];
    if head_size < MAIN_HEAD_SIZE || offset + head_size as u64 > file_len {
        return Err(RarError::format("RAR 1.3: main header is truncated"));
    }
    let mut extra = vec![0u8; head_size - MAIN_HEAD_SIZE];
    stream.read_exact(&mut extra).map_err(RarError::Io)?;

    let mut entries = Vec::new();
    let mut pos = offset + head_size as u64;
    while pos + FILE_HEAD_BASE_SIZE as u64 <= file_len {
        stream.seek(SeekFrom::Start(pos))?;
        let mut base = [0u8; FILE_HEAD_BASE_SIZE];
        stream.read_exact(&mut base).map_err(RarError::Io)?;

        let pack_size = u32::from_le_bytes(base[0..4].try_into().expect("fixed slice"));
        let unp_size = u32::from_le_bytes(base[4..8].try_into().expect("fixed slice"));
        let file_crc = u16::from_le_bytes([base[8], base[9]]);
        let head_size = usize::from(u16::from_le_bytes([base[10], base[11]]));
        let file_time = u32::from_le_bytes(base[12..16].try_into().expect("fixed slice"));
        let file_attr = base[16];
        let lhd_flags = base[17];
        let name_size = usize::from(base[19]);
        let method = base[20];

        let minimum_size = FILE_HEAD_BASE_SIZE + name_size;
        if head_size < minimum_size || pos + head_size as u64 > file_len {
            return Err(RarError::format(
                "RAR 1.3: file header is truncated or shorter than its name",
            ));
        }
        let mut tail = vec![0u8; head_size - FILE_HEAD_BASE_SIZE];
        stream.read_exact(&mut tail).map_err(RarError::Io)?;
        let name_bytes = &tail[..name_size];
        let extra = &tail[name_size..];

        let data_start = pos + head_size as u64;
        let data_end = data_start
            .checked_add(u64::from(pack_size))
            .ok_or_else(|| RarError::format("RAR 1.3: member data size overflows"))?;
        if data_end > file_len {
            return Err(RarError::format(
                "RAR 1.3: member data extends past the volume",
            ));
        }
        pos = data_end;

        let is_directory = file_attr & 0x10 != 0;
        let comment = if lhd_flags & LHD_COMMENT != 0 {
            parse_file_comment(extra)
        } else {
            None
        };
        let header = FileHeader {
            name: decode_name(name_bytes),
            unpacked_size: u64::from(unp_size),
            packed_size: u64::from(pack_size),
            attributes: u64::from(file_attr),
            mtime: crate::format::shared::legacy_time::dos_time_to_unix(file_time),
            crc32_val: Some(u32::from(file_crc)),
            comp_method: method.wrapping_sub(METHOD_STORE),
            comp_solid: lhd_flags & LHD_SOLID != 0,
            flags: u64::from(lhd_flags),
            is_directory,
            data_offset: data_start,
            format_version: 3,
            unp_ver: 15,
            comment,
            ..Default::default()
        };
        entries.push(ArchiveEntry {
            header,
            chunks: vec![DataChunk {
                volume_index: 0,
                data_offset: data_start,
                packed_size: u64::from(pack_size),
                crc32_val: None,
                is_final: lhd_flags & LHD_SPLIT_AFTER == 0,
                extra_data: Vec::new(),
            }],
        });
        // A crafted file of minimum-size headers could otherwise expand
        // without bound (RAR4 and RAR5 apply the same ceiling).
        check_entry_cap(entries.len(), MAX_CATALOG_ENTRIES)?;
    }

    Ok(Volume {
        flags,
        extra,
        entries,
    })
}

/// Parse the `LHD_COMMENT` header extension: a 16-bit length followed by
/// the raw comment bytes.
fn parse_file_comment(extra: &[u8]) -> Option<Vec<u8>> {
    let length = usize::from(u16::from_le_bytes(extra.get(0..2)?.try_into().ok()?));
    let end = 2usize.checked_add(length)?;
    extra.get(2..end).map(<[u8]>::to_vec)
}

/// Decode the archive-level comment from the main-header extension.
///
/// A packed comment is RAR13-cipher encrypted (fixed comment key) and then
/// `Unpack15`-compressed; a plain comment is stored verbatim.
fn archive_comment(flags: u8, extra: &[u8]) -> RarResult<Option<Vec<u8>>> {
    if flags & MHD_COMMENT == 0 {
        return Ok(None);
    }
    let length = usize::from(u16::from_le_bytes(
        extra
            .get(0..2)
            .ok_or_else(|| RarError::format("RAR 1.3: comment size is missing"))?
            .try_into()
            .expect("fixed slice"),
    ));
    if flags & MHD_PACK_COMMENT != 0 {
        if length < 2 {
            return Err(RarError::format(
                "RAR 1.3: packed comment is shorter than its size field",
            ));
        }
        let unpacked_len = usize::from(u16::from_le_bytes(
            extra
                .get(2..4)
                .ok_or_else(|| RarError::format("RAR 1.3: packed comment is truncated"))?
                .try_into()
                .expect("fixed slice"),
        ));
        let packed_len = length - 2;
        let packed_end = 4usize
            .checked_add(packed_len)
            .ok_or_else(|| RarError::format("RAR 1.3: comment size overflows"))?;
        let packed = extra
            .get(4..packed_end)
            .ok_or_else(|| RarError::format("RAR 1.3: packed comment is truncated"))?;
        let mut packed = packed.to_vec();
        crate::crypto::Rar13Cipher::new_comment().decrypt_in_place(&mut packed);
        let decoded = crate::codec::legacy::rar15::Rar15Decoder::new()
            .decode_member(&packed, unpacked_len as u64, false)
            .map_err(|error| RarError::format(format!("RAR 1.3 packed comment: {error:?}")))?;
        return Ok(Some(decoded));
    }
    let end = 2usize
        .checked_add(length)
        .ok_or_else(|| RarError::format("RAR 1.3: comment size overflows"))?;
    Ok(Some(
        extra
            .get(2..end)
            .ok_or_else(|| RarError::format("RAR 1.3: comment is truncated"))?
            .to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::decode_name;

    /// Valid UTF-8 names stay byte-identical through the decoder (the ANSI
    /// or lossy fallbacks must not engage).
    #[test]
    fn member_name_decoding_preserves_valid_utf8() {
        let name = "目录/中文文件.txt";
        assert_eq!(decode_name(name.as_bytes()), name);
    }
}
