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
//! let mut rar = RarArchive::create_with_options("backup.rar", Default::default()).unwrap();
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
pub mod codec;
pub mod crypto;
pub mod detect;
pub mod error;
pub mod features;
mod io_util;
#[doc(hidden)]
pub mod name_policy;
pub mod options;
mod parallel;
pub mod rar50;
#[doc(hidden)]
pub mod recovery;
pub mod version;
mod write_progress;

pub use archive::{ArchiveEntry, BatchEntry, RarArchive, discover_volumes};
// Multi-threaded encoding internals used by the mtbench example and the
// napi binding's streaming path; hidden from the public docs but stable
// enough to build against (feature `parallel` only).
#[doc(hidden)]
#[cfg(feature = "parallel")]
pub use codec::rar50::{EncoderState, encode_chunked_mt};
pub use codec::rar50::{decode, decode_standalone, encode, encode_chunked, encode_with_progress};
pub use crypto::{EncryptionParams, decrypt_data, derive_keys, encrypt_data};
pub use detect::sfx_offset_of;
pub use error::{RarError, RarResult};
pub use features::{Feature, FeatureSet};
pub use options::{CreateOptions, ExtractOptions, SolidReset, parse_dict_size};
pub use parallel::{set_compression_threads, set_extraction_threads};
pub use recovery::{
    rebuild_missing_volumes, rebuild_missing_volumes_with, repair_archive, repair_archive_path,
    repair_archive_path_with,
};
pub use version::ArchiveVersion;
