//! The write plan for one member: everything the family writers need
//! besides the payload bytes.
//!
//! Named so the serial, batch and streamed writers share one value instead of
//! a positional slab. It lives below `format` because `PreparedEntry` (in
//! this module) carries one across the parallel prepare/write split.

use crate::model::FileHeader;

/// Everything a member write needs besides the payload bytes: the file
/// header fields and the extra records. Named so the serial, batch and
/// streamed writers share one value instead of a positional slab.
pub(crate) struct MemberPlan {
    pub(crate) name: String,
    pub(crate) unpacked_size: u64,
    /// Header CRC: the plaintext CRC, or its hash-key MAC when encrypted.
    pub(crate) file_crc: u32,
    pub(crate) method: u8,
    pub(crate) dict_size_log: u8,
    pub(crate) dict_size_bytes: Option<u64>,
    /// Encryption/hash records plus the caller's FILE_TIME/OWNER extras.
    pub(crate) extra_data: Vec<u8>,
    pub(crate) attrs: u64,
    pub(crate) mtime: u32,
    pub(crate) solid: bool,
    /// BLAKE2sp hash value (MAC'd when encrypted).
    pub(crate) stored_hash: Option<[u8; 32]>,
}

impl MemberPlan {
    /// Append the caller's FILE_TIME / OWNER records after the
    /// encryption/hash records `payload_extra_and_crc` built.
    pub(crate) fn push_extra(&mut self, time_extra: Option<&[u8]>, owner_extra: Option<&[u8]>) {
        if let Some(time) = time_extra {
            self.extra_data.extend_from_slice(time);
        }
        if let Some(owner) = owner_extra {
            self.extra_data.extend_from_slice(owner);
        }
    }

    /// The member's file header with the on-disk `packed_size` and the
    /// time-derived `mtime` / `file_flags` filled in.
    pub(crate) fn file_header(&self, packed_size: u64, mtime: u32, file_flags: u64) -> FileHeader {
        FileHeader {
            name: self.name.clone(),
            unpacked_size: self.unpacked_size,
            packed_size,
            attributes: self.attrs,
            mtime,
            // `-htb`: the BLAKE2sp record *replaces* the CRC32 field, like
            // WinRAR (which clears `FILE_FLAG_CRC32`); the serializer drops
            // the flag and the field when this is `None`.
            crc32_val: if self.stored_hash.is_some() {
                None
            } else {
                Some(self.file_crc)
            },
            hash_type: if self.stored_hash.is_some() {
                0
            } else {
                u8::MAX
            },
            hash_value: self.stored_hash,
            comp_method: self.method,
            comp_solid: self.solid,
            comp_dict_size: self.dict_size_log,
            dict_size_bytes: self.dict_size_bytes,
            host_os: crate::platform::host_os(),
            file_flags,
            extra_data: self.extra_data.clone(),
            ..Default::default()
        }
    }
}
