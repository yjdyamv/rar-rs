//! RAR5 extraction: opening, block scanning, listing and member decoding.
//!
//! Mirrors the reference layout's `rar50/extract.rs`: every read-side
//! operation on [RarArchive] lives here while the shared state definition
//! stays in the facade (`crate::archive`).
//!
//! Role split:
//! - [`open`] — open/quick-open, signature verification and block scanning,
//! - [`read`] — reads and `test`,
//! - [`members`] — whole-archive and single-member extraction,
//! - [`dest`] — destinations: streams, timestamps, redirects, safe paths,
//! - [`solid`] — solid-chain decode drivers (RAR5 and RAR4),
//! - [`decode`] — packed-data assembly and member decoding,
//! - [`verify`] — integrity verification of decoded members.

mod decode;
mod dest;
mod members;
mod open;
mod read;
mod solid;
mod verify;

use crate::error::{RarError, RarResult};
use crate::model::FileHeader;

/// The dictionary size a member declares, after enforcing the extraction cap
/// (`ExtractOptions::max_dict_size`, WinRAR's `-mdx`).
///
/// RAR5 uses `128 KiB << comp_dict_size`; RAR7 carries the byte count
/// directly, and a hostile header can push that to a multi-TiB value, so
/// every decode entry point must go through this before allocating.
pub(super) fn capped_dict_bytes(hdr: &FileHeader, max_dict_size: Option<u64>) -> RarResult<u64> {
    let bytes = match hdr.dict_size_bytes {
        Some(bytes) => bytes,
        None => (128u64 * 1024) << hdr.comp_dict_size,
    };
    if let Some(cap) = max_dict_size
        && bytes > cap
    {
        return Err(RarError::LimitExceeded {
            limit: cap,
            context: format!(
                "{}: dictionary size {bytes} bytes exceeds the extraction cap (use -mdx to raise it)",
                hdr.name
            ),
        });
    }
    Ok(bytes)
}
