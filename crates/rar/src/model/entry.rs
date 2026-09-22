//! Format-neutral entry model shared by the RAR4 and RAR5 families.

// These private compatibility constants preserve the values previously supplied
// by rar50::{COMP_METHOD_STORE, OS_UNIX, FILE_FLAG_TIME_UNIX,
// FILE_FLAG_CRC32}. Keeping the values local makes the model a format-layer leaf.
const DEFAULT_COMP_METHOD: u8 = 0;
const DEFAULT_HOST_OS: u64 = 1;
const DEFAULT_FILE_FLAGS: u64 = 0x0002 | 0x0004;

/// Normalized file metadata shared by the supported RAR format families.
#[derive(Clone, Debug)]
pub struct FileHeader {
    /// Member name as stored (forward-slash separated, UTF-8).
    pub name: String,
    /// Uncompressed size in bytes.
    pub unpacked_size: u64,
    /// Packed (on-disk) size in bytes.
    pub packed_size: u64,
    /// Host attributes: RAR5 Unix mode bits or Windows DOS attributes,
    /// RAR4 the legacy 32-bit attribute word.
    pub attributes: u64,
    /// Modification time: Unix seconds for RAR5, DOS local wall-clock
    /// seconds for RAR 1.5–4.x (see [`ArchiveEntry::mtime`](crate::ArchiveEntry::mtime)).
    pub mtime: u32,
    /// Member checksum. CRC-32 for RAR4/RAR5 members, except RAR 1.3/1.4
    /// members (`format_version == 3`) where it holds the 16-bit rolling
    /// checksum in the low half.
    pub crc32_val: Option<u32>,
    /// Wire hash-record type (`0` = BLAKE2sp) when a hash extra record is
    /// present, otherwise `u8::MAX`.
    pub hash_type: u8,
    /// Expected file hash from the extra-area hash record.
    pub hash_value: Option<[u8; 32]>,
    /// Numeric compression method (`0` = store, `1`..=`5` = level); RAR 1.3/1.4
    /// reuse the field for their own codec selector.
    pub comp_method: u8,
    /// Codec generation within the family (RAR5: `0` = v50, `1` = RAR7 v70).
    pub comp_version: u8,
    /// Whether the member continues a solid chain (its data depends on the
    /// previous member's window).
    pub comp_solid: bool,
    /// Dictionary setting. RAR5: `log2(dictionary/128 KiB)`, with RAR7
    /// members carrying the byte count in
    /// [`dict_size_bytes`](Self::dict_size_bytes) instead. RAR 1.5–4.x:
    /// the window-bits field, `log2(window/64 KiB)` (7 marks a directory
    /// block).
    pub comp_dict_size: u8,
    /// Host OS the archive was written on (RAR5: `0` = Windows, `1` = Unix).
    pub host_os: u64,
    /// Raw member flags of the family's header (RAR5 file flags, RAR4
    /// `FILE_HEAD` flags).
    pub flags: u64,
    /// RAR5 file flag word (dictionary/hash/time flags); `0` for RAR4 members,
    /// whose flags live in [`flags`](Self::flags).
    pub file_flags: u64,
    /// Raw extra-area bytes of the member header (RAR5 extras / RAR4
    /// after-name records), undecoded.
    pub extra_data: Vec<u8>,
    /// Whether the member is a directory entry rather than a file.
    pub is_directory: bool,
    /// Offset of the member's packed data in its first volume.
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
    /// Group name (OWNER extra record), when the archive recorded one.
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
    // Only read through the `wire` surface.
    pub legacy_head_crc: Option<u16>,
    /// Per-member (file) comment for RAR 1.5–4.x archives: a `COMM_HEAD`
    /// block after the member data (RAR 3.x/4.x) or nested in the header via
    /// `FHD_COMMENT` (RAR 1.5–2.9). `None` when the member carries no
    /// comment. Decoded to raw text bytes (UTF-8 kept as-is; an even-length
    /// non-UTF-8 payload is treated as UTF-16LE), matching the
    /// archive-comment payload convention.
    pub comment: Option<Vec<u8>>,
}

/// Which attribute encoding a member's attribute field holds.
#[cfg(any(unix, windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostAttributes {
    /// Unix permission bits (RAR5 Unix-host members; RAR 1.5–4.x hosts 3
    /// and 5, whose mode sits in the high half of the attribute word).
    UnixMode(u32),
    /// DOS attribute bits (Windows-host members; RAR 1.3/1.4 always).
    Dos,
    /// Another host's encoding: no Unix mode to restore, and no stored DOS
    /// bits either (extraction applies the archive/directory bits only).
    Other,
}

impl FileHeader {
    /// Whether the stored time is legacy local wall-clock time
    /// (RAR 1.3–4.x) rather than a Unix instant.
    pub(crate) fn uses_local_civil_time(&self) -> bool {
        self.format_version == 3 || self.format_version == 4
    }

    /// Whether [`crc32_val`](Self::crc32_val) holds the RAR 1.3/1.4 16-bit
    /// rolling checksum rather than a CRC32.
    pub(crate) fn uses_rar13_checksum(&self) -> bool {
        self.format_version == 3
    }

    /// How to interpret the attribute field.
    #[cfg(any(unix, windows))]
    pub(crate) fn host_attributes(&self) -> HostAttributes {
        match self.format_version {
            // RAR 1.3/1.4 carry DOS attributes only.
            3 => HostAttributes::Dos,
            // RAR 1.5–4.x: hosts 3 (Unix) and 5 (BeOS) store a mode.
            4 if matches!(self.host_os, 3 | 5) => {
                HostAttributes::UnixMode(((self.attributes >> 16) & 0o7777) as u32)
            }
            4 => HostAttributes::Dos,
            // RAR5+: host 1 is Unix and the attribute vint is the mode;
            // host 0 is Windows, any other host carries neither.
            _ if self.host_os == 1 => HostAttributes::UnixMode((self.attributes & 0o7777) as u32),
            _ if self.host_os == 0 => HostAttributes::Dos,
            _ => HostAttributes::Other,
        }
    }
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
            comment: None,
        }
    }
}
