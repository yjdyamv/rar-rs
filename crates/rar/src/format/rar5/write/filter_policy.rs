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

/// The `-mc` filter records for one member, without encoding it.
///
/// The buffered member path and the solid path differ only in *how* they
/// encode the transformed bytes (a fresh member versus a continued chain), so
/// they share the decision: same per-64 KiB-block geometry, same
/// [`lzss_huff::best_combined_specs`] cover estimate, same margins.
///
/// `None` means "this policy has no per-block record form": forced modes and
/// the delta/x86 single-transform policies keep their own pickers, and
/// `RAR_RS_FILTER_COMBINED=0` restores them for experiments.
pub(super) fn combined_filter_specs(
    data: &[u8],
    method: u8,
    dsl: u8,
    variant: ArchiveVersion,
    policy: FilterOptions,
) -> Option<Vec<lzss_huff::FilterSpec>> {
    if policy.delta != FilterMode::Auto || policy.x86 != FilterMode::Auto {
        return None;
    }
    let combined = std::env::var("RAR_RS_FILTER_COMBINED")
        .map(|value| value != "0")
        .unwrap_or(true);
    if !combined {
        return None;
    }
    // Trial encodes run at the cheapest method on measured grounds: on
    // ntoskrnl m3 an m1 trial packed 5,789,172 bytes while a method-3 trial
    // managed 5,850,197 (at m3 "plain" wins more often, so the trial
    // over-picks no-filter), and a full-method trial also costs a second full
    // compression (~1.2 s here). The trial is only a ranking signal, and the
    // cheap one ranks better; `RAR_RS_TRIAL_METHOD` overrides it.
    let trial_method = std::env::var("RAR_RS_TRIAL_METHOD")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|&value| (1..=5).contains(&value))
        .unwrap_or(1);
    let specs = lzss_huff::best_combined_specs(
        data,
        method,
        dsl,
        variant,
        None,
        lzss_huff::FILTER_E8E9,
        trial_method,
    );
    (!specs.is_empty()).then_some(specs)
}

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
    if policy.delta == FilterMode::Forced && policy.x86 == FilterMode::Forced {
        // `-mcd+ -mce+` forces both transforms over the same bytes, but
        // RARLAB readers reject overlapping filter records. WinRAR splits the
        // member into 64 KiB blocks and emits one non-overlapping filter per
        // block; pick the same way instead of refusing the combination.
        let channels = policy.delta_channels.unwrap_or_else(|| {
            lzss_huff::pick_delta_channel(data, method, dsl, variant)
                .ok()
                .flatten()
                .unwrap_or(1)
        });
        let specs = lzss_huff::forced_combined_specs(
            data,
            method,
            dsl,
            variant,
            channels,
            lzss_huff::FILTER_E8E9,
        );
        return Ok(Some(lzss_huff::encode_with_filters_mt(
            data, method, dsl, &specs, variant, threads, cancel,
        )?));
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
    if policy.delta == FilterMode::Auto && policy.x86 == FilterMode::Auto {
        // Default `-mc`: WinRAR's forced-combined geometry — one filter per
        // 64 KiB block, delta vs x86 chosen by trial encode, and no record
        // at all where plain LZSS stays smallest.
        let combined = std::env::var("RAR_RS_FILTER_COMBINED")
            .map(|v| v != "0")
            .unwrap_or(true);
        if combined {
            let clock = std::env::var_os("RAR_RS_MTTIME").is_some();
            let pick_started = std::time::Instant::now();
            // Speed-first default: the per-block scan measures 213 blocks x 13
            // candidate channels and costs 44-58 ms of a member's wall time, but
            // measured it buys only 0.63 pp on a kernel image and nothing at all
            // on Rust sources or markdown — the x86 filter wins nearly every
            // block anyway. The fast rungs (`-m1`..`-m3`) emit one whole-member
            // E8E9 record instead; `-m4`/`-m5` keep the scan. The user pinned
            // `-m3` at the fastest point, so speed wins here;
            // `RAR_RS_FILTER_SCAN=1` buys the 0.63 pp back for ~30 ms.
            let scan = match std::env::var("RAR_RS_FILTER_SCAN") {
                Ok(value) => value != "0",
                Err(_) => method >= 4,
            };
            let specs = if scan {
                // Trial encodes run at the cheapest method on measured grounds:
                // on ntoskrnl m3 an m1 trial packed 5,789,172 bytes while a
                // method-3 trial managed 5,850,197 (at m3 "plain" wins more
                // often, so the trial over-picks no-filter), and a full-method
                // trial also costs a second full compression (~1.2 s here).
                // `RAR_RS_TRIAL_METHOD` overrides it for experiments.
                let trial_method = std::env::var("RAR_RS_TRIAL_METHOD")
                    .ok()
                    .and_then(|v| v.parse::<u8>().ok())
                    .filter(|&m| (1..=5).contains(&m))
                    .unwrap_or(1);
                lzss_huff::best_combined_specs(
                    data,
                    method,
                    dsl,
                    variant,
                    None,
                    lzss_huff::FILTER_E8E9,
                    trial_method,
                )
            } else {
                vec![lzss_huff::FilterSpec::new(
                    lzss_huff::FILTER_E8E9,
                    0,
                    0,
                    data.len() as u32,
                )]
            };
            if clock {
                eprintln!(
                    "MTTIME filter_pick={}ms specs={}",
                    pick_started.elapsed().as_millis(),
                    specs.len()
                );
            }
            if specs.is_empty() {
                return Ok(None);
            }
            let encode_started = std::time::Instant::now();
            let packed = lzss_huff::encode_with_filters_mt(
                data, method, dsl, &specs, variant, threads, cancel,
            )?;
            if clock {
                eprintln!(
                    "MTTIME member_encode={}ms",
                    encode_started.elapsed().as_millis()
                );
            }
            return Ok(Some(packed));
        }
    }
    if policy.delta != FilterMode::Disabled
        && let Some(channels) = lzss_huff::pick_delta_channel(data, method, dsl, variant)?
        && lzss_huff::filter_transform_wins(
            &lzss_huff::member_filter_probes(data),
            method,
            dsl,
            variant,
            |bytes, offset| lzss_huff::delta_stream_window(bytes, offset, channels).0,
        )?
        .is_some()
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
