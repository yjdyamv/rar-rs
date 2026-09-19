//! RAR5 container layer: format constants and definitions.
//!
//! Mirrors the reference layout's `rar50` family module: the container
//! vocabulary (block types, flags, extra-record types) and the block/header
//! structures live here; extraction is in [`crate::format::rar5::extract`], the
//! writer in [`crate::format::rar5::write`].
//!
//! RAR5 archive structure:
//! ```text
//! [Self-Extracting Module (optional)]
//! [Archive Signature]          -- 8 bytes: magic number
//! [Archive Encryption Header]  -- optional
//! [Main Archive Header]        -- archive-level metadata
//! [File Header] [File Data]    -- one per archived file
//! ...
//! [End of Archive Header]      -- marks archive end
//! ```

pub mod blake2sp;
pub(crate) mod extract;
#[doc(hidden)]
pub mod headers;
pub(crate) mod payload;

pub(crate) mod write;

// ── Archive Signature ──────────────────────────────────────────────────────
// The RAR5 magic lives in `crate::detect` (the signature table's single
// owner, next to the RAR4/RAR13 signatures); this re-export keeps the
// family-local path used across the RAR5 modules.
pub(crate) use crate::detect::RAR5_SIGNATURE;

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

// ── Compression Methods ────────────────────────────────────────────────────

// Owned by the codec (the layer that switches on them); re-exported so the
// container keeps one definition. `pub use` rather than an alias chain, so
// this stays the single source for callers under `format::rar5`.
#[allow(unused_imports)]
pub use crate::codec::lzss_huff::{
    COMP_METHOD_BEST, COMP_METHOD_FASTEST, COMP_METHOD_NORMAL, COMP_METHOD_STORE,
};

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

// ── Checksum / Hash Types ──────────────────────────────────────────────────

// ── OS / Platform Identifiers ──────────────────────────────────────────────

#[cfg_attr(not(windows), allow(dead_code))]
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
/// Extra record inside a service block ("service data"): the payload is
/// the service-specific data (recovery percent for "RR", the NTFS stream
/// name for "STM").
pub const EXTRA_SERVICE_SUBDATA: u64 = 0x07;

// ── Resource Limits ───────────────────────────────────────────────────────

/// Upper bound on a block's *declared* data size when the payload has to be
/// buffered in memory to be interpreted (archive comment "CMT", NTFS stream
/// "STM", quick-open "QO").
///
/// `read_block` only validates the header CRC, so a hand-made archive can
/// declare an arbitrarily large data area — a buffer sized straight from that
/// field would abort the process on allocation instead of returning an error.
/// Service payloads are metadata, not member data (member data goes through
/// the caller-configurable `ExtractOptions` limits), so one fixed ceiling
/// covers all of them.
///
/// Owned by the option layer (it is the default of
/// `ExtractOptions::max_metadata_bytes`); re-exported here for the RAR5
/// header parser.
pub(crate) use crate::options::MAX_METADATA_BYTES;

pub mod create;
