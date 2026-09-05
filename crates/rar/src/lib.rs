//! # rar5
//!
//! Pure-Rust RAR archive library. Creates, reads, and extracts legacy RAR
//! 1.5–4.x and modern RAR5/RAR7 archives with native compression — no
//! external binaries required.
//!
//! ## Quick Start
//!
//! ```no_run
//! use rar5::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions};
//!
//! // Create an archive
//! let mut writer = ArchiveWriter::create("backup.rar").unwrap();
//! let opts = EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL);
//! writer.add_path("src/", opts).unwrap();
//! writer.add_bytes("notes.txt", b"Some notes", opts).unwrap();
//! writer.finish().unwrap();
//!
//! // Extract an archive
//! let mut reader = ArchiveReader::open("backup.rar").unwrap();
//! reader.extract_all("/tmp/output/").unwrap();
//! ```
//!
//! ## License
//!
//! BSD-2-Clause. See LICENSE for details.

pub mod archive;
pub mod codec;
pub(crate) mod crc32;
pub mod crypto;
pub mod detect;
pub mod error;
pub mod features;
mod fs;
mod model;
// Internal home of the historical `rar40`/`rar50` module trees; the old
// public paths are re-exported below. `rar40` is raw-gated (feature
// `raw`); `rar50` stays public because in-tree wire tests still use it.
#[doc(hidden)]
pub mod format;
#[doc(hidden)]
pub mod name_policy;
pub mod options;
mod parallel;
#[cfg(feature = "raw")]
pub use crate::format::rar4 as rar40;
pub use crate::format::rar5 as rar50;
#[doc(hidden)]
pub mod recovery;
pub mod version;
mod write_progress;

pub use archive::{
    AppendOptions, ArchiveEditor, ArchiveEntry, ArchiveReader, ArchiveWriter, BatchEntry,
    CompressionLevel, DictionarySize, EditOp, EditPlan, EditReport, Entries, EntryId, EntryMatches,
    EntryRef, EntryWriteOptions, OpenOptions, RarArchive, ScanStrategy, SolidMode, ThreadCount,
    VerificationFailure, VerificationReport, WriteEntry, WriteReport, WriterOptions,
    discover_volumes,
};
// Multi-threaded encoding internals used by the mtbench example and the
// napi binding's streaming path; hidden from the public docs but stable
// enough to build against (feature `parallel` only).
pub use codec::lzss_huff::{EncodeOptions, decode, decode_standalone, encode, encode_chunked};
#[doc(hidden)]
#[cfg(feature = "parallel")]
pub use codec::lzss_huff::{EncoderState, encode_chunked_mt};
pub use crypto::{EncryptionParams, decrypt_data, derive_keys, encrypt_data};
pub use detect::sfx_offset_of;
pub use error::{ErrorCode, RarError, RarResult};
pub use features::{Feature, FeatureSet};
pub use options::{CreateOptions, ExtractOptions, SolidReset, parse_dict_size};
pub use parallel::{set_compression_threads, set_extraction_threads};
pub use recovery::{
    rebuild_missing_volumes, rebuild_missing_volumes_with, repair_archive, repair_archive_path,
    repair_archive_path_with, repair_legacy_archive_path,
};
pub use version::ArchiveVersion;
