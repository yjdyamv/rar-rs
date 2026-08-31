# 01 — matchless-block DP fast path

Type: task
Status: resolved

## Question

On incompressible data the optimal parse runs 2-4 pricing passes per block
over positions that have no match candidates at all — the result is
deterministically all literals, so the passes (and their ~2 MiB of per-call
arrays) are pure waste. Can the parse skip them while staying byte-identical?

## Answer

Yes. In `parse_one_block`, when the collector returns zero candidates
(`matches.runs.is_empty()`), a block parses to pure literals. The only thing
the pricing passes could find that the collector (a heuristic tree) missed is
an exact byte-match at a cached repeat distance, so the fast path re-checks
the two repeat probes (a couple of byte compares per position) and, if clean,
emits the all-literal symbols directly. Byte-exactness is locked by the
`matchless_fast_path_is_byte_identical` test (toggle seam + 5 corpora × 4
level/dict/extra variants).

Measured on 64 MiB random m3: DP CPU 6512 → 93 ms (mt8 accumulated); mt1
5044 → 1751 ms, mt8 1253 → 486 ms combined with 02. Ratio 100.02% → 100.01%.

Context: 3cd6b37.
