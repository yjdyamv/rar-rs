//! Write-side serialization of RAR5 block/header envelopes and the
//! extra-record / service-block builders.

#[cfg(unix)]
use crate::format::rar5::EXTRA_FILE_OWNER;
use crate::format::rar5::headers::{ArchiveHeader, EndOfArchiveHeader, FileHeader};
use crate::format::rar5::vint;
use crate::format::rar5::{
    ARCHIVE_FLAG_VOLUME, ARCHIVE_FLAG_VOLUME_NUM, BLOCK_FLAG_DATA_AREA, BLOCK_FLAG_EXTRA_DATA,
    BLOCK_FLAG_SKIP_IF_UNKNOWN, BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_TYPE_END_ARCHIVE,
    BLOCK_TYPE_FILE_HEADER, BLOCK_TYPE_SERVICE_HEADER, COMP_INFO_DICT_SHIFT,
    COMP_INFO_METHOD_SHIFT, COMP_INFO_SOLID_BIT, EXTRA_FILE_HASH, EXTRA_FILE_TIME, FILE_FLAG_CRC32,
    FILE_FLAG_DIRECTORY, FILE_FLAG_TIME_UNIX, OS_UNIX,
};

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
pub(crate) fn build_comment_block(comment: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(vint::encode(BLOCK_TYPE_SERVICE_HEADER));
    body.extend(vint::encode(BLOCK_FLAG_DATA_AREA));
    body.extend(vint::encode(comment.len() as u64));
    body.extend(vint::encode(FILE_FLAG_CRC32));
    body.extend(vint::encode(comment.len() as u64)); // unpacked size
    body.extend(vint::encode(0u64)); // attributes
    body.extend(crc32fast::hash(comment).to_le_bytes());
    body.extend(vint::encode(0u64)); // compression info (store)
    body.extend(vint::encode(OS_UNIX));
    body.extend(vint::encode(3u64)); // name length
    body.extend(b"CMT");

    let size_bytes = vint::encode(body.len() as u64);
    let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
    header_content.extend(&size_bytes);
    header_content.extend(&body);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&header_content);
    let crc = hasher.finalize();

    let mut block = Vec::with_capacity(4 + header_content.len() + comment.len());
    block.extend(crc.to_le_bytes());
    block.extend(header_content);
    block.extend_from_slice(comment);
    block
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
    body.extend(vint::encode(data_size)); // data size
    body.extend(vint::encode(0u64)); // file flags
    body.extend(vint::encode(data_size)); // unpacked size
    body.extend(vint::encode(0u64)); // attributes
    body.extend(vint::encode(0u64)); // compression info (store)
    body.extend(vint::encode(OS_UNIX));
    body.extend(vint::encode(name.len() as u64));
    body.extend(name.as_bytes());
    body.extend(subdata);

    let size_bytes = vint::encode(body.len() as u64);
    let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
    header_content.extend(&size_bytes);
    header_content.extend(&body);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&header_content);
    let crc = hasher.finalize();
    let mut hdr = Vec::with_capacity(4 + header_content.len());
    hdr.extend(crc.to_le_bytes());
    hdr.extend(header_content);
    hdr
}
/// Encode `value` as a fixed 5-byte RAR5 vint (LSB-first, continuation bit
/// on every byte except the last). Valid for values < 2^35.
pub(crate) fn vint_fixed5(value: u64) -> [u8; 5] {
    let mut out = [0x80u8; 5];
    let mut v = value;
    for (i, byte) in out.iter_mut().enumerate() {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if i < 4 {
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
}
