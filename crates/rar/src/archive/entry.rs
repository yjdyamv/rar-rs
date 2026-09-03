//! Public archive entry types: the metadata view returned by listing and
//! the input description used by batch addition.

use crate::rar50::headers::{DataChunk, FileHeader};
use crate::rar50::method_name;
use std::path::Path;

/// A single entry in the archive (public API).
#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub header: FileHeader,
    pub chunks: Vec<DataChunk>,
}

/// One entry to add through [`RarArchive::add_batch`].
///
/// Borrowed views only: byte payloads are copied by the library during
/// preparation, and file entries are read (up to the batch member cap)
/// before compression.
#[derive(Debug, Clone, Copy)]
pub enum BatchEntry<'a> {
    /// In-memory payload added under `name`.
    Bytes {
        /// Archive entry name.
        name: &'a str,
        /// Raw member content.
        data: &'a [u8],
        /// Compression level 0..=5.
        level: u8,
    },
    /// File from disk; `name` overrides the archive entry name when set.
    File {
        /// Path on disk.
        path: &'a Path,
        /// Optional archive name override.
        name: Option<&'a str>,
        /// Compression level 0..=5.
        level: u8,
    },
    /// Directory header only (no recursion).
    Directory {
        /// Path on disk.
        path: &'a Path,
        /// Optional archive name override (basename when `None`).
        name: Option<&'a str>,
    },
}

/// A fully prepared member: hashed, filtered/compressed (or STORE) and
/// encrypted, ready to be written in archive order.
#[cfg(feature = "parallel")]
pub(crate) struct PreparedEntry {
    pub(crate) name: String,
    pub(crate) unpacked_size: u64,
    pub(crate) attrs: u64,
    pub(crate) mtime: u32,
    pub(crate) file_crc: u32,
    pub(crate) method: u8,
    pub(crate) dict_size_log: u8,
    pub(crate) dict_size_bytes: Option<u64>,
    pub(crate) extra_data: Vec<u8>,
    pub(crate) stored_hash: Option<[u8; 32]>,
    pub(crate) payload: Vec<u8>,
}

/// Immutable snapshot of the writer settings needed to prepare a member
/// off-thread. `Sync`-safe where `&RarArchive` is not (the progress
/// callback is a `FnMut` trait object).
#[cfg(feature = "parallel")]
pub(crate) struct BatchPrepareCtx<'a> {
    pub(crate) password: Option<&'a str>,
    pub(crate) blake2: bool,
    pub(crate) dict_size_log: Option<u8>,
    pub(crate) dict_size_bytes: Option<u64>,
    pub(crate) force_v70: bool,
    pub(crate) save_ctime: bool,
    pub(crate) save_atime: bool,
    pub(crate) save_mtime: bool,
    pub(crate) save_owner: bool,
    pub(crate) time_precision_seconds: bool,
    /// Compression worker count for this batch (per-file MT slicing).
    pub(crate) threads: usize,
    /// Caller-owned cancellation flag, checked per chunk in the parallel
    /// prepare loop; `None` = never cancelled.
    pub(crate) cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl ArchiveEntry {
    /// Archive member name (forward-slash separated, UTF-8).
    pub fn name(&self) -> &str {
        &self.header.name
    }

    /// Uncompressed size in bytes.
    pub fn size(&self) -> u64 {
        self.header.unpacked_size
    }

    /// On-disk (packed) size in bytes.
    pub fn compressed_size(&self) -> u64 {
        self.header.packed_size
    }

    /// Whether this entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.header.is_directory
    }

    /// CRC32 of the uncompressed content, if present.
    pub fn crc32(&self) -> Option<u32> {
        self.header.crc32_val
    }

    /// Human-readable compression method name ("Store", "Normal", etc.).
    pub fn method_name(&self) -> &'static str {
        method_name(self.header.comp_method)
    }

    /// Numeric compression method (0 = store, 1..=5 = level).
    pub fn method(&self) -> u8 {
        self.header.comp_method
    }

    /// Modification time as a Unix timestamp (seconds since epoch).
    pub fn mtime(&self) -> u32 {
        self.header.mtime
    }

    /// Modification time nanosecond component (`None` when stored at
    /// 1-second precision or when the archive has no FILE_TIME record).
    pub fn mtime_ns(&self) -> Option<u32> {
        self.header.mtime_ns
    }

    /// Creation time (seconds, nanoseconds) from the FILE_TIME extra
    /// record (`None` when absent).
    pub fn ctime(&self) -> Option<(u64, u32)> {
        self.header.ctime
    }

    /// Last access time (seconds, nanoseconds) from the FILE_TIME extra
    /// record (`None` when absent).
    pub fn atime(&self) -> Option<(u64, u32)> {
        self.header.atime
    }

    /// Host OS identifier (0 = Windows, 1 = Unix).
    pub fn host_os(&self) -> u64 {
        self.header.host_os
    }

    /// File attributes (OS-specific).
    pub fn attributes(&self) -> u64 {
        self.header.attributes
    }
}
