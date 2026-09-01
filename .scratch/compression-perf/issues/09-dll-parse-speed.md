# 09 — DLL single-thread parse speed (m3): measurements & levers

Type: task
Status: open — measured; one micro-opt landed (dd2d01c), the big lever (a
cache-resident near finder) is designed but not implemented

## Where the time goes (13 MB ntoskrnl, m3 seq, this machine)

Phase timers on the encode path (temporary instrumentation):

- plain path: collect 5.3 s (67%), dp 2.4 s (p1 1.1 + p2 1.3), encode 0.09 s
- x86-filtered path (the user-facing one): collect ~5.3 s, dp ~3.2 s
  (the transform makes the data more repetitive -> longer tree chains)

WinRAR 7.23 does the whole 13 MB in 1.8 s (mt1) / 0.42 s (mt8).

## Root cause of the collect cost

The BT4 descent runs ~5.5 steps per query (12 M queries), each step = one
dependent random read into the multi-MiB son array (~85-100 ns, DRAM):
the descent is latency-bound, not compute-bound. Depth histogram: spike at
depth 0 (10%, empty hash buckets), tail to 32; the floor guard bounds the
depth (newer-child links), so the chain budget (96 at m3) is never the
limit — budget 8 == budget 96 output-wise.

## Measured levers (all ratio-checked)

| change | dll speed | dll ratio | verdict |
|---|---|---|---|
| u64 word compare in descent | -10% (8799→7904) | identical | LANDED (dd2d01c) |
| tree window cap 4 MiB | -27% collect | +0.9% | rejected (ratio) |
| tree window cap 2 MiB | -37% total (1.6x) | +1.9% (plain) / +2.2% (x86f) | rejected (ratio) |
| DP pass 2 at m3 dropped | -1.3 s | +0.47% dll, +1.7% text64 | rejected (ratio) |
| prefetch son/input (T0) | regression ~+15% | identical | rejected (cache pollution) |
| mid-range via LR (cap + LR min_dist) | not built | — | LR full-cadence probes would cost ~4 s |

## The remaining lever: cache-resident near finder (two-tier)

The 32 MiB son array cannot fit the 16 MiB L3 (shared with input etc.).
A separate small hash-chain over the recent ~1-2 MiB (prev ring + head,
~8 MiB total, L3-resident) consulted FIRST; the big tree only as the far
fallback. Consistency analysis: every position must stay reachable for far
matches, so tree insertions cannot simply be skipped for chain-resolved
positions (a far match's source may be a chain-resolved position). Options
to explore: (a) keep the tree insertion but only on the fallback path and
measure the far-match loss; (b) batched re-insertion of chain-expired
positions (no net gain — same descent cost); (c) accept the sampled LR for
the mid-range (probe cadence must be gated hard).

## Note

Ratio is the contract: every candidate speed change was byte-diffed on the
corpora; the tree-window caps and pass reductions all violated it and were
rejected. The DLL ratio itself now wins vs WinRAR (43.90% vs 44.81%
x86-filtered); the speed gap (3.6x mt1, ~7x mt8) is the open item.

## 2026-09-01 follow-up: the levers tried and rejected, and what's left

The session landed hash 20 bits (byte-identical, -4-15%) and adaptive MT
slices (dll mt8 -50%) and then tried several further collect speedups. All
measured on this machine, all rejected because the speed did not pay for the
ratio cost (the plan's ratio contract; the user accepts a modest ratio hit
but these were steep):

- BT4 nice_len early exit (stop descending at a match >= N): at 32 it
  almost never fires on the DLL (matches are 4-15 bytes) so no speed; at 8
  the DLL was -23% at +4% ratio, text64 +3.5% — steep.
- Cache-resident chain near-finder (head + prev ring, the tree as far
  fallback): three configs (T=4 break-first, T=8 break-first, K=16 longest)
  were all net-negative. T=4 gave the DLL -46% but +8.8% ratio (the chain's
  first-match bias loses the longer matches the tree's descent finds) and
  broke the random-data fast path (rand64 15 s + compressed). T=8 held the
  DLL ratio (+0.37%) but gave no speed (the >=8 first-matches are rare) and
  the chain's per-position find overhead slowed rand64 to 10 s.
- Tree window caps (8 MiB, 4 MiB): the L3 is 16 MiB and the working set
  (13 MiB input + son + 4 MiB head + LR) overflows it, so a smaller son
  barely helps; 8 MiB gave ~0 speed at +0.09% ratio, 4 MiB -27% collect at
  +0.9%.
- DP pass-2 dropped at m3 (1 pass): -15% speed but xml +4.8%, text64 +1.7%.

Verdict: the BT4 descent is genuinely DRAM-latency-bound (12 M queries x
~5.5 dependent reads into a 32 MiB son). WinRAR's per-byte parse is ~3x
faster and is likely hand-assembly-tuned; matching it in pure Rust needs
software pipelining (batch several positions' descents so one position's
DRAM read overlaps another's compute) — the designed but unbuilt next lever.
Also on the list: the CLI's ~1 s overhead over the library core (the
batch-wave + nested MT pool nesting), which is pure waste.
