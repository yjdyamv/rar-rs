//! RAR5/RAR7 (v70) decoding entry point.
//!
//! Re-exports the decoder implementation ([`super::decoder`]) and provides
//! the high-level `decompress` dispatch function, which normalizes the
//! WinRAR compression-method byte (STORE/FASTEST..=BEST) into a call on the
//! underlying `decode*` machinery.

pub use super::decoder::*;

use crate::rar50::{COMP_METHOD_BEST, COMP_METHOD_FASTEST, COMP_METHOD_STORE};

/// Decompress `data` using the specified RAR5 compression method.
pub fn decompress(
    data: &[u8],
    method: u8,
    unpacked_size: u64,
    dict_size_log: u8,
    state: Option<&mut DecoderState>,
) -> Result<Vec<u8>, String> {
    if method == COMP_METHOD_STORE {
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        let result = decode(
            data,
            unpacked_size,
            DecodeOptions {
                dict_size_log,
                state,
                ..Default::default()
            },
        )?;
        if result.len() != unpacked_size as usize {
            return Err(format!(
                "decompressed size mismatch: expected {unpacked_size}, got {}",
                result.len()
            ));
        }
        return Ok(result);
    }
    Err(format!("unknown compression method: {method}"))
}
