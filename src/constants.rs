/// RAR5 format constants and definitions.
///
/// RAR5 archive structure:
/// ```text
/// [Self-Extracting Module (optional)]
/// [Archive Signature]          -- 8 bytes: magic number
/// [Archive Encryption Header]  -- optional
/// [Main Archive Header]        -- archive-level metadata
/// [File Header] [File Data]    -- one per archived file
/// ...
/// [End of Archive Header]      -- marks archive end
/// ```

// ── Archive Signature ──────────────────────────────────────────────────────

/// RAR5 magic number (8 bytes).
pub const RAR5_SIGNATURE: &[u8; 8] = b"Rar!\x1a\x07\x01\x00";


// ── Block Types ────────────────────────────────────────────────────────────

pub const BLOCK_TYPE_ARCHIVE_HEADER: u64 = 0x01;
pub const BLOCK_TYPE_FILE_HEADER: u64 = 0x02;
pub const BLOCK_TYPE_SERVICE_HEADER: u64 = 0x03;
pub const BLOCK_TYPE_ENCRYPT_HEADER: u64 = 0x04;
pub const BLOCK_TYPE_END_ARCHIVE: u64 = 0x05;

// ── General Block Flags ────────────────────────────────────────────────────

pub const BLOCK_FLAG_EXTRA_DATA: u64 = 0x0001;
pub const BLOCK_FLAG_DATA_AREA: u64 = 0x0002;
pub const BLOCK_FLAG_SKIP_IF_UNKNOWN: u64 = 0x0004;
pub const BLOCK_FLAG_DATA_CONTINUES: u64 = 0x0008;
pub const BLOCK_FLAG_DATA_CONTINUE_TO: u64 = 0x0010;
pub const BLOCK_FLAG_DEPENDS_PREV: u64 = 0x0020;
pub const BLOCK_FLAG_PRESERVE_CHILD: u64 = 0x0040;

// ── Archive Header Flags ───────────────────────────────────────────────────

pub const ARCHIVE_FLAG_VOLUME: u64 = 0x0001;
pub const ARCHIVE_FLAG_VOLUME_NUM: u64 = 0x0002;
pub const ARCHIVE_FLAG_SOLID: u64 = 0x0004;
pub const ARCHIVE_FLAG_RECOVERY: u64 = 0x0008;
pub const ARCHIVE_FLAG_LOCKED: u64 = 0x0010;

// ── File Header Flags ──────────────────────────────────────────────────────

pub const FILE_FLAG_DIRECTORY: u64 = 0x0001;
pub const FILE_FLAG_TIME_UNIX: u64 = 0x0002;
pub const FILE_FLAG_CRC32: u64 = 0x0004;
pub const FILE_FLAG_UNKNOWN_SIZE: u64 = 0x0008;

// ── Compression Methods ────────────────────────────────────────────────────

pub const COMP_METHOD_STORE: u8 = 0;
pub const COMP_METHOD_FASTEST: u8 = 1;
pub const COMP_METHOD_FAST: u8 = 2;
pub const COMP_METHOD_NORMAL: u8 = 3;
pub const COMP_METHOD_GOOD: u8 = 4;
pub const COMP_METHOD_BEST: u8 = 5;

pub fn method_name(method: u8) -> &'static str {
    match method {
        0 => "Store",
        1 => "Fastest",
        2 => "Fast",
        3 => "Normal",
        4 => "Good",
        5 => "Best",
        _ => "Unknown",
    }
}

pub fn level_to_method(level: u8) -> u8 {
    level.min(5)
}

// ── Compression Info Field Layout ──────────────────────────────────────────

pub const COMP_INFO_VERSION_MASK: u64 = 0x003F;
pub const COMP_INFO_SOLID_BIT: u64 = 0x0040;
pub const COMP_INFO_METHOD_SHIFT: u32 = 7;
pub const COMP_INFO_METHOD_MASK: u64 = 0x0380;
pub const COMP_INFO_DICT_SHIFT: u32 = 10;
pub const COMP_INFO_DICT_MASK: u64 = 0x3C00;

pub const DEFAULT_DICT_SIZE_LOG: u8 = 17;

// ── Checksum / Hash Types ──────────────────────────────────────────────────

pub const HASH_NONE: u8 = 0x00;
pub const HASH_CRC32: u8 = 0x01;
pub const HASH_BLAKE2: u8 = 0x02;

// ── OS / Platform Identifiers ──────────────────────────────────────────────

pub const OS_WINDOWS: u64 = 0x00;
pub const OS_UNIX: u64 = 0x01;

// ── End-of-Archive Flags ───────────────────────────────────────────────────

pub const END_FLAG_NEXT_VOLUME: u64 = 0x0001;

// ── Extra Area Record Types ────────────────────────────────────────────────

pub const EXTRA_FILE_ENCRYPTION: u64 = 0x01;
pub const EXTRA_FILE_HASH: u64 = 0x02;
pub const EXTRA_FILE_TIME: u64 = 0x03;
pub const EXTRA_FILE_VERSION: u64 = 0x04;
pub const EXTRA_FILE_REDIRECT: u64 = 0x05;
pub const EXTRA_FILE_OWNER: u64 = 0x06;
pub const EXTRA_FILE_SERVICE: u64 = 0x07;

// ── Encryption Parameters ──────────────────────────────────────────────────

pub const ENCR_VERSION_AES256: u8 = 0x00;
pub const ENCR_SALT_SIZE: usize = 16;
pub const ENCR_IV_SIZE: usize = 16;
pub const ENCR_KEY_SIZE: usize = 32;
pub const ENCR_PBKDF2_ITER_LOG: u8 = 15;
