# 10 — DLL ratio gap: WinRAR's 2-3 byte matches (frequency bootstrap)

Type: task
Status: open — root cause identified, approach tried and rejected (net
negative with our 2-pass pricing)

## Finding

WinRAR's DLL archive (43.06%) vs ours (43.90%, +0.84%) — the remaining
ratio gap. Symbol-level analysis (analyze_stream with the new 8-bucket
len_hist + short_dist):

- WinRAR emits **315 K matches of length 2-3** (130 K len-2, 184 K len-3);
  we emit **zero** (the tree's MIN_MATCH=4 forbids them).
- WinRAR's len-2 matches are almost all at dist < 256 (109 K of 130 K);
  len-3 mostly dist < 4 K (79 K of 184 K).
- WinRAR's literal count 1.83 M vs our 2.35 M — the short matches are how
  WinRAR converts literals to matches.
- Our x86 filter scan covers 99.4% of E8/E9 opcodes (WinRAR 98.6%) — the
  scan is NOT the gap. Feeding WinRAR's exact 203 filter regions through
  our encoder gives 44.13% (worse than our own 43.90%) — the region
  layout is not the gap either. The parse is the gap.

## Why our attempt failed

Tried: a 2-byte-window probe (head2 + prev2 ring over 64 KiB) reporting
len-2/3 candidates at no-candidate positions + pricing lengths 2-3 in the
DP (run_start.max(2)) + distance gates (len-2 dist<256, len-3 dist<4K).

Measured (all ratio-checked, all rejected):
- max(2) alone: DLL +176 B, xml +2.2 K — the DP emits short matches at
  tree-candidate distances where they cost more than literals.
- + distance gates: DLL +176 B (still), xml +2.2 K — the short slots'
  NC codes are ~10-11 bits when rare.
- + optimistic estimate (base 5 for raw<4): DLL +15.7 K, xml +4.5 K —
  committing harder made the distance-table perturbation worse.

Root cause: the short-match slots' Huffman codes depend on their
frequency (a 2-byte match's NC symbol is ~10 bits when rare, ~4 bits when
common like WinRAR's 315 K). WinRAR's parse commits to the short matches
en masse so the slots get short codes — a frequency bootstrap our 2-pass
pricing cannot reproduce safely: pass 1 (flat estimate) picks them, pass 2
prices with pass-1's tables (which reflect the short-match-heavy pass-1
tokens), so the tables and the final tokens stay mismatched, and the
distance-table perturbation from the short distances shifts the 1.1 M
regular matches' costs.

## Status

Reverted the encoder changes; kept the analyzer tooling (8-bucket len_hist,
short_dist, filter_regions) that made the diagnosis possible. The DLL
ratio gap (+0.84%) stays open. Next candidates: a proper pricing model for
the frequency bootstrap (per-slot estimated costs, or a third pass so the
tables converge), or accept the gap and move to speed.
