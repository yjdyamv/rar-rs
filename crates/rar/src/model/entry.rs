// These private compatibility constants preserve the values previously supplied
// by rar50::{COMP_METHOD_STORE, OS_UNIX, FILE_FLAG_TIME_UNIX,
// FILE_FLAG_CRC32}. Keeping the values local makes the model a format-layer leaf.
const DEFAULT_COMP_METHOD: u8 = 0;
const DEFAULT_HOST_OS: u64 = 1;
const DEFAULT_FILE_FLAGS: u64 = 0x0002 | 0x0004;

/// Normalized file metadata shared by the supported RAR format families.
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
    /// non-power-of-two sizes up to 126 GiB. `None` for RAR5 members, whose
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
    /// RAR 1.5–4.x unpack version codec selector (`15`/`20`/`26`/`29`/`36`).
    /// Meaningful only when [`format_version`](Self::format_version) is 4.
    pub unp_ver: u8,
    /// RAR 3.x+ (unp_ver >= 29) per-file encryption salt; `None` for
    /// RAR1.5/2.x (no salt) and for unencrypted members.
    pub salt: Option<[u8; 8]>,
    /// RAR 1.5–4.x raw header CRC (16-bit); `None` for RAR5 members, which
    /// use the 32-bit header CRC32.
    pub legacy_head_crc: Option<u16>,
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
            comp_method: DEFAULT_COMP_METHOD,
            comp_version: 0,
            comp_solid: false,
            comp_dict_size: 0,
            host_os: DEFAULT_HOST_OS,
            flags: 0,
            file_flags: DEFAULT_FILE_FLAGS,
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
            unp_ver: 0,
            salt: None,
            legacy_head_crc: None,
        }
    }
}
