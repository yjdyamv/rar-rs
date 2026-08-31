# 06 — multi-threaded solid archives

Type: research
Status: open

## Question

`flush_window` forces solid archives to the sequential path
(`if !chain_solid && work.len() >= MT_MIN && threads > 1`). Solid archives
(backup workloads) get no MT speedup at all.

## Analysis

- `encode_chunked_mt` already carries encoder state across windows (tail +
  shared LR table + reset repeat-distance cache). The solid chain is exactly
  that state carried across members.
- The MT path's documented divergences from sequential (repeat-distance
  cache reset per slice, x86 ratio +8.2%) would carry into solid archives —
  a ratio change, not a correctness one.
- The decoder handles solid members identically regardless of how the
  encoder produced them, so interop is unaffected.

## Open questions

- Is the +8.2% x86 divergence acceptable for solid, or does it need fixing
  first (per-slice repeat cache is the suspect)?
- Does anything in the solid write path (member-boundary state) break under
  MT windows — e.g. filter regions spanning members?
