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

## Head-to-head after adaptive blocks + corruption fix (2026-09, m3 seq)

Same corpus, this machine (release). All roundtrips byte-verified; our
archives pass WinRAR `t`, WinRAR archives pass our `t`.

| file | ours packed | winrar packed | winrar archive | verdict |
|---|---|---|---|---|
| text64 | 6058 B | 8696 B (wr_text64.rar) | — | **-30% vs WinRAR** (was +44% worse) |
| dll (x86 filter) | 5751393 B (43.90%) | 5870437 B (44.81%, dll8.rar) | — | **-2% vs WinRAR**; previously CORRUPT (silent tree bug, fixed) |
| mixed20 | 10489643 B (50.02%) | 10492xxx B | — | ≈ / slightly better |
| rand64 | 67118715 B | 67108940 B | — | tie (STORE) |
| xml (1 MB) | 88950 B | 87656 B | — | +1.5% (pre-existing parse gap, not block overhead) |

text64 12681→6058 came from emitted-block merging (see PLAN.md): WinRAR
writes one block per member on stable data; we merge emitted blocks to 4
MiB with local-drift closing. dll's corruption was the persistent tree
wiping its son array on chunk grows + rebase slot misplacement — fixed,
with a byte-verification net and a 129 KB x86 regression fixture.

Remaining gaps: dll single-thread parse speed (~6 s vs WinRAR 1.8 s — ratio
now wins), xml parse (~1.5%), text64 MT ratio (6554 vs 6058, the documented
MT divergence).

## Definitive head-to-head (2026-09-01, fixed CLI, m3, this machine)

After the rar-cli parallel-feature fix (the CLI silently ran single-
threaded when built standalone — see commit "fix(cli): enable the rar5
parallel feature") and the u64 word compare:

| file | ours mt1 | ours mt8 | win mt1 | win mt8 | ours B | win B |
|---|---|---|---|---|---|---|
| text64 | 2275 ms | 846 ms | 927 ms | 390 ms | 6133 | 8771 |
| dll (x86f) | 8094 ms | 3115 ms | 1722 ms | 418 ms | 5751467 | 5640802 |
| mixed20 | 2716 ms | 1032 ms | 1254 ms | 373 ms | 10490667 | 10509703 |
| rand64 | 192 ms | 186 ms | 15836 ms | 2663 ms | 67108940 | 67109030 |

Ratio verdict: text64 **-30%** (win), mixed **-0.2%** (win), rand64 tie,
dll **+0.84%** (43.90% vs 43.06% — the fresh WinRAR run; the earlier
dll8.rar comparison was against a suboptimal WinRAR archive). Speed:
2.2-2.5x behind mt1 (text64/mixed), 4.7x on the dll mt1; mt8 2.2-7.5x.

DLL parse diff (both dict 2^7, m3): ours lit=2345249 / match=1082545 /
filter records=49 / 100 blocks @ 60 KB; WinRAR lit=1833699 /
match=1279456 / filter=203 / 207 blocks @ 28 KB. We emit ~520 K more
literals — the remaining dll ratio gap is the parse + the x86 filter scan
(fewer, larger filter regions vs WinRAR's finer scan).

## Filter members are MT now (2026-08, 2e21d0b)

encode_with_filters_mt: forward transform (unchanged) + windowed MT encode,
filter records lead the first slice. Both auto filters take a threads count.
Root cause found on the way: the MT long-range table is pre-built over the
whole window, so a slice's own positions shadowed the copy source it should
match (a random+exact-distant-copy 32 MiB file compressed to ~100% instead
of 50%). Fixed by keeping the previous occurrence per key (vals2) and
probing with get_before; the sequential encoder is byte-identical (its
newest entry is always before the chunk being parsed).

m3 results: dll 6.7 -> 3.1 s (-mt8, 36 KB smaller), mixed 2.7 -> 1.1 s,
text 0.73 -> 0.39 s. Remaining gap vs WinRAR: per-byte parse speed on dense
binaries (~2-3x slower single-thread) and the ultra-repetitive-text ratio.
