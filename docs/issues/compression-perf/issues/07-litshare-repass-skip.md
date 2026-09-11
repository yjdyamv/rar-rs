# 07 — lit-share re-pass skip (rejected)

Type: task
Status: rejected — measured, no win on the target, small ratio cost on dll

## Proposal

`parse_one_block` runs `passes` pricing passes (2-4 at m2-m5). Skip the
re-passes (2..N) when the first pass's token stream is ≥85% literals: the
tables barely move on such blocks, so repricing should be wasted work. The
original motivation was the DLL m3 encode (dense binaries parse ~2-3x slower
per byte than WinRAR single-threaded).

## Measurement (this machine, release, m3/m5 × seq/mt8, real corpora)

- 85% threshold: **dll output changed and grew** 6184409 → 6186068 B (+1659,
  0.027%); xml/sparse bytes changed (same size). text64/mixed/rand64
  byte-identical. Speed: mixed m5 seq 1639→890 ms, rand64 m5 seq 3325→2055,
  sparse m5 seq 605→283 — but **dll unchanged** (the target corpus has
  match-dense x86 blocks, not literal-heavy ones; literal-heavy corpora were
  already covered by the matchless fast path).
- 95% threshold: dll delta shrinks to ±20-40 B (m3 +20, m5 -38, ~0.0005%);
  xml/sparse become byte-identical; speed wins persist on mixed/rand/sparse.
  **dll timing still unchanged** (m3 seq 6187 vs 6052 ms, i.e. the gate never
  fires where it matters).

## Verdict

Rejected. The gate only fires on literal-heavy blocks, which are already the
cheap part of the parse (the matchless fast path and the collector fast mode
cover the all-literal case; the DP cost on a 85-95% literal block is small).
It never fires on the dense-match blocks that actually dominate DLL time, so
it delivers no speed on the target corpus while changing dll bytes
(net +20 B at m3 even at the 95% threshold) — a ratio-contract violation on
a tracked corpus for zero benefit. The DLL hotspot is elsewhere (collect on
dense-match blocks), and the previous probe run's `std::env::var("DBG_DIST")`
per-position probe was a red herring that made the whole tree look 8x slower.

Lesson re-confirmed from map.md: measure before optimizing; a gate that
"obviously can't hurt" still must be byte-diffed on every corpus.
