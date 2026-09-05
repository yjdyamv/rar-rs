//! Read-side parsing of RAR5 block/header envelopes and fields.

use std::io::{self, Read, Seek};

use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{
    ArchiveHeader, BlockMeta, EndOfArchiveHeader, FileHeader, RawBlock, RawHeader, RedirectSpec,
};
use crate::format::rar5::vint;
use crate::format::rar5::{
    ARCHIVE_FLAG_VOLUME_NUM, BLOCK_FLAG_DATA_AREA, BLOCK_FLAG_EXTRA_DATA, COMP_INFO_DICT_MASK,
    COMP_INFO_DICT_SHIFT, COMP_INFO_METHOD_MASK, COMP_INFO_METHOD_SHIFT, COMP_INFO_SOLID_BIT,
    COMP_INFO_VERSION_MASK, EXTRA_FILE_HASH, EXTRA_FILE_OWNER, EXTRA_FILE_REDIRECT,
    EXTRA_FILE_TIME, EXTRA_FILE_VERSION, EXTRA_SERVICE_SUBDATA, FILE_FLAG_CRC32,
    FILE_FLAG_DIRECTORY, FILE_FLAG_TIME_UNIX,
};
#[cfg(test)]
use crate::format::rar5::{BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_TYPE_FILE_HEADER};

pub fn read_block<R: Read + Seek>(
    reader: &mut R,
    key: Option<&[u8; 32]>,
) -> RarResult<Option<BlockMeta>> {
    let block_start = reader.stream_position()?;
    let header = match key {
        Some(key) => match read_encrypted_header(reader, key)? {
            Some(h) => h,
            None => return Ok(None),
        },
        None => match read_plain_header(reader)? {
            Some(h) => h,
            None => return Ok(None),
        },
    };

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&header.vint_bytes);
    hasher.update(&header.body);
    let computed = hasher.finalize();
    if computed != header.stored_crc {
        return Err(RarError::Crc {
            expected: header.stored_crc,
            actual: computed,
            context: "block header".into(),
        });
    }

    let (block_type, flags, data_size) = parse_block_fields(&header.body)?;
    let data_offset = reader.stream_position()?;
    let data_end = data_offset
        .checked_add(data_size)
        .ok_or_else(|| RarError::Format("block data offset overflows u64".into()))?;
    let raw = RawBlock {
        header_crc: header.stored_crc,
        header_data: header.body,
        data_size,
        data_offset,
        block_type,
        flags,
    };
    Ok(Some(BlockMeta {
        block_type,
        flags,
        block_start,
        data_offset,
        data_end,
        header_bytes: header.on_disk,
        hsize_vint_len: header.vint_bytes.len(),
        raw,
    }))
}

fn read_plain_header<R: Read>(reader: &mut R) -> RarResult<Option<RawHeader>> {
    let mut crc_buf = [0u8; 4];
    match reader.read_exact(&mut crc_buf[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut crc_buf[1..]).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            RarError::Format("truncated plaintext header CRC".into())
        } else {
            e.into()
        }
    })?;
    let stored_crc = u32::from_le_bytes(crc_buf);

    let mut vint_bytes = Vec::with_capacity(2);
    let hsize = loop {
        let mut b = [0u8; 1];
        reader.read_exact(&mut b).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                RarError::Format("truncated plaintext header size vint".into())
            } else {
                e.into()
            }
        })?;
        vint_bytes.push(b[0]);
        if b[0] & 0x80 == 0 {
            break vint::decode_from_slice(&vint_bytes, 0)
                .map_err(|e| RarError::Format(format!("bad vint: {e}")))?
                .0;
        }
        if vint_bytes.len() == 10 {
            return Err(RarError::Format(
                "plaintext header size vint exceeds 10 bytes".into(),
            ));
        }
    };
    if hsize == 0 || hsize > 2 * 1024 * 1024 {
        return Err(RarError::Format(format!(
            "implausible header size: {hsize}"
        )));
    }

    let hsize = usize::try_from(hsize)
        .map_err(|_| RarError::Format("header size overflows host address space".into()))?;
    let mut body = vec![0u8; hsize];
    reader.read_exact(&mut body).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            RarError::Format("truncated plaintext header body".into())
        } else {
            e.into()
        }
    })?;

    let capacity = 4usize
        .checked_add(vint_bytes.len())
        .and_then(|n| n.checked_add(body.len()))
        .ok_or_else(|| RarError::Format("plaintext header size overflows usize".into()))?;
    let mut on_disk = Vec::with_capacity(capacity);
    on_disk.extend_from_slice(&crc_buf);
    on_disk.extend_from_slice(&vint_bytes);
    on_disk.extend_from_slice(&body);
    Ok(Some(RawHeader {
        stored_crc,
        vint_bytes,
        body,
        on_disk,
    }))
}

fn read_encrypted_header<R: Read>(reader: &mut R, key: &[u8; 32]) -> RarResult<Option<RawHeader>> {
    let mut iv = [0u8; crate::format::rar5::ENCR_IV_SIZE];
    match reader.read_exact(&mut iv[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut iv[1..]).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            RarError::Format("truncated encrypted header IV".into())
        } else {
            e.into()
        }
    })?;

    let mut first_ct = [0u8; 16];
    reader.read_exact(&mut first_ct)?;
    let first_pt = crate::crypto::decrypt_data(&first_ct, key, &iv)?;
    let (stored_crc, vint_len, hsize) = {
        let stored_crc = u32::from_le_bytes(first_pt[..4].try_into().unwrap());
        let (hsize, vint_len) = vint::decode_from_slice(&first_pt, 4)
            .map_err(|e| RarError::Format(format!("encrypted block vint: {e}")))?;
        (stored_crc, vint_len, hsize)
    };
    if hsize == 0 || hsize > 2 * 1024 * 1024 {
        return Err(RarError::Format(format!(
            "implausible encrypted header size: {hsize}"
        )));
    }

    // Total raw bytes = CRC(4) + vint + body, padded to 16 bytes.
    let hsize = usize::try_from(hsize).map_err(|_| {
        RarError::Format("encrypted header size overflows host address space".into())
    })?;
    let total_raw = 4usize
        .checked_add(vint_len)
        .and_then(|n| n.checked_add(hsize))
        .ok_or_else(|| RarError::Format("encrypted header size overflows usize".into()))?;
    let enc_size = total_raw
        .checked_add(15)
        .map(|n| n / 16 * 16)
        .ok_or_else(|| RarError::Format("encrypted header padding overflows usize".into()))?;
    let mut full_ct = vec![0u8; enc_size];
    full_ct[..16].copy_from_slice(&first_ct);
    if enc_size > 16 {
        reader.read_exact(&mut full_ct[16..])?;
    }
    let full_pt = crate::crypto::decrypt_data(&full_ct, key, &iv)?;

    let body_start = 4usize
        .checked_add(vint_len)
        .ok_or_else(|| RarError::Format("encrypted header offset overflows usize".into()))?;
    if total_raw > full_pt.len() || body_start > total_raw {
        return Err(RarError::Format(
            "truncated encrypted header plaintext".into(),
        ));
    }
    let vint_bytes = full_pt[4..body_start].to_vec();
    let body = full_pt[body_start..total_raw].to_vec();
    let on_disk_capacity = 16usize
        .checked_add(enc_size)
        .ok_or_else(|| RarError::Format("encrypted header size overflows usize".into()))?;
    let mut on_disk = Vec::with_capacity(on_disk_capacity);
    on_disk.extend_from_slice(&iv);
    on_disk.extend_from_slice(&full_ct);
    Ok(Some(RawHeader {
        stored_crc,
        vint_bytes,
        body,
        on_disk,
    }))
}

/// Parse block type, flags and data size out of a plaintext header body
/// (the fields after the header size vint).
fn parse_block_fields(body: &[u8]) -> RarResult<(u64, u64, u64)> {
    let mut offset = 0usize;
    let (block_type, n) = vint::decode_from_slice(body, offset)
        .map_err(|e| RarError::Format(format!("block type: {e}")))?;
    offset += n;
    let (flags, n) = vint::decode_from_slice(body, offset)
        .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
    offset += n;
    if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        let (_, n) = vint::decode_from_slice(body, offset)
            .map_err(|e| RarError::Format(format!("extra size: {e}")))?;
        offset += n;
    }
    let mut data_size = 0u64;
    if flags & BLOCK_FLAG_DATA_AREA != 0 {
        let (v, n) = vint::decode_from_slice(body, offset)
            .map_err(|e| RarError::Format(format!("data size: {e}")))?;
        data_size = v;
        offset += n;
    }
    let _ = offset;
    Ok((block_type, flags, data_size))
}

impl ArchiveHeader {
    /// Parse from a [`RawBlock`].
    pub fn from_raw(raw: &RawBlock) -> RarResult<Self> {
        let data = &raw.header_data;
        let mut offset = 0;

        let (_, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let (block_flags, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;

        let mut extra_size = 0u64;
        if block_flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (v, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(e.to_string()))?;
            extra_size = v;
            offset += n;
        }

        let (arch_flags, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;

        // Volume number follows arch_flags when ARCHIVE_FLAG_VOLUME_NUM is set
        let volume_number = if arch_flags & ARCHIVE_FLAG_VOLUME_NUM != 0 {
            let (v, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(e.to_string()))?;
            offset += n;
            Some(v)
        } else {
            None
        };

        let extra_data = if extra_size > 0 {
            let extra_size = usize::try_from(extra_size).map_err(|_| {
                RarError::Format("archive extra size overflows host address space".into())
            })?;
            let end = offset
                .checked_add(extra_size)
                .ok_or_else(|| RarError::Format("archive extra size overflows usize".into()))?;
            if end > data.len() {
                return Err(RarError::Format("truncated archive extra area".into()));
            }
            data[offset..end].to_vec()
        } else {
            Vec::new()
        };

        Ok(ArchiveHeader {
            flags: arch_flags,
            extra_data,
            volume_number,
        })
    }
}

impl FileHeader {
    /// Parse from a [`RawBlock`] with the given stream position.
    pub fn from_raw(raw: &RawBlock, stream_pos: u64) -> RarResult<Self> {
        let data = &raw.header_data;
        let mut offset = 0;

        let (_, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let (block_flags, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;

        let mut extra_size = 0u64;
        let mut data_size = 0u64;
        if block_flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (v, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(e.to_string()))?;
            extra_size = v;
            offset += n;
        }
        if block_flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (v, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(e.to_string()))?;
            data_size = v;
            offset += n;
        }

        let (file_flags, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let (unpacked_size, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let (attributes, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;

        let mut mtime = 0u32;
        if file_flags & FILE_FLAG_TIME_UNIX != 0 {
            let end = offset
                .checked_add(4)
                .ok_or_else(|| RarError::Format("mtime offset overflows usize".into()))?;
            if end > data.len() {
                return Err(RarError::Format("truncated mtime".into()));
            }
            mtime = u32::from_le_bytes(data[offset..end].try_into().unwrap());
            offset = end;
        }

        let mut crc32_val = None;
        if file_flags & FILE_FLAG_CRC32 != 0 {
            let end = offset
                .checked_add(4)
                .ok_or_else(|| RarError::Format("CRC32 offset overflows usize".into()))?;
            if end > data.len() {
                return Err(RarError::Format("truncated CRC32".into()));
            }
            crc32_val = Some(u32::from_le_bytes(data[offset..end].try_into().unwrap()));
            offset = end;
        }

        let (comp_info, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let comp_version = (comp_info & COMP_INFO_VERSION_MASK) as u8;
        let comp_solid = comp_info & COMP_INFO_SOLID_BIT != 0;
        let comp_method = ((comp_info & COMP_INFO_METHOD_MASK) >> COMP_INFO_METHOD_SHIFT) as u8;
        let comp_dict_size = ((comp_info & COMP_INFO_DICT_MASK) >> COMP_INFO_DICT_SHIFT) as u8;
        // RAR7 (compression version 1): dictionary = 128 KiB << (5-bit
        // field at bits 10-14), plus a 1/32 increment from bits 15-19
        // (up to 126 GiB, non-power-of-two allowed).
        let dict_size_bytes = if comp_version == 1 {
            let base = 0x20000u64 << ((comp_info >> 10) & 0x1F);
            let inc = (comp_info >> 15) & 0x1F;
            Some(base + base / 32 * inc)
        } else {
            None
        };

        let (host_os, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let (name_len, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;

        let name_len = usize::try_from(name_len).map_err(|_| {
            RarError::Format("file name length overflows host address space".into())
        })?;
        let name_end = offset
            .checked_add(name_len)
            .ok_or_else(|| RarError::Format("file name length overflows usize".into()))?;
        if name_end > data.len() {
            return Err(RarError::Format("truncated file name".into()));
        }
        let name = String::from_utf8_lossy(&data[offset..name_end]).into_owned();
        offset = name_end;

        let extra_data = if extra_size > 0 {
            let extra_size = usize::try_from(extra_size).map_err(|_| {
                RarError::Format("file extra size overflows host address space".into())
            })?;
            let end = offset
                .checked_add(extra_size)
                .ok_or_else(|| RarError::Format("file extra size overflows usize".into()))?;
            if end > data.len() {
                return Err(RarError::Format("truncated file extra area".into()));
            }
            data[offset..end].to_vec()
        } else {
            Vec::new()
        };

        let is_directory = file_flags & FILE_FLAG_DIRECTORY != 0;
        let (hash_type, hash_value) = parse_hash_record(&extra_data);
        let (mtime_override, mtime_ns, ctime, atime, owner, group, version) =
            parse_extra_records(&extra_data);

        Ok(FileHeader {
            name,
            unpacked_size,
            packed_size: data_size,
            attributes,
            mtime: mtime_override.map(|s| s as u32).unwrap_or(mtime),
            crc32_val,
            hash_type,
            hash_value,
            comp_method,
            comp_version,
            comp_solid,
            comp_dict_size,
            host_os,
            flags: block_flags,
            file_flags,
            extra_data,
            is_directory,
            data_offset: stream_pos,
            format_version: 5,
            dict_size_bytes,
            mtime_ns,
            ctime,
            atime,
            owner,
            group,
            version,
            unp_ver: 0,
            salt: None,
            legacy_head_crc: None,
        })
    }
}

/// Parse the extra-area records that carry member metadata: nanosecond
/// modification time (`EXTRA_FILE_TIME`), owner/group names
/// (`EXTRA_FILE_OWNER`) and the file version (`EXTRA_FILE_VERSION`).
#[allow(clippy::type_complexity)]
fn parse_extra_records(
    extra_data: &[u8],
) -> (
    Option<u64>,
    Option<u32>,
    Option<(u64, u32)>,
    Option<(u64, u32)>,
    Option<String>,
    Option<String>,
    Option<u64>,
) {
    let mut mtime_override = None;
    let mut mtime_ns = None;
    let mut ctime = None;
    let mut atime = None;
    let mut owner = None;
    let mut group = None;
    let mut version = None;
    let mut offset = 0usize;
    while offset < extra_data.len() {
        let (rec_size, n) = match vint::decode_from_slice(extra_data, offset) {
            Ok(v) => v,
            Err(_) => break,
        };
        offset += n;
        let rec_size = match usize::try_from(rec_size) {
            Ok(size) => size,
            Err(_) => break,
        };
        let rec_end = match offset.checked_add(rec_size) {
            Some(end) if end <= extra_data.len() => end,
            _ => break,
        };
        let (rec_type, tn) = match vint::decode_from_slice(extra_data, offset) {
            Ok(v) => v,
            Err(_) => break,
        };
        let body_start = match offset.checked_add(tn) {
            Some(start) if start <= rec_end => start,
            _ => break,
        };
        match rec_type {
            EXTRA_FILE_TIME => {
                // FILE_TIME (HTIME): [flags vint][per present time: 4-byte
                // unix seconds or 8-byte FILETIME][if UNIX_NS: same order,
                // 4-byte nanosecond fields]. Flag bits: 0x01 unix format,
                // 0x02 mtime, 0x04 ctime, 0x08 atime, 0x10 unix ns.
                let mut p = body_start;
                let (flags, fl) = match vint::decode_from_slice(extra_data, p) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                p += fl;
                let unix_format = flags & 0x0001 != 0;
                let have = [
                    flags & 0x0002 != 0, // mtime
                    flags & 0x0004 != 0, // ctime
                    flags & 0x0008 != 0, // atime
                ];
                let mut secs = [0u64; 3];
                let mut nss = [0u32; 3];
                for (i, present) in have.iter().enumerate() {
                    if !*present {
                        continue;
                    }
                    if unix_format {
                        if p + 4 > rec_end {
                            break;
                        }
                        secs[i] =
                            u32::from_le_bytes(extra_data[p..p + 4].try_into().unwrap()) as u64;
                        p += 4;
                    } else {
                        if p + 8 > rec_end {
                            break;
                        }
                        // FILETIME: 100 ns units since 1601-01-01; the
                        // sub-second remainder becomes the nanosecond field.
                        let ft = u64::from_le_bytes(extra_data[p..p + 8].try_into().unwrap());
                        secs[i] = (ft / 10_000_000).saturating_sub(11_644_473_600);
                        nss[i] = ((ft % 10_000_000) * 100) as u32;
                        p += 8;
                    }
                }
                if unix_format && flags & 0x0010 != 0 {
                    for (i, present) in have.iter().enumerate() {
                        if !*present {
                            continue;
                        }
                        if p + 4 > rec_end {
                            break;
                        }
                        let ns = u32::from_le_bytes(extra_data[p..p + 4].try_into().unwrap())
                            & 0x3fff_ffff;
                        nss[i] = if ns < 1_000_000_000 { ns } else { 0 };
                        p += 4;
                    }
                }
                if have[0] {
                    mtime_override = Some(secs[0]);
                    if secs[0] > 0 || nss[0] > 0 {
                        mtime_ns = Some(nss[0]);
                    }
                }
                if have[1] {
                    ctime = Some((secs[1], nss[1]));
                }
                if have[2] {
                    atime = Some((secs[2], nss[2]));
                }
            }
            EXTRA_FILE_OWNER => {
                // [flags][owner len][owner][group len][group]
                let mut p = body_start;
                let (flags, fl) = match vint::decode_from_slice(extra_data, p) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                p += fl;
                if flags & 0x0001 != 0 {
                    if let Some((name, np)) = read_extra_name(extra_data, p, rec_end) {
                        owner = Some(name);
                        p = np;
                    } else {
                        break;
                    }
                }
                if flags & 0x0002 != 0
                    && let Some((name, _)) = read_extra_name(extra_data, p, rec_end)
                {
                    group = Some(name);
                }
            }
            EXTRA_FILE_VERSION => {
                if let Ok((v, _)) = vint::decode_from_slice(extra_data, body_start) {
                    version = Some(v);
                }
            }
            _ => {}
        }
        offset = rec_end;
    }
    (
        mtime_override,
        mtime_ns,
        ctime,
        atime,
        owner,
        group,
        version,
    )
}

/// Extract the extra area of a block body (`[type][flags][extra_size?]
/// [data_size?][...][name][extra at end]`): the last `extra_size` bytes,
/// like the reference reader does.
pub(crate) fn block_extra_area(body: &[u8]) -> RarResult<Vec<u8>> {
    let mut offset = 0usize;
    let (_, n) = vint::decode_from_slice(body, offset)
        .map_err(|e| RarError::Format(format!("block type: {e}")))?;
    offset += n;
    let (flags, n) = vint::decode_from_slice(body, offset)
        .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
    offset += n;
    let mut extra_size = 0usize;
    if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        let (value, _) = vint::decode_from_slice(body, offset)
            .map_err(|e| RarError::Format(format!("block extra size: {e}")))?;
        extra_size = usize::try_from(value).map_err(|_| {
            RarError::Format("block extra size overflows host address space".into())
        })?;
    }
    let start = body
        .len()
        .checked_sub(extra_size)
        .ok_or_else(|| RarError::Format("truncated block extra area".into()))?;
    Ok(body[start..].to_vec())
}

/// Extract the service-data (SUBDATA) record payload from a service
/// block's extra area: the payload of the record whose type is
/// `EXTRA_SERVICE_SUBDATA` (recovery percent for "RR", NTFS stream name
/// for "STM"). `extra_data` is the block's extra area.
pub(crate) fn parse_service_subdata(extra_data: &[u8]) -> Option<Vec<u8>> {
    let mut offset = 0usize;
    while offset + 2 <= extra_data.len() {
        let (rec_size, n) = vint::decode_from_slice(extra_data, offset).ok()?;
        offset += n;
        let rec_size = usize::try_from(rec_size).ok()?;
        let rec_end = offset.checked_add(rec_size)?;
        if rec_end > extra_data.len() {
            return None;
        }
        let (rec_type, tn) = vint::decode_from_slice(extra_data, offset).ok()?;
        let body_start = offset.checked_add(tn)?;
        if body_start > rec_end {
            return None;
        }
        if rec_type == EXTRA_SERVICE_SUBDATA {
            return Some(extra_data[body_start..rec_end].to_vec());
        }
        offset = rec_end;
    }
    None
}

/// Parse the compression parameters of a service block ("STM" stream
/// records carry a compressed payload): `(unpacked_size, method, dict_log)`.
pub(crate) fn parse_stream_params(body: &[u8]) -> Option<(u64, u8, u8)> {
    let mut offset = 0usize;
    let (_, n) = vint::decode_from_slice(body, offset).ok()?;
    offset += n;
    let (flags, n) = vint::decode_from_slice(body, offset).ok()?;
    offset += n;
    if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        let (_, n) = vint::decode_from_slice(body, offset).ok()?;
        offset += n;
    }
    if flags & BLOCK_FLAG_DATA_AREA != 0 {
        let (_, n) = vint::decode_from_slice(body, offset).ok()?;
        offset += n;
    }
    let (file_flags, n) = vint::decode_from_slice(body, offset).ok()?;
    offset += n;
    let (unpacked_size, n) = vint::decode_from_slice(body, offset).ok()?;
    offset += n;
    let (_, n) = vint::decode_from_slice(body, offset).ok()?; // attributes
    offset += n;
    if file_flags & FILE_FLAG_TIME_UNIX != 0 {
        offset = offset.checked_add(4)?;
    }
    if file_flags & FILE_FLAG_CRC32 != 0 {
        offset = offset.checked_add(4)?;
    }
    if offset > body.len() {
        return None;
    }
    let (comp_info, n) = vint::decode_from_slice(body, offset).ok()?;
    let method = ((comp_info >> 7) & 7) as u8;
    let dict_log = ((comp_info >> 10) & 0x0F) as u8;
    let _ = n;
    Some((unpacked_size, method, dict_log))
}

/// Read a length-prefixed name from an extra record body.
fn read_extra_name(data: &[u8], mut p: usize, end: usize) -> Option<(String, usize)> {
    let (len, n) = vint::decode_from_slice(data, p).ok()?;
    p = p.checked_add(n)?;
    let len = usize::try_from(len).ok()?;
    let name_end = p.checked_add(len)?;
    if name_end > end {
        return None;
    }
    let name = String::from_utf8_lossy(&data[p..name_end]).into_owned();
    Some((name, name_end))
}

/// Parse the extra-area hash record (`EXTRA_FILE_HASH`).
///
/// Record format: `[rec_size vint][rec_type vint=0x02][hash_type vint]
/// [hash bytes]`. Hash type `0` is BLAKE2sp (32 bytes).
fn parse_hash_record(extra_data: &[u8]) -> (u8, Option<[u8; 32]>) {
    let mut offset = 0usize;
    while offset < extra_data.len() {
        let (rec_size, n) = match vint::decode_from_slice(extra_data, offset) {
            Ok(v) => v,
            Err(_) => break,
        };
        offset += n;
        let rec_size = match usize::try_from(rec_size) {
            Ok(size) => size,
            Err(_) => break,
        };
        let rec_end = match offset.checked_add(rec_size) {
            Some(end) if end <= extra_data.len() => end,
            _ => break,
        };
        let (rec_type, tn) = match vint::decode_from_slice(extra_data, offset) {
            Ok(v) => v,
            Err(_) => break,
        };
        let body_start = match offset.checked_add(tn) {
            Some(start) if start <= rec_end => start,
            _ => break,
        };
        if rec_type == EXTRA_FILE_HASH {
            if let Ok((hash_type, hn)) = vint::decode_from_slice(extra_data, body_start)
                && let Some(data_start) = body_start.checked_add(hn)
                && data_start <= rec_end
                && hash_type == 0
                && rec_end - data_start == 32
            {
                let mut value = [0u8; 32];
                value.copy_from_slice(&extra_data[data_start..rec_end]);
                return (hash_type as u8, Some(value));
            }
            return (0, None);
        }
        offset = rec_end;
    }
    (u8::MAX, None)
}

impl EndOfArchiveHeader {
    /// Parse from a [`RawBlock`].
    pub fn from_raw(raw: &RawBlock) -> RarResult<Self> {
        let data = &raw.header_data;
        let mut offset = 0;

        let (_, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let (_, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;

        let end_flags = if offset < data.len() {
            let (v, _) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(e.to_string()))?;
            v
        } else {
            0
        };

        Ok(EndOfArchiveHeader { flags: end_flags })
    }
}

/// Locate the quick-open and recovery offset fields inside an existing
/// main archive header (plaintext-relative offsets, used to patch the
/// locator in place when appending).
pub(crate) fn main_header_locator_fields(
    meta: &BlockMeta,
) -> RarResult<(Option<usize>, Option<usize>)> {
    const LOCATOR_TYPE: u64 = 0x01;
    const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
    const LOCATOR_FLAG_RECOVERY: u64 = 0x0002;
    let data = &meta.raw.header_data;
    let mut offset = 0usize;
    let (_, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("block type: {e}")))?;
    offset += n;
    let (flags, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
    offset += n;
    let mut extra_size = 0usize;
    if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        let (v, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("extra size: {e}")))?;
        extra_size = usize::try_from(v).map_err(|_| {
            RarError::Format("main header extra size overflows host address space".into())
        })?;
        offset += n;
    }
    if flags & BLOCK_FLAG_DATA_AREA != 0 {
        let (_, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("data size: {e}")))?;
        offset += n;
    }
    let (_, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("archive flags: {e}")))?;
    offset += n;
    let extra_end = offset
        .checked_add(extra_size)
        .ok_or_else(|| RarError::Format("main header extra size overflows usize".into()))?;
    if extra_end > data.len() {
        return Err(RarError::Format("truncated main header extra area".into()));
    }
    let extra = &data[offset..extra_end];
    // Header layout: [crc 4][size vint][body ...][extra area].
    let extra_base = 4usize
        .checked_add(meta.hsize_vint_len)
        .and_then(|n| n.checked_add(offset))
        .ok_or_else(|| RarError::Format("main header locator offset overflows usize".into()))?;

    let mut e = 0usize;
    while e < extra.len() {
        let (rec_size, n) = vint::decode_from_slice(extra, e)
            .map_err(|e| RarError::Format(format!("extra record: {e}")))?;
        e = e.checked_add(n).ok_or_else(|| {
            RarError::Format("main header extra record offset overflows usize".into())
        })?;
        let rec_start = e;
        let rec_size = usize::try_from(rec_size).map_err(|_| {
            RarError::Format("main header extra record size overflows host address space".into())
        })?;
        let (rec_type, type_len) = vint::decode_from_slice(extra, e)
            .map_err(|e| RarError::Format(format!("extra record type: {e}")))?;
        let type_end = e.checked_add(type_len).ok_or_else(|| {
            RarError::Format("main header extra record type offset overflows usize".into())
        })?;
        if type_end > extra.len() {
            return Err(RarError::Format(
                "truncated main header extra record type".into(),
            ));
        }
        e = type_end;
        if rec_type == LOCATOR_TYPE {
            // WinRAR counts the type vint in the record size, while older
            // rar-rs archives count only the locator body. Parse the known
            // fields first, then require one of those exact boundaries.
            let (loc_flags, n) = vint::decode_from_slice(extra, e)
                .map_err(|e| RarError::Format(format!("locator flags: {e}")))?;
            e = e
                .checked_add(n)
                .ok_or_else(|| RarError::Format("locator flags offset overflows usize".into()))?;
            let mut qo = None;
            if loc_flags & LOCATOR_FLAG_QUICK_OPEN != 0 {
                qo = Some(extra_base.checked_add(e).ok_or_else(|| {
                    RarError::Format("quick-open locator offset overflows usize".into())
                })?);
                let (_, qn) = vint::decode_from_slice(extra, e)
                    .map_err(|e| RarError::Format(format!("quick-open offset: {e}")))?;
                e = e.checked_add(qn).ok_or_else(|| {
                    RarError::Format("quick-open locator end overflows usize".into())
                })?;
            }
            let mut rr = None;
            if loc_flags & LOCATOR_FLAG_RECOVERY != 0 {
                rr = Some(extra_base.checked_add(e).ok_or_else(|| {
                    RarError::Format("recovery locator offset overflows usize".into())
                })?);
                let (_, rn) = vint::decode_from_slice(extra, e)
                    .map_err(|e| RarError::Format(format!("recovery offset: {e}")))?;
                e = e.checked_add(rn).ok_or_else(|| {
                    RarError::Format("recovery locator end overflows usize".into())
                })?;
            }
            let includes_type = rec_start
                .checked_add(rec_size)
                .is_some_and(|rec_end| rec_end == e);
            let excludes_type = type_end
                .checked_add(rec_size)
                .is_some_and(|rec_end| rec_end == e);
            if !includes_type && !excludes_type {
                return Err(RarError::Format(
                    "malformed main header locator record size".into(),
                ));
            }
            return Ok((qo, rr));
        }

        let rec_end = rec_start.checked_add(rec_size).ok_or_else(|| {
            RarError::Format("main header extra record size overflows usize".into())
        })?;
        if rec_end > extra.len() || rec_end < type_end {
            return Err(RarError::Format(
                "truncated main header extra record".into(),
            ));
        }
        e = rec_end;
    }
    Ok((None, None))
}

/// Read the quick-open offset out of a main archive header's extra area
/// (locator record type 0x01, flag 0x0001). The value is relative to the
/// archive start (after the 8-byte signature), matching how the writer
/// patches the field at close time.
pub(crate) fn locator_quick_open_offset(extra: &[u8]) -> Option<u64> {
    const LOCATOR_TYPE: u64 = 0x01;
    const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
    let mut e = 0usize;
    while e < extra.len() {
        let (rec_size, n) = vint::decode_from_slice(extra, e).ok()?;
        e += n;
        let rec_size = usize::try_from(rec_size).ok()?;
        let rec_end = e.checked_add(rec_size)?;
        if rec_end > extra.len() {
            return None;
        }
        let (rec_type, tn) = vint::decode_from_slice(extra, e).ok()?;
        e += tn;
        if rec_type == LOCATOR_TYPE {
            let (flags, fn_) = vint::decode_from_slice(extra, e).ok()?;
            e += fn_;
            if flags & LOCATOR_FLAG_QUICK_OPEN != 0 {
                let (qo, _) = vint::decode_from_slice(extra, e).ok()?;
                return Some(qo);
            }
            return None;
        }
        e = rec_end;
    }
    None
}

/// Parse the block envelope out of a complete in-memory block
/// (`[CRC32][size vint][body]`), verifying the header CRC over the stored
/// size vint bytes plus the body (non-canonical vints included).
pub(crate) fn parse_block_bytes(data: &[u8]) -> RarResult<RawBlock> {
    if data.len() < 5 {
        return Err(RarError::Format("truncated block envelope".into()));
    }
    let stored_crc = u32::from_le_bytes(data[..4].try_into().unwrap());
    let (hsize, vint_len) =
        vint::decode_from_slice(data, 4).map_err(|e| RarError::Format(e.to_string()))?;
    let body_start = 4usize
        .checked_add(vint_len)
        .ok_or_else(|| RarError::Format("header body offset overflows usize".into()))?;
    let hsize = usize::try_from(hsize)
        .map_err(|_| RarError::Format("header size overflows host address space".into()))?;
    let body_end = body_start
        .checked_add(hsize)
        .ok_or_else(|| RarError::Format("header size overflow".into()))?;
    if body_end > data.len() {
        return Err(RarError::Format("truncated block body".into()));
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&data[4..body_end]);
    let computed = hasher.finalize();
    if computed != stored_crc {
        return Err(RarError::Crc {
            expected: stored_crc,
            actual: computed,
            context: "block header".into(),
        });
    }
    let (block_type, flags, data_size) = parse_block_fields(&data[body_start..body_end])?;
    Ok(RawBlock {
        header_crc: stored_crc,
        header_data: data[body_start..body_end].to_vec(),
        data_size,
        data_offset: body_end as u64,
        block_type,
        flags,
    })
}

/// Split a main archive header's extra area into the locator record
/// contents (`had_qo`, `had_rr`) and the remaining records verbatim.
pub(crate) fn split_main_extra(extra: &[u8]) -> RarResult<(bool, bool, Vec<u8>)> {
    const LOCATOR_TYPE: u64 = 0x01;
    const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
    const LOCATOR_FLAG_RECOVERY: u64 = 0x0002;
    let mut had_qo = false;
    let mut had_rr = false;
    let mut rest = Vec::new();
    let mut off = 0usize;
    while off < extra.len() {
        let (rec_size, n) = vint::decode_from_slice(extra, off)
            .map_err(|e| RarError::Format(format!("main header extra record: {e}")))?;
        let rec_start = off.checked_add(n).ok_or_else(|| {
            RarError::Format("main header extra record offset overflows usize".into())
        })?;
        let (rec_type, tn) = vint::decode_from_slice(extra, rec_start)
            .map_err(|e| RarError::Format(format!("main header extra record type: {e}")))?;
        if rec_type == LOCATOR_TYPE {
            // The locator record size convention differs between writers
            // (WinRAR counts the type byte, rar-rs does not), so the record
            // boundary is derived from the parsed fields instead.
            let mut p = rec_start.checked_add(tn).ok_or_else(|| {
                RarError::Format("main header locator offset overflows usize".into())
            })?;
            let (loc_flags, ln) = vint::decode_from_slice(extra, p)
                .map_err(|e| RarError::Format(format!("locator flags: {e}")))?;
            p += ln;
            if loc_flags & LOCATOR_FLAG_QUICK_OPEN != 0 {
                had_qo = true;
                let (_, qn) = vint::decode_from_slice(extra, p)
                    .map_err(|e| RarError::Format(format!("quick-open offset: {e}")))?;
                p += qn;
            }
            if loc_flags & LOCATOR_FLAG_RECOVERY != 0 {
                had_rr = true;
                let (_, rn) = vint::decode_from_slice(extra, p)
                    .map_err(|e| RarError::Format(format!("recovery offset: {e}")))?;
                p += rn;
            }
            off = p;
        } else {
            let rec_size = usize::try_from(rec_size).map_err(|_| {
                RarError::Format(
                    "main header extra record size overflows host address space".into(),
                )
            })?;
            let rec_end = rec_start.checked_add(rec_size).ok_or_else(|| {
                RarError::Format("main header extra record size overflows".into())
            })?;
            if rec_end > extra.len() || rec_end <= rec_start {
                return Err(RarError::Format("malformed main header extra area".into()));
            }
            rest.extend_from_slice(&extra[off..rec_end]);
            off = rec_end;
        }
    }
    Ok((had_qo, had_rr, rest))
}

/// Parse the file redirection record out of an entry's extra area.
pub(crate) fn parse_redirect_record(extra: &[u8]) -> Option<RedirectSpec> {
    let mut offset = 0usize;
    while offset < extra.len() {
        let (rec_size, n) = vint::decode_from_slice(extra, offset).ok()?;
        offset += n;
        // The record size counts the type byte and everything after it.
        let rec_size = usize::try_from(rec_size).ok()?;
        let rec_end = offset.checked_add(rec_size)?;
        if rec_end > extra.len() {
            return None;
        }
        let (rec_type, tn) = vint::decode_from_slice(extra, offset).ok()?;
        let mut p = offset.checked_add(tn)?;
        if p > rec_end {
            return None;
        }
        if rec_type == EXTRA_FILE_REDIRECT {
            let (redir_type, rn) = vint::decode_from_slice(extra, p).ok()?;
            p = p.checked_add(rn)?;
            let (flags, fn_len) = vint::decode_from_slice(extra, p).ok()?;
            p = p.checked_add(fn_len)?;
            let (name_len, nn) = vint::decode_from_slice(extra, p).ok()?;
            p = p.checked_add(nn)?;
            let name_start = p;
            let name_len = usize::try_from(name_len).ok()?;
            let name_end = name_start.checked_add(name_len)?;
            if name_end != rec_end {
                return None;
            }
            let _ = flags;
            return Some(RedirectSpec {
                redir_type,
                target: String::from_utf8_lossy(&extra[name_start..name_end]).into_owned(),
            });
        }
        offset = rec_end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn expect_error<T>(result: RarResult<T>) -> RarError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected parsing to fail"),
        }
    }

    fn raw_block(header_data: Vec<u8>) -> RawBlock {
        RawBlock {
            header_crc: 0,
            header_data,
            data_size: 0,
            data_offset: 0,
            block_type: 0,
            flags: 0,
        }
    }

    #[test]
    fn main_locator_accepts_legacy_size_without_type_vint() {
        let (body, _, _) = crate::format::rar5::headers::locator::build_locator_body(true, false);
        let mut extra = vint::encode(body.len() as u64);
        extra.extend(vint::encode(
            crate::format::rar5::headers::locator::LOCATOR_TYPE,
        ));
        extra.extend(body);

        let mut header_data = Vec::new();
        header_data.extend(vint::encode(BLOCK_TYPE_ARCHIVE_HEADER));
        header_data.extend(vint::encode(BLOCK_FLAG_EXTRA_DATA));
        header_data.extend(vint::encode(extra.len() as u64));
        header_data.extend(vint::encode(0));
        header_data.extend(extra);
        let meta = BlockMeta {
            block_type: BLOCK_TYPE_ARCHIVE_HEADER,
            flags: BLOCK_FLAG_EXTRA_DATA,
            block_start: 0,
            data_offset: 0,
            data_end: 0,
            header_bytes: Vec::new(),
            hsize_vint_len: 1,
            raw: raw_block(header_data),
        };

        let (quick_open, recovery) = main_header_locator_fields(&meta).unwrap();
        assert!(quick_open.is_some());
        assert_eq!(recovery, None);
    }

    #[test]
    fn plaintext_header_size_vint_rejects_more_than_ten_bytes() {
        let mut bytes = vec![0u8; 4];
        bytes.extend([0x80; 10]);
        let error = expect_error(read_plain_header(&mut Cursor::new(bytes)));
        assert!(error.to_string().contains("exceeds 10 bytes"), "{error}");
    }

    #[test]
    fn plaintext_header_size_vint_accepts_ten_bytes() {
        let mut bytes = vec![0u8; 4];
        bytes.push(0x81);
        bytes.extend([0x80; 8]);
        bytes.push(0);
        bytes.push(0);

        match read_plain_header(&mut Cursor::new(bytes)) {
            Ok(Some(header)) => {
                assert_eq!(header.vint_bytes.len(), 10);
                assert_eq!(header.body, [0]);
            }
            Ok(None) => panic!("expected a header"),
            Err(error) => panic!("expected a valid ten-byte vint: {error}"),
        }
    }

    #[test]
    fn plaintext_header_rejects_partial_crc() {
        let error = expect_error(read_plain_header(&mut Cursor::new(vec![0u8; 3])));
        assert!(error.to_string().contains("truncated plaintext header CRC"));
    }

    #[test]
    fn block_data_offset_overflow_is_rejected() {
        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_FILE_HEADER));
        body.extend(vint::encode(BLOCK_FLAG_DATA_AREA));
        body.extend(vint::encode(u64::MAX));
        let hsize = vint::encode(body.len() as u64);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&hsize);
        hasher.update(&body);
        let mut bytes = hasher.finalize().to_le_bytes().to_vec();
        bytes.extend(hsize);
        bytes.extend(body);

        let error = expect_error(read_block(&mut Cursor::new(bytes), None));
        assert!(error.to_string().contains("offset overflows"), "{error}");
    }

    #[test]
    fn file_header_rejects_truncated_name() {
        let mut body = Vec::new();
        for value in [BLOCK_TYPE_FILE_HEADER, 0, 0, 0, 0, 0, 0, 5] {
            body.extend(vint::encode(value));
        }
        body.extend_from_slice(b"ab");

        let error = FileHeader::from_raw(&raw_block(body), 0).unwrap_err();
        assert!(error.to_string().contains("truncated file name"), "{error}");
    }

    #[test]
    fn archive_header_rejects_truncated_extra_area() {
        let mut body = Vec::new();
        for value in [BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_FLAG_EXTRA_DATA, 5, 0] {
            body.extend(vint::encode(value));
        }
        body.extend_from_slice(&[1, 2]);

        let error = expect_error(ArchiveHeader::from_raw(&raw_block(body)));
        assert!(error.to_string().contains("truncated archive extra area"));
    }
}
