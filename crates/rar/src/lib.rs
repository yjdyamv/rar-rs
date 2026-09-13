//! # rar-rs
//!
//! Pure-Rust RAR archive library. Creates, reads, and extracts legacy RAR
//! 1.5–4.x and modern RAR5/RAR7 archives with native compression — no
//! external binaries required.
//!
//! ## Quick Start
//!
//! ```no_run
//! use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions};
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

// Codec-independent AES-256-CBC / PBKDF2 primitives. The archive layer is
// the only in-crate caller; the public entry points are re-exported through
// [`wire`].
pub(crate) mod crypto;

pub mod detect;
pub mod error;
pub mod features;
mod fs;
mod model;
// Internal home of the `rar4` / `rar5` module trees. Every in-crate path
// goes through `crate::format::…`; the wire-level helpers that external
// tools need are re-exported through [`wire`].
pub(crate) mod format;

pub mod options;
mod parallel;

// Recovery-record and recovery-volume support. The supported entry points
// are re-exported at the crate root and through [`wire`].
pub(crate) mod recovery;

pub mod version;
pub mod wire;
mod write_progress;

pub use archive::{
    AppendOptions, ArchiveEditor, ArchiveEntry, ArchiveReader, ArchiveWriter, BatchEntry,
    CompressionLevel, DictionarySize, EditOp, EditPlan, EditReport, Entries, EntryId, EntryMatches,
    EntryRef, EntryWriteOptions, OpenOptions, ScanStrategy, SolidMode, ThreadCount,
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
pub use detect::sfx_offset_of;
pub use error::{ErrorCode, RarError, RarResult};
pub use features::{Feature, FeatureSet};
pub use options::{ExtractOptions, MarkOfTheWeb, SolidReset, parse_dict_bytes, parse_dict_size};
pub use parallel::{set_compression_threads, set_extraction_threads};
pub use recovery::rev50::{build_recovery_volumes_for_set, plan_recovery_volume_count};
pub use recovery::{
    rebuild_missing_volumes, rebuild_missing_volumes_with, repair_archive, repair_archive_path,
    repair_archive_path_with, repair_legacy_archive_path, repair_legacy_archive_path_with_password,
};
pub use version::ArchiveVersion;
