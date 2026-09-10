//! RAR5/RAR7 (v70) encoding entry points.
//!
//! Re-exports the encoder implementation ([`super::encoder`]) and provides
//! the high-level `encode*` dispatch functions, which normalize the WinRAR
//! compression-method byte (STORE/FASTEST..=BEST) into a call on the
//! underlying raw `encode*_raw` machinery.

#[cfg(feature = "parallel")]
pub(crate) use super::encoder::encode_chunked_mt_with_progress;
pub(crate) use super::encoder::encode_chunked_raw_with_lead;
#[cfg(all(test, feature = "parallel"))]
pub(crate) use super::encoder::set_fast_path_enabled;
pub use super::encoder::{
    DEFAULT_CHUNK_SIZE, EncoderState, FilterSpec, MAX_FILTER_BLOCK_LENGTH, encode_chunked_mt,
    encode_chunked_raw, encode_raw, encode_with_auto_delta_filter, encode_with_auto_x86_filter,
    encode_with_filters, encode_with_filters_mt, encode_with_progress_raw, pick_delta_channel,
};

use crate::error::{RarError, RarResult};
use crate::format::rar5::{
    COMP_METHOD_BEST, COMP_METHOD_FASTEST, COMP_METHOD_NORMAL, COMP_METHOD_STORE,
};
use crate::version::ArchiveVersion;

/// Options controlling compression of one member.
///
/// All fields are optional except [`method`](EncodeOptions::method) and
/// [`dict_size_log`](EncodeOptions::dict_size_log); the rest carry the
/// streaming/solid configuration that `encode_chunked` needs. Construct with
/// [`EncodeOptions::new`], then set extra fields for streaming members.
pub struct EncodeOptions<'a> {
    /// WinRAR compression method byte: 0 = store, 1..=5 = fastest..best.
    pub method: u8,
    /// Dictionary size as log2(size/128KB), 0 = 128KB.
    pub dict_size_log: u8,
    /// Maximum input bytes processed per chunk. Defaults to
    /// [`DEFAULT_CHUNK_SIZE`]; the symbol table and match finder stay
    /// proportional to this instead of the whole file.
    pub chunk_size: usize,
    /// Shared encoder state for solid-chain continuity (`None` for
    /// standalone members).
    pub state: Option<&'a mut EncoderState>,
    /// Whether this call is the last chunk of one member: only the final
    /// chunk's last emitted block carries the end-of-stream flag.
    pub is_final: bool,
    /// Codec variant (`ArchiveVersion::V70` selects the RAR7 80-entry
    /// distance table instead of the RAR5 64-entry one).
    pub variant: ArchiveVersion,
    /// Progress callback, called as `(bytes_processed, total_bytes)`.
    pub progress: Option<&'a mut dyn FnMut(u64, u64)>,
    /// Skip the incompressible-input probe (see
    /// [`encode_chunked`](crate::codec::lzss_huff::encode_chunked)). Callers
    /// that already probed the member themselves set this to avoid paying
    /// for the sample encodes twice.
    pub skip_incompressible_probe: bool,
}

impl<'a> EncodeOptions<'a> {
    /// A single-chunk, standalone, no-progress encode of one member at
    /// `method`/`dict_size_log`. Callers needing streaming or solid state
    /// set the corresponding fields after construction.
    pub fn new(method: u8, dict_size_log: u8) -> Self {
        EncodeOptions {
            method,
            dict_size_log,
            chunk_size: DEFAULT_CHUNK_SIZE,
            state: None,
            is_final: true,
            variant: ArchiveVersion::default(),
            progress: None,
            skip_incompressible_probe: false,
        }
    }
}

impl Default for EncodeOptions<'_> {
    fn default() -> Self {
        EncodeOptions::new(COMP_METHOD_NORMAL, 0)
    }
}

/// Encode `data` into RAR5/RAR7 compressed bytes using `opts`.
pub fn encode(data: &[u8], opts: EncodeOptions<'_>) -> RarResult<Vec<u8>> {
    encode_chunked(data, opts)
}

/// Encode `data` in bounded chunks, optionally carrying encoder state across
/// files (solid archives). Callers fall back to STORE when the result is not
/// smaller than the input.
///
/// Large inputs that sample as incompressible short-circuit to a verbatim
/// copy of the input — identical to what `method == STORE` produces — so
/// callers that would otherwise pay for full match finding plus literal
/// entropy coding only to STORE the member anyway get it for the price of
/// four small sample encodes. The probe runs only when nothing precedes
/// `data`: with a live window ([`EncoderState`]) skipping the encode would
/// discard members that could have matched into it, and mid-member chunks
/// are excluded for the same reason. Callers that already probed the member
/// (the archive write path does, on the file rather than in memory) set
/// [`EncodeOptions::skip_incompressible_probe`] to avoid paying twice.
pub fn encode_chunked(data: &[u8], opts: EncodeOptions<'_>) -> RarResult<Vec<u8>> {
    let EncodeOptions {
        method,
        dict_size_log,
        chunk_size,
        state,
        is_final,
        variant,
        progress,
        skip_incompressible_probe,
    } = opts;
    if method == COMP_METHOD_STORE {
        if let Some(cb) = progress {
            cb(data.len() as u64, data.len() as u64);
        }
        return Ok(data.to_vec());
    }
    if is_final
        && !skip_incompressible_probe
        && data.len() >= crate::codec::common::incompressible::SAMPLE_PROBE_MIN_INPUT
        && state.as_ref().is_none_or(|s| s.is_fresh())
        && crate::codec::common::incompressible::sample_is_incompressible(data, method)
    {
        if let Some(cb) = progress {
            cb(data.len() as u64, data.len() as u64);
        }
        // Nothing upstream can match into this member either way; callers
        // treat a STORE result as "break the solid chain", which `reset`
        // already expresses.
        if let Some(s) = state {
            s.reset();
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
    Err(RarError::Unsupported(format!(
        "unknown compression method: {method}"
    )))
}
