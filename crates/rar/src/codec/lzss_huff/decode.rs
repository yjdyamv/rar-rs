//! RAR5/RAR7 (v70) decoding entry point.
//!
//! Re-exports the decoder implementation ([`super::decoder`]) and provides
//! the high-level `decode` dispatch function, which normalizes the
//! WinRAR compression-method byte (STORE/FASTEST..=BEST) into a call on the
//! underlying raw `decode_raw` machinery.

pub use super::decoder::*;

use crate::error::{RarError, RarResult};
use crate::rar50::{COMP_METHOD_BEST, COMP_METHOD_FASTEST, COMP_METHOD_STORE};

/// Decode `data` using the specified RAR5 compression method.
pub fn decode(
    data: &[u8],
    method: u8,
    unpacked_size: u64,
    dict_size_log: u8,
    state: Option<&mut DecoderState>,
) -> RarResult<Vec<u8>> {
    if method == COMP_METHOD_STORE {
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        let result = decode_raw(
            data,
            unpacked_size,
            DecodeOptions {
                dict_size_log,
                state,
                ..Default::default()
            },
        )
        .map_err(RarError::Format)?;
        if result.len() != unpacked_size as usize {
            return Err(RarError::Format(format!(
                "decoded size mismatch: expected {unpacked_size}, got {}",
                result.len()
            )));
        }
        return Ok(result);
    }
    Err(RarError::Unsupported(format!(
        "unknown compression method: {method}"
    )))
}
