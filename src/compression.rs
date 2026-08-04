/// RAR5 compression engine — dispatches to store or native LZSS+Huffman codec.
use crate::codec;
use crate::constants::*;

/// Compress `data` using the specified RAR5 compression method.
pub fn compress(data: &[u8], method: u8, dict_size_log: u8) -> Result<Vec<u8>, String> {
    compress_with_progress(data, method, dict_size_log, None)
}

/// Compress `data` using the specified RAR5 compression method, reporting
/// progress as `(bytes_processed, total_bytes)` through `progress`.
pub fn compress_with_progress(
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
        return compress_chunked(
            data,
            method,
            dict_size_log,
            codec::DEFAULT_CHUNK_SIZE,
            None,
            true,
            progress,
        );
    }
    Err(format!("unknown compression method: {method}"))
}

/// Compress `data` in bounded chunks, optionally carrying encoder state
/// across files (solid archives). The symbol table and match finder stay
/// proportional to `chunk_size` instead of the whole file.
pub fn compress_chunked(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    state: Option<&mut codec::EncoderState>,
    is_final: bool,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<Vec<u8>, String> {
    if method == COMP_METHOD_STORE {
        if let Some(cb) = progress {
            cb(data.len() as u64, data.len() as u64);
        }
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        return codec::encode_chunked(
            data,
            method,
            dict_size_log,
            chunk_size,
            state,
            is_final,
            progress,
        );
    }
    Err(format!("unknown compression method: {method}"))
}

/// Decompress `data` using the specified RAR5 compression method.
pub fn decompress(
    data: &[u8],
    method: u8,
    unpacked_size: u64,
    dict_size_log: u8,
    state: Option<&mut codec::DecoderState>,
) -> Result<Vec<u8>, String> {
    if method == COMP_METHOD_STORE {
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        let result = if let Some(st) = state {
            codec::decode(data, unpacked_size, dict_size_log, Some(st))?
        } else {
            codec::decode_standalone(data, unpacked_size, dict_size_log)?
        };
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
