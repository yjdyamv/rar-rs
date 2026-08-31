# rar-rs tracker map — compression performance

Effort: **encoder speed & ratio**. Scope: the optimal-parse encode path
(sequential + MT), the auto delta/x86 filters, and member-level fallbacks.
Decisions so far:

- **Ratio is the contract.** Every speed change must keep packed bytes
  unchanged on the standard corpora (text/mixed/xml/sparse + random), or
  shrink them. Two "fast mode" tunings were rejected/redesigned because
  they worsened text ratio (see 02).
- **Byte-identical fast paths are preferred over heuristics.** The
  matchless-block DP skip (01) is provably identical; the collector fast
  mode (02) is not byte-identical but ratio-neutral by construction
  (gates only on zero-match runs).
- **Measure before optimizing.** The mtprobe/ratiocheck examples are the
  regression gates; a hotspot must be confirmed by probe before any change.

Open frontier (see issues/):

- 04 window-level incompressible skip for MT — biggest remaining speed
  lever on random data; member-level ratio safety is the open question.
- 05 streaming-path auto filters (>64 MiB members never get delta/x86)
- 06 solid archives stay single-threaded (speed gap on backups)

## Fog

- mt1 random 64 MiB (l3) ≈ 1750 ms: collect ≈ 440, encode ≈ 450, dp ≈ 93,
  ~800 unaccounted (fast-path loop, splitter, seeding, LR build, setup).
- Collect is now dominated by the 256-descent pre-fast warmup + recovery
  searches, not the per-position loop.
- The pre-gate for delta (`auto_delta_filter_channels`) is sample-based
  since 45fa1e0; candidates are the PCM frame sizes.

## Completed

- 01 matchless-block DP fast path (resolved, 3cd6b37)
- 02 collector fast mode gating `longest==0` + thresholds 256 (resolved, 3cd6b37)
- 03 delta filter candidate channels + sampled pre-gate (resolved, 45fa1e0)
