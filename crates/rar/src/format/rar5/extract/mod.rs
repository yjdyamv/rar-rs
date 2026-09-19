//! RAR5 read path: block scanning, quick-open resolution and member
//! decoding.
//!
//! Role split:
//! - [`open`] — RAR5 full/quick scan and quick-open parsing,
//! - [`solid`] — RAR5 solid-chain decode driver,
//! - [`decode`] — packed-data assembly and member decoding,
//! - [`verify`] — integrity verification of decoded members.
//!
//! The family-neutral read orchestration (opening across families,
//! extraction, destinations) lives in [`crate::format::shared::extract`].

pub(crate) mod decode;
pub(crate) mod open;
pub(crate) mod solid;
pub(crate) mod verify;

pub(crate) use decode::read_streams_with;
#[cfg(feature = "parallel")]
pub(crate) use verify::verify_integrity_for;

use crate::error::{RarError, RarResult};
use crate::model::FileHeader;

/// The dictionary size a member declares, after enforcing the extraction cap
/// (`ExtractOptions::max_dict_size`, WinRAR's `-mdx`).
///
/// RAR5 uses `128 KiB << comp_dict_size`; RAR7 carries the byte count
/// directly, and a hostile header can push that to a multi-TiB value, so
/// every decode entry point must go through this before allocating.
pub(crate) fn capped_dict_bytes(hdr: &FileHeader, max_dict_size: Option<u64>) -> RarResult<u64> {
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
