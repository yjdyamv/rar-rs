//! Public archive entry types: the metadata view returned by listing and
//! the input description used by batch addition.

#[cfg(feature = "parallel")]
use super::MemberPlan;
use crate::codec::lzss_huff::method_name;
use crate::model::{DataChunk, FileHeader};
use crate::version::ArchiveVersion;
use std::path::Path;

/// A single entry in the archive (public API).
///
/// The underlying header and chunk-list fields are `pub(crate)`: external
/// code reads metadata through the curated accessors rather than reaching
/// into the wire representation, so the archive layer is free to change how
/// a member's data is located.
#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub(crate) header: FileHeader,
    pub(crate) chunks: Vec<DataChunk>,
}

/// One entry to add through the batch paths: the facade's
/// [`ArchiveWriter::add_batch`](crate::ArchiveWriter::add_batch) accepts
/// [`WriteEntry`](crate::WriteEntry) values and converts them into this type.
///
/// Borrowed views only: byte payloads are copied by the library during
/// preparation, and file entries are read (up to the batch member cap)
/// before compression.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
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

/// A fully prepared member: the emission plan plus its packed (and
/// encrypted) payload, ready to be written in archive order.
#[cfg(feature = "parallel")]
pub(crate) struct PreparedEntry {
    pub(crate) plan: MemberPlan,
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
    /// Compression filter policy (`-mc`) for this batch.
    pub(crate) filters: crate::options::FilterOptions,
    pub(crate) save_ctime: bool,
    pub(crate) save_atime: bool,
    pub(crate) save_mtime: bool,
    pub(crate) save_owner: bool,
    pub(crate) time_precision_seconds: bool,
    /// Compression worker count for this batch (per-file MT slicing).
    pub(crate) threads: usize,
    /// Members in this wave. The wave already runs one member per worker, so
    /// a member only has spare workers to hand its MT slices to when the wave
    /// is smaller than `threads`; slicing anyway nests MT in a saturated pool.
    pub(crate) wave_len: usize,
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

    /// Modification time in seconds.
    ///
    /// RAR5 members carry a Unix timestamp (seconds since epoch). For
    /// RAR 1.3–4.x the header stores DOS local wall-clock time, and the
    /// catalog preserves it as civil seconds (the stored date/time fields
    /// interpreted as UTC), so consume it as an instant only after
    /// applying `format::shared::legacy_time::local_civil_to_epoch` —
    /// extraction does exactly that (see `apply_member_times`).
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

    /// Host OS identifier on the shared axis (0 = Windows, 1 = Unix).
    /// RAR 1.5–4.x transmit the raw DOS/OS2/Win32/Unix/Mac code; it is
    /// normalized here (the raw byte stays available through
    /// [`Self::host_os_raw`] and [`Self::host_os_name`]).
    pub fn host_os(&self) -> u64 {
        match self.header.format_version {
            3 | 4 => match self.header.host_os {
                0 | 2 => 0,
                _ => 1,
            },
            _ => self.header.host_os,
        }
    }

    /// The raw host code from the header (RAR4's DOS/OS2/Win32/Unix/Mac
    /// table, RAR5's 0 = Windows / 1 = Unix).
    pub fn host_os_raw(&self) -> u64 {
        self.header.host_os
    }

    /// Display name for the `Host OS:` column, matching WinRAR.
    pub fn host_os_name(&self) -> &'static str {
        match self.header.format_version {
            4 => match self.header.host_os {
                0 => "DOS",
                1 => "OS/2",
                2 => "Windows",
                3 => "Unix",
                _ => "Mac",
            },
            _ => {
                if self.header.host_os == 1 {
                    "Unix"
                } else {
                    "Windows"
                }
            }
        }
    }

    /// File attributes (OS-specific).
    pub fn attributes(&self) -> u64 {
        self.header.attributes
    }

    /// Codec version this member was compressed with: `0` = RAR5 (v50),
    /// `1` = RAR7 (v70).
    pub fn comp_version(&self) -> u8 {
        self.header.comp_version
    }

    /// The member compression version in the unified version table. Legacy
    /// RAR 1.5–4.x members map their `unp_ver` (`15`/`20`/`26`/`29`/`36`)
    /// onto [`ArchiveVersion`]; RAR5 members map `comp_version` to v50/v70.
    pub fn version(&self) -> ArchiveVersion {
        match self.header.format_version {
            3 => ArchiveVersion::V14,
            4 => ArchiveVersion::from_unp_ver(self.header.unp_ver).unwrap_or(ArchiveVersion::V29),
            _ => ArchiveVersion::from_v70(self.header.comp_version == 1),
        }
    }

    /// Dictionary setting: `log2(size/128KiB)` for RAR5 members (RAR7
    /// members carry the byte count via
    /// [`dict_size_bytes`](Self::dict_size_bytes)), or the RAR 1.5–4.x
    /// window-bits field (`log2(window/64KiB)`, 7 = directory).
    pub fn comp_dict_size(&self) -> u8 {
        self.header.comp_dict_size
    }

    /// Actual dictionary size in bytes for RAR7 members, `None` for RAR5.
    pub fn dict_size_bytes(&self) -> Option<u64> {
        self.header.dict_size_bytes
    }

    /// Owner record (OWNER extra) from the file header, when present.
    pub fn owner(&self) -> Option<&str> {
        self.header.owner.as_deref()
    }

    /// Whether this member is part of a solid chain (shares the LZ window
    /// with its neighbours).
    pub fn comp_solid(&self) -> bool {
        self.header.comp_solid
    }

    /// Byte offset of this member's packed data within its volume (the
    /// [`data_offset`](Self::data_offset) of the first data chunk).
    pub fn data_offset(&self) -> u64 {
        self.header.data_offset
    }

    /// Read-only view of the member's data chunks (one per volume for
    /// multi-volume members).
    pub fn chunks(&self) -> &[DataChunk] {
        &self.chunks
    }

    /// Per-member (file) comment for RAR 3.x/4.x archives (a `COMM_HEAD`
    /// block after the member data; nested in the header for RAR 1.5–2.9),
    /// if the member carries one. Returns raw text bytes (UTF-8 when the
    /// comment was ASCII, UTF-16LE decoded otherwise).
    pub fn comment(&self) -> Option<&[u8]> {
        self.header.comment.as_deref()
    }
}
