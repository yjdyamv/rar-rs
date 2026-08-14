/// RAR5 block and header data structures.
///
/// Every RAR5 block shares the same outer envelope:
/// ```text
/// [Header CRC32]  4 bytes LE
/// [Header Size]   vint — bytes after this field
/// [Header Type]   vint
/// [Header Flags]  vint
/// [Extra Size]    vint — if BLOCK_FLAG_EXTRA_DATA
/// [Data Size]     vint — if BLOCK_FLAG_DATA_AREA
/// ... type-specific fields ...
/// [Extra Area]    bytes — if present
/// ```
use std::io::{self, Read, Seek};

use crate::constants::*;
use crate::error::{RarError, RarResult};
use crate::vint;

// ── Raw Block ──────────────────────────────────────────────────────────────

/// A raw, unparsed RAR5 block as read from the archive stream.
pub struct RawBlock {
    pub header_crc: u32,
    pub header_data: Vec<u8>,
    pub data_size: u64,
    pub data_offset: u64,
    pub block_type: u64,
    pub flags: u64,
}

impl RawBlock {
    /// Read the next raw block from the stream.
    /// Validates the header CRC32.
    pub fn read_from<R: Read + Seek>(r: &mut R) -> RarResult<Self> {
        // Read 4-byte header CRC32
        let mut crc_buf = [0u8; 4];
        r.read_exact(&mut crc_buf).map_err(|_| {
            RarError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stream ended reading block CRC",
            ))
        })?;
        let stored_crc = u32::from_le_bytes(crc_buf);

        // Read header size (vint)
        let header_size = vint::read(r).map_err(|e| RarError::Format(format!("bad vint: {e}")))?;
        if header_size == 0 || header_size > 2 * 1024 * 1024 {
            return Err(RarError::Format(format!(
                "implausible header size: {header_size}"
            )));
        }

        // Read the entire header body
        let mut header_data = vec![0u8; header_size as usize];
        r.read_exact(&mut header_data)?;

        // Validate CRC over size_bytes + header_body
        let size_bytes = vint::encode(header_size);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&size_bytes);
        hasher.update(&header_data);
        let computed_crc = hasher.finalize();
        if computed_crc != stored_crc {
            return Err(RarError::Crc {
                expected: stored_crc,
                actual: computed_crc,
                context: "block header".into(),
            });
        }

        // Parse type and flags
        let mut offset = 0usize;
        let (block_type, n) = vint::decode_from_slice(&header_data, offset)
            .map_err(|e| RarError::Format(format!("block type: {e}")))?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(&header_data, offset)
            .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
        offset += n;

        // Extra area and data area sizes
        let mut _extra_size = 0u64;
        let mut data_size = 0u64;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (v, n) = vint::decode_from_slice(&header_data, offset)
                .map_err(|e| RarError::Format(format!("extra size: {e}")))?;
            _extra_size = v;
            offset += n;
        }
        if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (v, n) = vint::decode_from_slice(&header_data, offset)
                .map_err(|e| RarError::Format(format!("data size: {e}")))?;
            data_size = v;
            offset += n;
        }
        let _ = offset; // remaining header_data parsed by typed header

        let data_offset = r.stream_position()?;

        Ok(RawBlock {
            header_crc: stored_crc,
            header_data,
            data_size,
            data_offset,
            block_type,
            flags,
        })
    }
}

// ── Archive Header ─────────────────────────────────────────────────────────

/// RAR5 Main Archive Header (block type 0x01).
pub struct ArchiveHeader {
    pub flags: u64,
    pub extra_data: Vec<u8>,
    pub volume_number: Option<u64>,
}

impl ArchiveHeader {
    /// Serialize to RAR5 binary format (including CRC).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_ARCHIVE_HEADER));

        // Block-level flags (not archive-level flags)
        let mut block_flags = 0u64;
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

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();

        let mut result = Vec::with_capacity(4 + header_content.len());
        result.extend(crc.to_le_bytes());
        result.extend(header_content);
        result
    }

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

        let extra_data = if extra_size > 0 && offset < data.len() {
            let end = (offset + extra_size as usize).min(data.len());
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

// ── Data Chunk ─────────────────────────────────────────────────────────────

/// Describes a contiguous slice of packed file data within one volume.
///
/// Multi-volume archives split a file's packed data across multiple volumes.
#[derive(Clone, Debug)]
pub struct DataChunk {
    pub volume_index: usize,
    pub data_offset: u64,
    pub packed_size: u64,
    pub crc32_val: Option<u32>,
    pub is_final: bool,
    pub extra_data: Vec<u8>,
}

// ── File Header ────────────────────────────────────────────────────────────

/// File Header (RAR5 block type 0x02).
#[derive(Clone, Debug)]
pub struct FileHeader {
    pub name: String,
    pub unpacked_size: u64,
    pub packed_size: u64,
    pub attributes: u64,
    pub mtime: u32,
    pub crc32_val: Option<u32>,
    /// Wire hash-record type (`0` = BLAKE2sp) when a hash extra record is
    /// present, otherwise `u8::MAX`.
    pub hash_type: u8,
    /// Expected file hash from the extra-area hash record.
    pub hash_value: Option<[u8; 32]>,
    pub comp_method: u8,
    pub comp_version: u8,
    pub comp_solid: bool,
    pub comp_dict_size: u8,
    pub host_os: u64,
    pub flags: u64,
    pub file_flags: u64,
    pub extra_data: Vec<u8>,
    pub is_directory: bool,
    pub data_offset: u64,
    /// Archive format version (4 or 5).
    pub format_version: u8,
    /// Nanosecond fraction of the modification time (FILE_TIME extra
    /// record); `None` when only the second-precision header time exists.
    pub mtime_ns: Option<u32>,
    /// Owner and group names (OWNER extra record).
    pub owner: Option<String>,
    pub group: Option<String>,
    /// File version (VERSION extra record).
    pub version: Option<u64>,
}

impl Default for FileHeader {
    fn default() -> Self {
        FileHeader {
            name: String::new(),
            unpacked_size: 0,
            packed_size: 0,
            attributes: 0o100644,
            mtime: 0,
            crc32_val: None,
            hash_type: u8::MAX,
            hash_value: None,
            comp_method: COMP_METHOD_STORE,
            comp_version: 0,
            comp_solid: false,
            comp_dict_size: 0,
            host_os: OS_UNIX,
            flags: 0,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
            extra_data: Vec::new(),
            is_directory: false,
            data_offset: 0,
            format_version: 5,
            mtime_ns: None,
            owner: None,
            group: None,
            version: None,
        }
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
            body.extend(vint::encode(self.packed_size));
        }

        body.extend(vint::encode(eff_file_flags));
        body.extend(vint::encode(self.unpacked_size));
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
        comp_info |= ((self.comp_dict_size as u64) & 0x0F) << COMP_INFO_DICT_SHIFT;
        body.extend(vint::encode(comp_info));
        body.extend(vint::encode(self.host_os));

        let name_bytes = self.name.as_bytes();
        body.extend(vint::encode(name_bytes.len() as u64));
        body.extend(name_bytes);

        body.extend(&self.extra_data);

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();

        let mut result = Vec::with_capacity(4 + header_content.len());
        result.extend(crc.to_le_bytes());
        result.extend(header_content);
        result
    }

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
            if offset + 4 > data.len() {
                return Err(RarError::Format("truncated mtime".into()));
            }
            mtime = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
        }

        let mut crc32_val = None;
        if file_flags & FILE_FLAG_CRC32 != 0 {
            if offset + 4 > data.len() {
                return Err(RarError::Format("truncated CRC32".into()));
            }
            crc32_val = Some(u32::from_le_bytes(
                data[offset..offset + 4].try_into().unwrap(),
            ));
            offset += 4;
        }

        let (comp_info, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let comp_version = (comp_info & COMP_INFO_VERSION_MASK) as u8;
        let comp_solid = comp_info & COMP_INFO_SOLID_BIT != 0;
        let comp_method = ((comp_info & COMP_INFO_METHOD_MASK) >> COMP_INFO_METHOD_SHIFT) as u8;
        let comp_dict_size = ((comp_info & COMP_INFO_DICT_MASK) >> COMP_INFO_DICT_SHIFT) as u8;

        let (host_os, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;
        let (name_len, n) =
            vint::decode_from_slice(data, offset).map_err(|e| RarError::Format(e.to_string()))?;
        offset += n;

        let name_end = (offset + name_len as usize).min(data.len());
        let name = String::from_utf8_lossy(&data[offset..name_end]).into_owned();
        offset = name_end;

        let extra_data = if extra_size > 0 && offset < data.len() {
            let end = (offset + extra_size as usize).min(data.len());
            data[offset..end].to_vec()
        } else {
            Vec::new()
        };

        let is_directory = file_flags & FILE_FLAG_DIRECTORY != 0;
        let (hash_type, hash_value) = parse_hash_record(&extra_data);
        let (mtime_ns, owner, group, version) = parse_extra_records(&extra_data);

        Ok(FileHeader {
            name,
            unpacked_size,
            packed_size: data_size,
            attributes,
            mtime,
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
            mtime_ns,
            owner,
            group,
            version,
        })
    }
}

/// Parse the extra-area records that carry member metadata: nanosecond
/// modification time (`EXTRA_FILE_TIME`), owner/group names
/// (`EXTRA_FILE_OWNER`) and the file version (`EXTRA_FILE_VERSION`).
fn parse_extra_records(
    extra_data: &[u8],
) -> (Option<u32>, Option<String>, Option<String>, Option<u64>) {
    let mut mtime_ns = None;
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
        let rec_end = match offset.checked_add(rec_size as usize) {
            Some(end) if end <= extra_data.len() => end,
            _ => break,
        };
        let (rec_type, tn) = match vint::decode_from_slice(extra_data, offset) {
            Ok(v) => v,
            Err(_) => break,
        };
        let body_start = offset + tn;
        match rec_type {
            EXTRA_FILE_TIME => {
                // [flags][per present time: sec u32 + ns u32]; the
                // modification time is the first pair.
                let mut p = body_start;
                let (flags, fl) = match vint::decode_from_slice(extra_data, p) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                p += fl;
                if flags & 0x0001 != 0 && p + 8 <= rec_end {
                    let sec = u32::from_le_bytes(extra_data[p..p + 4].try_into().unwrap());
                    let ns = u32::from_le_bytes(extra_data[p + 4..p + 8].try_into().unwrap());
                    if sec > 0 || ns > 0 {
                        mtime_ns = Some(ns);
                    }
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
    (mtime_ns, owner, group, version)
}

/// Read a length-prefixed name from an extra record body.
fn read_extra_name(data: &[u8], mut p: usize, end: usize) -> Option<(String, usize)> {
    let (len, n) = vint::decode_from_slice(data, p).ok()?;
    p += n;
    let name_end = p.checked_add(len as usize)?;
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
        let rec_end = match offset.checked_add(rec_size as usize) {
            Some(end) if end <= extra_data.len() => end,
            _ => break,
        };
        let (rec_type, tn) = match vint::decode_from_slice(extra_data, offset) {
            Ok(v) => v,
            Err(_) => break,
        };
        let body_start = offset + tn;
        if rec_type == EXTRA_FILE_HASH {
            if let Ok((hash_type, hn)) = vint::decode_from_slice(extra_data, body_start) {
                let data_start = body_start + hn;
                if hash_type == 0 && rec_end - data_start == 32 {
                    let mut value = [0u8; 32];
                    value.copy_from_slice(&extra_data[data_start..rec_end]);
                    return (hash_type as u8, Some(value));
                }
            }
            return (0, None);
        }
        offset = rec_end;
    }
    (u8::MAX, None)
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

// ── End of Archive Header ──────────────────────────────────────────────────

/// RAR5 End of Archive Header (block type 0x05).
pub struct EndOfArchiveHeader {
    pub flags: u64,
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

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();

        let mut result = Vec::with_capacity(4 + header_content.len());
        result.extend(crc.to_le_bytes());
        result.extend(header_content);
        result
    }

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
