//! Dictionary sizing (write-side layout policy): WinRAR-compatible `-md`
//! semantics. The STORE fallback probe itself lives in
//! [`crate::codec::common::incompressible`] so that the codec entry points
//! can apply it to bare `encode` calls too.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::error::{RarError, RarResult};

pub(crate) use crate::codec::common::incompressible::{
    SAMPLE_PROBE_HEAD, sample_is_incompressible, sample_is_incompressible_file,
};

/// WinRAR 7.23 dictionary selection for a non-solid member: the requested
/// dictionary (`-md`, or the default 32 MiB at every compression level) is
/// capped at twice the file size rounded down to a power of two, floored at
/// 128 KiB, and clamped to the RAR5 range (128 KiB .. 4 GiB, log 0..15).
fn dict_log_for(data_size: usize, requested: Option<u8>, _level: u8) -> u8 {
    let base = 128 * 1024;
    let file_pow2 = 1usize << (usize::BITS - 1 - data_size.max(1).leading_zeros());
    let auto_cap = (file_pow2 * 2).max(base);
    let requested_bytes = requested.map_or(32 * 1024 * 1024, |log| base << log);
    let target = auto_cap.min(requested_bytes);
    let mut log = 0u8;
    while (base << log) < target && log < 15 {
        log += 1;
    }
    log
}

/// WinRAR 7.23 dictionary selection for one member, covering both RAR5
/// (v50) and RAR7 (v70) creation. Returns `(encoder_window_log,
/// header_dict_bytes)`:
///
/// - `header_dict_bytes = None`: a plain RAR5 member; the log drives both
///   the header `comp_dict_size` field and the encoder window.
/// - `header_dict_bytes = Some(b)`: a RAR7 member whose header declares an
///   actual dictionary of `b` bytes (possibly not a power of two, WinRAR's
///   `-md` above 4 GiB). The encoder window stays bounded — match
///   distances are chunk-limited anyway — only the header declares the
///   large dictionary.
///
/// Like WinRAR, a > 4 GiB request is still capped at twice the file size
/// rounded down to a power of two; when the cap lands in the RAR5 range
/// the member is written as plain v50 with the capped log. `force_v70`
/// overrides that downgrade (the format allows v70 with any dictionary;
/// it is the test seam that runs the v70 paths at small scale).
pub(crate) fn dict_params_for(
    data_size: usize,
    requested_log: Option<u8>,
    requested_bytes: Option<u64>,
    level: u8,
    force_v70: bool,
) -> (u8, Option<u64>) {
    let Some(requested) = requested_bytes else {
        return (dict_log_for(data_size, requested_log, level), None);
    };
    let base = 128 * 1024;
    let file_pow2 = 1usize << (usize::BITS - 1 - data_size.max(1).leading_zeros());
    let auto_cap = file_pow2.saturating_mul(2).max(base);
    let capped = (requested as usize).min(auto_cap);
    if force_v70 || capped as u64 > 4 * 1024 * 1024 * 1024u64 {
        // RAR7 (v70): the header declares the (capped) dictionary — floored
        // at 128 KiB, the smallest the 5+5-bit encoding can represent. The
        // encoder window follows the plain RAR5 selection rules but is
        // clamped to the declared dictionary: the decoder window IS the
        // declared dict, so emitting a longer distance would be
        // undecodable. (The > 4 GiB path never tripped this because the
        // RAR5 log ceiling of 4 GiB is already below any v70 dict; the
        // clamp matters for force_v70's small dicts.)
        let declared = capped.max(base) as u64;
        let window_log = dict_log_for(data_size, requested_log, level)
            .min((63 - (declared / base as u64).leading_zeros()) as u8);
        (window_log, Some(declared))
    } else {
        // The 2x-file-size cap fell into the RAR5 range: plain v50 member
        // with the capped dictionary.
        let mut log = 0u8;
        while (base << log) < capped && log < 15 {
            log += 1;
        }
        (log, None)
    }
}

/// Compute the plaintext CRC32 (and optional BLAKE2sp) of a file in a
/// single streaming pass.
pub(crate) fn hash_file(
    path: &Path,
    size: u64,
    want_blake: bool,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> RarResult<(u32, Option<[u8; 32]>)> {
    let mut crc = crc32fast::Hasher::new();
    let mut blake = want_blake.then(crate::format::rar5::blake2sp::Hasher::new);
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        crc.update(&buf[..n]);
        if let Some(h) = blake.as_mut() {
            h.update(&buf[..n]);
        }
    }
    if total != size {
        return Err(RarError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("file changed size while being hashed: expected {size} bytes, read {total}"),
        )));
    }
    Ok((crc.finalize(), blake.map(|h| h.finalize())))
}
