//! Private format-neutral archive-domain model.
//!
//! Format parsers translate wire data into these values, and archive
//! orchestration stores them. This module must remain independent of the
//! `rar40` and `rar50` format implementations.

mod chunk;
mod entry;

pub use chunk::DataChunk;
pub use entry::FileHeader;

#[cfg(test)]
mod tests {
    use super::FileHeader;

    #[test]
    fn file_header_defaults_match_legacy_public_contract() {
        let header = FileHeader::default();

        assert!(header.name.is_empty());
        assert_eq!(header.unpacked_size, 0);
        assert_eq!(header.packed_size, 0);
        assert_eq!(header.attributes, 0o100644);
        assert_eq!(header.mtime, 0);
        assert_eq!(header.crc32_val, None);
        assert_eq!(header.hash_type, u8::MAX);
        assert_eq!(header.hash_value, None);
        assert_eq!(header.comp_method, 0);
        assert_eq!(header.comp_version, 0);
        assert!(!header.comp_solid);
        assert_eq!(header.comp_dict_size, 0);
        assert_eq!(header.host_os, 1);
        assert_eq!(header.flags, 0);
        assert_eq!(header.file_flags, 0x0002 | 0x0004);
        assert!(header.extra_data.is_empty());
        assert!(!header.is_directory);
        assert_eq!(header.data_offset, 0);
        assert_eq!(header.format_version, 5);
        assert_eq!(header.dict_size_bytes, None);
        assert_eq!(header.mtime_ns, None);
        assert_eq!(header.ctime, None);
        assert_eq!(header.atime, None);
        assert_eq!(header.owner, None);
        assert_eq!(header.group, None);
        assert_eq!(header.version, None);
        assert_eq!(header.unp_ver, 0);
        assert_eq!(header.salt, None);
        assert_eq!(header.legacy_head_crc, None);
    }
}
