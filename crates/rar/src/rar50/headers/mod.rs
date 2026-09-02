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
/// [`serialize`]. The shared structs and block envelope stay here.
pub(crate) mod parse;
pub(crate) mod serialize;

pub use parse::*;
pub use serialize::*;

use crate::rar50::*;

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
    /// Actual dictionary size in bytes for RAR7 members (`comp_version`
    /// 1): the 5-bit dict field plus the 1/32 increment allow
    /// non-power-of-two sizes up to 64 GB. `None` for RAR5 members, whose
    /// dictionary is `128 KiB << comp_dict_size`.
    pub dict_size_bytes: Option<u64>,
    /// Nanosecond fraction of the modification time (FILE_TIME extra
    /// record); `None` when only the second-precision header time exists.
    pub mtime_ns: Option<u32>,
    /// Creation/change time from the FILE_TIME extra record (seconds,
    /// nanoseconds); `None` when absent. Windows creation time, or ctime
    /// (inode change time) on Unix, matching WinRAR's `-tsc`.
    pub ctime: Option<(u64, u32)>,
    /// Last access time from the FILE_TIME extra record (seconds,
    /// nanoseconds); `None` when absent (WinRAR `-tsa`).
    pub atime: Option<(u64, u32)>,
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
            dict_size_bytes: None,
            mtime_ns: None,
            ctime: None,
            atime: None,
            owner: None,
            group: None,
            version: None,
        }
    }
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
