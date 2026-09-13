//! `-mc` filter policy for buffered whole-member encodes.
//!
//! The streaming path applies the same policy inline (its filters are
//! window-based); this module covers the buffered RAR5 member encoder and
//! the parallel batch workers, which previously duplicated the automatic
//! delta-then-x86 order.

use std::sync::atomic::AtomicBool;

use crate::codec::lzss_huff;
use crate::error::RarResult;
use crate::options::{FilterMode, FilterOptions};
use crate::version::ArchiveVersion;

/// Encode one whole member under the `-mc` policy.
///
/// Forced modes run regardless of gain (the caller still compares the
/// packed size against STORE); auto modes keep the existing
/// delta-then-x86 "beats plain LZSS" order; disabled modes are skipped.
pub(super) fn encode_with_filter_policy(
    data: &[u8],
    method: u8,
    dsl: u8,
    variant: ArchiveVersion,
    policy: FilterOptions,
    threads: usize,
    cancel: Option<&AtomicBool>,
) -> RarResult<Option<Vec<u8>>> {
    if data.is_empty() || method == 0 || data.len() > u32::MAX as usize {
        return Ok(None);
    }
    if policy.delta == FilterMode::Forced || policy.x86 == FilterMode::Forced {
        let mut specs = Vec::new();
        if policy.delta == FilterMode::Forced {
            let channels = policy.delta_channels.unwrap_or_else(|| {
                lzss_huff::pick_delta_channel(data, method, dsl, variant)
                    .ok()
                    .flatten()
                    .unwrap_or(1)
            });
            specs.push(lzss_huff::FilterSpec::new(
                lzss_huff::FILTER_DELTA,
                channels,
                0,
                data.len() as u32,
            ));
        }
        if policy.x86 == FilterMode::Forced {
            specs.push(lzss_huff::FilterSpec::new(
                lzss_huff::FILTER_E8E9,
                0,
                0,
                data.len() as u32,
            ));
        }
        return Ok(Some(lzss_huff::encode_with_filters_mt(
            data, method, dsl, &specs, variant, threads, cancel,
        )?));
    }
    if policy.delta != FilterMode::Disabled
        && let Some(filtered) =
            lzss_huff::encode_with_auto_delta_filter(data, method, dsl, variant, threads, cancel)?
    {
        return Ok(Some(filtered));
    }
    if policy.x86 != FilterMode::Disabled
        && let Some(filtered) =
            lzss_huff::encode_with_auto_x86_filter(data, method, dsl, variant, threads, cancel)?
    {
        return Ok(Some(filtered));
    }
    Ok(None)
}
