//! # rar
//!
//! Pure-Rust RAR5 archive library. Creates, reads, and extracts RAR5 archives
//! with native LZSS+Huffman compression — no external binaries required.
//!
//! ## Quick Start
//!
//! ```no_run
//! use rar::RarArchive;
//!
//! // Create an archive
//! let mut rar = RarArchive::create("backup.rar").unwrap();
//! rar.add("src/", 3).unwrap();
//! rar.add_bytes("notes.txt", b"Some notes", 3).unwrap();
//! rar.close().unwrap();
//!
//! // Extract an archive
//! let mut rar = RarArchive::open("backup.rar").unwrap();
//! rar.extract_all("/tmp/output/").unwrap();
//! ```
//!
//! ## License
//!
//! BSD-2-Clause. See LICENSE for details.

pub mod archive;
#[doc(hidden)]
pub mod codec;
#[doc(hidden)]
pub mod crypto;
pub mod detect;
pub mod error;
pub mod features;
mod io_util;
#[doc(hidden)]
pub mod name_policy;
pub mod options;
mod parallel;
pub mod version;
pub mod rar50;
#[doc(hidden)]
pub mod recovery;
mod write_progress;

pub use archive::{ArchiveEntry, BatchEntry, RarArchive, discover_volumes};
pub use codec::rar50::{compress, compress_chunked, compress_with_progress, decompress};
pub use crypto::EncryptionParams;
pub use detect::sfx_offset_of;
pub use features::{Feature, FeatureSet};
pub use version::ArchiveVersion;
pub use error::{RarError, RarResult};
pub use options::{CreateOptions, ExtractOptions};
pub use parallel::{set_compression_threads, set_extraction_threads};
pub use rar50::*;
pub use recovery::{rebuild_missing_volumes, repair_archive};
