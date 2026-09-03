//! RAR5/RAR7 (v70) encoding entry points.
//!
//! Re-exports the encoder implementation ([`super::encoder`]) and provides
//! the high-level `encode*` dispatch functions, which normalize the WinRAR
//! compression-method byte (STORE/FASTEST..=BEST) into a call on the
//! underlying raw `encode*_raw` machinery.

pub use super::encoder::*;

use crate::rar50::{COMP_METHOD_BEST, COMP_METHOD_FASTEST, COMP_METHOD_STORE};
use crate::version::ArchiveVersion;

/// Encode `data` using the specified RAR5 compression method.
pub fn encode(data: &[u8], method: u8, dict_size_log: u8) -> Result<Vec<u8>, String> {
    encode_with_progress(data, method, dict_size_log, None)
}

/// Encode `data` using the specified RAR5 compression method, reporting
/// progress as `(bytes_processed, total_bytes)` through `progress`.
pub fn encode_with_progress(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<Vec<u8>, String> {
    if method == COMP_METHOD_STORE {
        if let Some(cb) = progress {
            cb(data.len() as u64, data.len() as u64);
        }
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        return encode_chunked(
            data,
            method,
            dict_size_log,
            DEFAULT_CHUNK_SIZE,
            None,
            true,
            progress,
            ArchiveVersion::Rar50,
        );
    }
    Err(format!("unknown compression method: {method}"))
}

/// Encode `data` in bounded chunks, optionally carrying encoder state
/// across files (solid archives). The symbol table and match finder stay
/// proportional to `chunk_size` instead of the whole file.
///
/// `variant` selects the codec variant: every archive version maps to its
/// own distance code table (`ArchiveVersion::Rar70` → the RAR7 80-entry
/// table instead of the RAR5 64-entry one), set when the member header
/// declares a dictionary above 4 GiB.
#[allow(clippy::too_many_arguments)]
pub fn encode_chunked(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    state: Option<&mut EncoderState>,
    is_final: bool,
    progress: Option<&mut dyn FnMut(u64, u64)>,
    variant: ArchiveVersion,
) -> Result<Vec<u8>, String> {
    if method == COMP_METHOD_STORE {
        if let Some(cb) = progress {
            cb(data.len() as u64, data.len() as u64);
        }
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        return encode_chunked_raw(
            data,
            method,
            dict_size_log,
            chunk_size,
            state,
            is_final,
            progress,
            variant,
        );
    }
    Err(format!("unknown compression method: {method}"))
}
