//! Phase timers for the encoder's hot path.
//!
//! The encoder's cost centres move around: the tree descent, the block
//! collector, the optimal parse's priced DP and the Huffman emission were each
//! blamed for the distance to WinRAR at some point, and guessing which one
//! dominates wasted more time than any of the fixes did. This module records
//! wall time per phase, off unless `RAR_RS_PROFILE` is set, and prints one
//! line per phase when the encoder finishes a member:
//!
//! ```text
//! $ RAR_RS_PROFILE=1 rar a -m3 -mt1 out.rar big.exe
//! PROF parse.total   4612 ms
//! PROF parse.collect 2510 ms (54%)
//! ...
//! ```
//!
//! The timers are per block or per chunk, never per position: the two atomic
//! adds cost nothing at that granularity, and a per-position clock read would
//! distort what it measures.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// `true` when `RAR_RS_PROFILE` is set; cached on first use.
pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RAR_RS_PROFILE").is_some())
}

/// Phase ids, in reporting order.
pub(super) const PARSE_TOTAL: usize = 0;
/// Tree seeding, `grow_to`, `rebase` and long-range pushes.
pub(super) const PARSE_PREP: usize = 1;
/// Block collection: the tree descent plus the short-match tables.
pub(super) const PARSE_COLLECT: usize = 2;
/// The priced shortest-path DP per block.
pub(super) const PARSE_DP: usize = 3;
/// `convert_tokens`: symbol emission and frequency counting.
pub(super) const PARSE_CONVERT: usize = 4;
/// `find_block_end_adaptive` + `encode_block` in the chunk driver.
pub(super) const EMIT: usize = 5;
/// The greedy path at level 1, for completeness.
pub(super) const GREEDY: usize = 6;

const NAMES: [&str; 7] = [
    "total",
    "  prep",
    "  collect",
    "  dp",
    "  convert",
    "emit",
    "greedy",
];

/// Event counters, for attributing time to work rather than guessing.
pub(super) const POS: usize = 0;
/// Full tree descents (each also inserts the position).
pub(super) const SEARCH: usize = 1;
/// 2- and 3-byte table probes.
pub(super) const SHORT_PROBE: usize = 2;
/// Long-range table probes.
pub(super) const LR_PROBE: usize = 3;
/// Length verifications (`match_length_at` calls) of tree reports.
pub(super) const VERIFY: usize = 4;

const COUNT_NAMES: [&str; 5] = ["positions", "searches", "short", "lr", "verify"];

static COUNTS: [AtomicU64; 5] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

static NANOS: [AtomicU64; 7] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Record `n` events of `slot`.
#[inline]
pub(super) fn count(slot: usize, n: u64) {
    if enabled() {
        COUNTS[slot].fetch_add(n, Ordering::Relaxed);
    }
}

/// Add one phase measurement. Cheap enough to call per block.
#[inline]
pub(super) fn add(phase: usize, since: Instant) {
    if enabled() {
        NANOS[phase].fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

/// Print the cumulative counters (they are never cleared, so the last line of
/// a run is the whole run).
pub(super) fn dump() {
    if !enabled() {
        return;
    }
    let total = NANOS[PARSE_TOTAL].load(Ordering::Relaxed) as f64;
    if total == 0.0 {
        return;
    }
    let mut covered = 0f64;
    let mut report = String::new();
    for (index, name) in NAMES.iter().enumerate() {
        if index == PARSE_TOTAL {
            continue;
        }
        let nanos = NANOS[index].load(Ordering::Relaxed) as f64;
        if nanos == 0.0 {
            continue;
        }
        covered += nanos;
        report.push_str(&format!(
            "PROF {name:<10} {:8.1} ms ({:5.1}%)\n",
            nanos / 1e6,
            nanos / total * 100.0
        ));
    }
    eprintln!("PROF {:<10} {:8.1} ms", "total", total / 1e6);
    if total > 0.0 {
        let mut counts = String::new();
        for (slot, name) in COUNT_NAMES.iter().enumerate() {
            let n = COUNTS[slot].load(Ordering::Relaxed);
            if n > 0 {
                counts.push_str(&format!(" {name}={n}"));
            }
        }
        if !counts.is_empty() {
            eprintln!("PROF counts{counts}");
        }
    }
    eprint!("{report}");
    let rest = total - covered;
    if rest.abs() > total * 0.01 {
        eprintln!(
            "PROF {:<10} {:8.1} ms ({:5.1}%)",
            "  other",
            rest / 1e6,
            rest / total * 100.0
        );
    }
}
