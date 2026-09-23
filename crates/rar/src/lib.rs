#![warn(missing_docs)]

//! # rar-rs
//!
//! Pure-Rust RAR archive library. Creates, reads, and extracts legacy RAR
//! 1.3–4.x and modern RAR5/RAR7 archives with native compression — no
//! external binaries required. Creation covers RAR 1.3/1.4 (`RE~^`,
//! `ArchiveVersion::V14`), RAR 1.5/2.x/4.x and RAR5/RAR7.
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
//! The project's own contributions are BSD-2-Clause (see LICENSE). Portions
//! ported from other projects keep their own terms; see NOTICE and
//! THIRD_PARTY_LICENSES.md.

// Role facades ([`ArchiveReader`] / [`ArchiveWriter`] / [`ArchiveEditor`]) and
// the crate-internal `RarArchive` engine behind them (ADR 0006: the engine is
// not part of the public surface). The facades are re-exported at the crate
// root; nothing else in here is reachable from outside.
pub(crate) mod archive;
pub mod codec;
pub(crate) mod crc32;

// Codec-independent AES-256-CBC / PBKDF2 primitives. The archive layer is
// the only in-crate caller; the public entry points are re-exported through
// [`wire`].
pub(crate) mod crypto;

pub mod detect;
pub mod error;
// State, entry types and limits shared by the engine (`archive`) and the
// per-family container code (`format`). Kept below both so neither has to
// depend on the other.
mod engine;
pub mod features;
mod fs;
// Generic `std::io` helpers owned by no single layer, so that the public
// `codec` never has to reach into `fs` for one loop.
mod io_util;
mod model;
// Internal home of the `rar4` / `rar5` module trees. Every in-crate path
// goes through `crate::format::…`; the wire-level helpers that external
// tools need are re-exported through [`wire`].
pub(crate) mod format;

pub mod options;
mod parallel;
// The running platform's RAR5 metadata style (host marker, attribute bits,
// time form). A leaf named by both `engine` and `format`, like `time`.
pub(crate) mod platform;

// Legacy civil-time primitives (DOS/local-wall-clock conversions). Public
// because the CLI's `-ts` handling needs the same math; see `time.rs`.
pub mod time;

// Recovery-record and recovery-volume support. The supported entry points
// are re-exported at the crate root and through [`wire`].
pub(crate) mod recovery;

pub mod version;
// Varint coding is a wire-level primitive shared by the RAR5 container, the
// legacy edit paths and the crypto layer, so it lives at the crate root rather
// than under any one family (publicly reachable through `wire`).
#[doc(hidden)]
pub mod vint;
pub mod wire;
mod write_progress;

pub use archive::{
    AppendOptions, ArchiveEditor, ArchiveEntry, ArchiveReader, ArchiveWriter, BatchEntry,
    CompressionLevel, EditOp, EditPlan, EditReport, Entries, EntryId, EntryMatches, EntryRef,
    EntryWriteOptions, ExtractionReport, OpenOptions, ReconstructReport, ScanStrategy, SolidMode,
    ThreadCount, VerificationFailure, VerificationReport, WriteEntry, WriteReport, WriterOptions,
    discover_volumes, reconstruct_archive_path,
};
// Root re-exports of the public codec surface; the full item set lives at
// `codec::lzss_huff`. The `parallel`-gated MT internals (`EncoderState`,
// `encode_chunked_mt`) are hidden from the public docs but kept stable for
// the benchmark examples (`mtbench`, `mtwin`, `ratiocheck`, …).
pub use codec::lzss_huff::{EncodeOptions, decode, decode_standalone, encode, encode_chunked};
#[doc(hidden)]
#[cfg(feature = "parallel")]
pub use codec::lzss_huff::{EncoderState, encode_chunked_mt};
pub use detect::sfx_offset_of;
pub use error::{ErrorCode, RarError, RarResult};
pub use features::{Feature, FeatureSet};
pub use fs::atomic::StagedCopy;
pub use options::{
    DictionarySize, ExtractOptions, FilterMode, FilterOptions, MarkOfTheWeb, OverwriteChoice,
    OverwritePrompt, SolidReset, parse_dict_bytes, parse_dict_size,
};
pub use parallel::{set_compression_threads, set_extraction_threads};
pub use recovery::rev50::{build_recovery_volumes_for_set, plan_recovery_volume_count};
pub use recovery::{
    LegacyDamagedSector, LegacyRepair, rebuild_missing_volumes, rebuild_missing_volumes_with,
    repair_archive, repair_archive_path, repair_archive_path_with, repair_legacy_archive_path,
    repair_legacy_archive_path_with_password,
};
pub use version::ArchiveVersion;
