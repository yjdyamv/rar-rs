pub(crate) mod locator;
/// The outer RAR5 block envelope is shared by every header:
/// ```text
/// [Header CRC32]  4 bytes LE
/// [Header Size]   vint -- bytes after this field
/// [Header Type]   vint
/// [Header Flags]  vint
/// [Extra Size]    vint -- if BLOCK_FLAG_EXTRA_DATA
/// [Data Size]     vint -- if BLOCK_FLAG_DATA_AREA
/// ... type-specific fields ...
/// [Extra Area]    bytes -- if present
/// ```
///
/// Read-side parsing lives in [`parse`]; write-side serialization in
/// [`serialize`]. RAR5 wire structs and the block envelope stay here, while
/// shared model types are re-exported for compatibility.
pub(crate) mod parse;
pub(crate) mod serialize;

pub use crate::model::{DataChunk, FileHeader};
pub use parse::read_block;
pub(crate) use parse::{
    block_extra_area, locator_quick_open_offset, main_header_locator_fields, parse_block_bytes,
    parse_redirect_record, parse_service_subdata, parse_stream_params, split_main_extra,
};
#[cfg(unix)]
pub(crate) use serialize::build_owner_extra_record;
pub use serialize::hash_extra_record;
pub(crate) use serialize::{
    build_comment_block, build_service_block, file_time_extra_record, redirect_extra_bytes,
    vint_fixed5,
};

/// A raw, unparsed RAR5 block as read from the archive stream.
pub struct RawBlock {
    pub header_crc: u32,
    pub header_data: Vec<u8>,
    pub data_size: u64,
    pub data_offset: u64,
    pub block_type: u64,
    pub flags: u64,
}

/// Byte span of one block in an archive being read or rewritten, with its
/// parsed (plaintext) header and the exact on-disk header bytes.
pub struct BlockMeta {
    pub block_type: u64,
    pub flags: u64,
    /// Absolute offset where the block starts (the CRC32 field; for
    /// header-encrypted archives, the IV).
    pub block_start: u64,
    /// Absolute offset where the data area starts (right after the header;
    /// for header-encrypted archives after the IV + ciphertext).
    pub data_offset: u64,
    /// Absolute offset one past the end of the block.
    pub data_end: u64,
    /// Exact bytes of the header as stored on disk: `[CRC32][size vint]
    /// [body]`, or `[IV][ciphertext]` for header-encrypted archives.
    pub header_bytes: Vec<u8>,
    /// Length of the size vint inside the plaintext header.
    pub hsize_vint_len: usize,
    pub raw: RawBlock,
}

/// The decrypted/plaintext pieces of one block header plus its exact
/// on-disk bytes.
struct RawHeader {
    stored_crc: u32,
    vint_bytes: Vec<u8>,
    body: Vec<u8>,
    on_disk: Vec<u8>,
}

/// RAR5 Main Archive Header (block type 0x01).
pub struct ArchiveHeader {
    pub flags: u64,
    pub extra_data: Vec<u8>,
    pub volume_number: Option<u64>,
}

// ── End of Archive Header ──────────────────────────────────────────────────

/// RAR5 End of Archive Header (block type 0x05).
pub struct EndOfArchiveHeader {
    pub flags: u64,
}

/// RAR5 file redirection (EXTRA_FILE_REDIRECT) record: symlink, hardlink
/// or file copy target reference.
pub(crate) struct RedirectSpec {
    pub redir_type: u64,
    pub target: String,
}
