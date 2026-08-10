//! # rar5
//!
//! Pure-Rust RAR5 archive library. Creates, reads, and extracts RAR5 archives
//! with native LZSS+Huffman compression — no external binaries required.
//!
//! ## Quick Start
//!
//! ```no_run
//! use rar5::RarArchive;
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
mod blake2sp;
pub mod codec;
pub mod compression;
pub mod constants;
pub mod encryption;
pub mod error;
pub mod headers;
pub mod options;
pub mod recovery;
pub mod write_progress;
pub use write_progress::{WriteOperation, WriteProgressEvent};
pub mod vint;

pub use archive::{ArchiveEntry, BatchEntry, RarArchive, discover_volumes};
pub use archive::{set_compression_threads, sfx_offset_of};
pub use constants::*;
pub use encryption::EncryptionParams;
pub use error::{RarError, RarResult};
pub use headers::DataChunk;
pub use options::{CreateOptions, ExtractOptions};
pub use recovery::{rebuild_missing_volumes, repair_archive};
