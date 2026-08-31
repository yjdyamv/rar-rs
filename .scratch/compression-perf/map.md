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

## Real head-to-head vs WinRAR 7.23 (2026-08, m3)

Corpus: text64 (68 MB repetitive text), rand64, mixed20 (text+random+text),
dll (ntoskrnl.exe 13 MB). Ours built with rar5/parallel; `-mt<N>` honored
via normalize_switch (`-mt8` → `--threads=8`).

| file | ours mt1 | ours mt8 | winrar mt1 | winrar mt8 | ratio ours | ratio win |
|---|---|---|---|---|---|---|
| text64 | 2.2 s | 0.86 s | 0.91 s | 0.39 s | 12681 B | 8769 B |
| rand64 | 0.19 s (STORE probe) | — | 15.8 s | 2.7 s | 100.0% | 100.0% |
| mixed20 | 2.7 s | 2.7 s (filter seq) | 1.3 s | 0.39 s | 50.03% | 50.11% |
| dll13 | 6.5 s | 6.5 s (filter seq) | 1.8 s | 0.42 s | 45.08% | 43.06% |

Findings: (1) filter members (x86) are strictly sequential — mixed/dll get
no MT benefit; the single filtered m3 encode of 13 MB dense binary is ~2
MiB/s vs WinRAR ~7 MiB/s. (2) MT scaling is healthy on unfiltered files
(2-64 MiB MT landed 7d7008c; text 19.5 MB 711→369 ms). (3) ultra-repetitive
text ratio gap (12681 vs 8769). (4) incompressible probe keeps us 10-80x
faster than WinRAR on random data.
