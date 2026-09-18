# rar-rs tracker map — compression performance

Effort: **encoder speed & ratio**. Scope: the optimal-parse encode path
(sequential + MT), the auto delta/x86 filters, and member-level fallbacks.
Decisions so far:

- **Ratio is the contract.** Every speed change must keep packed bytes unchanged
  on the standard corpora (text/mixed/xml/sparse + random), or shrink them. Two
  "fast mode" tunings were rejected/redesigned because they worsened text ratio
  (see 02).
- **Byte-identical fast paths are preferred over heuristics.** The
  matchless-block DP skip (01) is provably identical; the collector fast mode
  (02) is not byte-identical but ratio-neutral by construction (gates only on
  zero-match runs).
- **Measure before optimizing.** The mtprobe/ratiocheck examples are the
  regression gates; a hotspot must be confirmed by probe before any change.
- **Emitted-block policy has one owner (2026-09).** `EMITTED_BLOCK_SIZE` +
  `find_block_end_adaptive` live in `parse.rs`; the sequential filtered path was
  the last user of the old fixed 128 KiB splitter. Ratio effect: a forced E8
  member shrinks (8 MiB of a repeated byte: 837,559 → 836,590 B, -0.12%); MT
  unchanged. Bytes shrink, never grow — the ratio contract holds.
- **MT low-step tier on sparse-match data (measured 2026-09).** Data whose only
  matches are far apart (~1 MiB unique windows, repeated) hits the MT tier's
  chain-16 cap: mt4 packed volume measured ~8x mt1 on a synthetic corpus.
  Realistic mixed corpora stay at ~+1.9pp (issue 13's documented trade); treat
  this corner as evidence for that accepted trade, not a new bug.

- 13 collect 带宽/延迟判定 + 远带候选预算前缘（2026-09-07）：远带预算
  `RAR_RS_FAR_BAND` 是 seq opt-in 速度档（1M/2：-9% @ +0.22pp），非 mt8
  解药；mt8 需每位置步数降 ~5x（架构级）。
- 13 定论（2026-09-08）：**MT-only 低步数 m3 搜索落地为 MT
  默认**（`mt_slice_symbols_low_step`， hash-chain greedy+lazy，链预算
  16，head/prev 数组跨 slice 复用 `chain_parts`；seq 不动）。 mt8 tsc 2791→1569
  ms（1.78x，ratio +2.43pp）、text 1075→238 ms（4.5x，+0.49pp）；random
  0.34x（STORE 兜底，有界）。vs WinRAR m3/mt8：text 反超（238 vs 251 ms /
  +0.53pp），tsc 仍差（2.6x 慢 / +5pp，DRAM 带宽悬崖，架构级）。
- 09 双环/逐级 son-pair prefetch
  扩展（2026-09-08）：最后一个未试的字节级流水杠杆（descent
  内异步预取两候选子对的 son 对）20 样本 A/B 中性偏负（tsc6 med 3048 vs 3078），
  与既有 T0 son+input 拒绝一致——未取分支的缓存行在稠密二进制 L2 上是净污染。BT4
  字节级流水已封顶（首步 value-carry -3.6%
  即全量）；再降每位置步数只剩显式取舍项（MT-only 低步数搜索 / FAR_BAND
  opt-in），见 issue 13 判决（下表行 13）。
- 14 两制近存实测为负（2026-09-07）：近带 256 KiB L3 驻留 + 远树到期重插（曾用
  env `RAR_RS_TWO_TIER`，已回退、不再有开关）——远插把每位置步数做大约 2-3x， seq
  -75%、mt8 -98%；budget=4 采样式远插 ratio 漂 +0.54pp 仍 mt8
  -66%。方向废弃，留档负例。

Open frontier (see issues/):

- 04 window-level incompressible skip for MT — biggest remaining speed lever on
  random data; member-level ratio safety is the open question. (The MT low-step
  tier closed most of the compressible-tail gap but random through the chain is
  still ~3x slower than the matchless fast path — see issue 13 verdict.)
- 09 dll single-threaded parse speed (~5.9x behind WinRAR -mt1 on a real DLL;
  BT4 descent steps are structurally locked, see issue 09)
- 15 Fast-LZMA2 / zstd-style parser ideas for speed (intended, opened
  2026-09-18): price-driven effort tiers (a cheap bounded-candidate priced DP to
  fill the m1 -> m2 cliff, first on the MT low-step tier), tabulated price
  components (byte-identical), a row/radix-style bounded-depth finder for the
  fast tiers, and a cheaper incompressibility pre-gate (issue 04's other half).
  The default `-m` levels keep their bytes; FL2's own trade (1-5% larger output
  at the same dictionary) is only acceptable in a new documented tier, and
  upstream entropy coders/format cannot be transplanted into RAR. See
  [issues/15-fl2-zstd-parser-tiers.md](issues/15-fl2-zstd-parser-tiers.md).

Closed: 05 streaming delta + x86 landed (`delta_stream_window` /
`x86_stream_window`, PLAN 旧版 `git show c2c43d4:PLAN.md`「RAR5（压缩面）」,
2026-09); 06 solid-mt member-level gap is structural (chunk-level MT landed;
member-level parallelism needs a shared window, PLAN 旧版
`git show c2c43d4:PLAN.md`「定论」, 2026-09).

## Fog

- mt1 random 64 MiB (l3) ≈ 1750 ms: collect ≈ 440, encode ≈ 450, dp ≈ 93, ~800
  unaccounted (fast-path loop, splitter, seeding, LR build, setup).
- Collect is now dominated by the 256-descent pre-fast warmup + recovery
  searches, not the per-position loop.
- The pre-gate for delta (`auto_delta_filter_channels`) is sample-based since
  45fa1e0; candidates are the PCM frame sizes.

## Closed issues (verdicts)

The individual ticket files were folded into this table once they closed; the
open ones (04, 09, 15) still have their own file.

| #  | Issue                                         | Verdict                                                                                                                                                                                                                        |
| -- | --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 01 | Matchless-block DP fast path                  | Landed, byte-identical                                                                                                                                                                                                         |
| 02 | Collector fast-mode gating                    | Landed (`longest == 0`, threshold 256)                                                                                                                                                                                         |
| 03 | Delta candidate channels + sampled pre-gate   | Landed                                                                                                                                                                                                                         |
| 05 | Auto delta/x86 filters on the streaming path  | Landed                                                                                                                                                                                                                         |
| 06 | Member-level solid MT                         | Structurally abandoned; chunk-level MT landed instead (a shared window cannot be parallelized across members)                                                                                                                  |
| 07 | Skip re-pricing on literal-heavy blocks       | Rejected: the gate only fires where parsing is already cheap (the matchless fast path covers the all-literal case), never on the dense-match blocks that dominate DLL time, yet it changed DLL bytes                           |
| 08 | Persistent tree corruption across chunk grows | Fixed (`grow_to` copies links, `rebase` migrates slots) plus a per-byte verification net in the collector                                                                                                                      |
| 10 | 2-3 byte short matches                        | Rejected: short-match slot code lengths depend on a frequency bootstrap (WinRAR emits 315 K of them), which two-pass pricing cannot reproduce safely — every variant lost ratio. The analyzer tooling that found this was kept |
| 11 | BT4 first-step value carry                    | Landed (-3.6%); batch cheats beyond one step stay barred by the insertion-order invariant                                                                                                                                      |
| 12 | MT near-window alignment                      | Landed, then superseded by 13's low-step tier                                                                                                                                                                                  |
| 13 | Far-band budget + MT bandwidth cliff          | Verdict: MT-only low-step search became the MT default; the far-band budget only helps seq and is dormant                                                                                                                      |
| 14 | Two-tier near window                          | Measured negative (seq -75%, mt8 -98%), abandoned and kept as a negative example                                                                                                                                               |

## Completed

- 01 matchless-block DP fast path (resolved, 3cd6b37)
- 02 collector fast mode gating `longest==0` + thresholds 256 (resolved,
  3cd6b37)
- 03 delta filter candidate channels + sampled pre-gate (resolved, 45fa1e0)
- 05 streaming-path auto delta filter for >64 MiB members (resolved, 2026-09-07;
  per-window piecewise transform, records relative to window start, solid-chain
  break, regression in large_paths)

## Measured level ladder (2026-09-18, single host)

`rar a -m<level> --dict-size 32m --threads <t>` on a 12.5 MiB DLL (the CLI path,
so the x86 filter policy is in play). Snapshot, not a contract:

| level | seq (mt1)           | mt8, before ③       | mt8, row index      |
| ----- | ------------------- | ------------------- | ------------------- |
| m1    | 974 ms / 6,766,361  | (chain 4, not run)  | 2106 ms / 5,949,729 |
| m2    | 6137 ms / 5,779,542 | (chain 16, not run) | 1787 ms / 5,897,174 |
| m3    | 6727 ms / 5,751,821 | 1769 ms / 6,516,302 | 2013 ms / 5,858,387 |
| m4    | 7705 ms / 5,750,782 | (chain 16, not run) | 2306 ms / 5,830,234 |
| m5    | 9324 ms / 5,751,001 | (chain 16, not run) | 2791 ms / 5,809,527 |

The codec-only ladder measured earlier through `collectbench` (no filter policy,
same file, same dictionary) is the frame of reference for the "cliff": m1 1067
ms / 54.05%, m2 6548 ms / 46.88%, m3 7242 ms / 46.67%, m5 9247 ms / 46.67%. On
the CLI numbers the m1 -> m2 step is 6,766,361 -> 5,779,542 B (**-14.6%** for
6.3x the time), and above m2 the ladder is nearly flat: m3 -> m5 buys under
0.02% for +39% time, with m5 not even monotone (5,751,001 B against m4's
5,750,782 B) — the repricing passes add fraction-of-a-percent noise, which is
what issue 09 records.

What a level changes comes from `LEVEL_PARAMS` plus the parse-pass table, and
nothing else (the dictionary is **not** level-dependent: `dict_log_for` defaults
to 32 MiB at every level, capped at twice the file size, and `max_match` is
0x1001 everywhere):

| level | matcher       | chain budget | price passes | long-range table |
| ----- | ------------- | ------------ | ------------ | ---------------- |
| 1     | greedy + lazy | 4            | 0            | no               |
| 2     | optimal DP    | 16           | 2            | yes              |
| 3     | optimal DP    | 96           | 2            | yes              |
| 4     | optimal DP    | 256          | 3            | yes              |
| 5     | optimal DP    | 1024         | 4            | yes              |

`-m` under `-mt`: the MT row-index tier carries its own level dial, a candidate
depth per level (`MT_ROW_INDEX_DEPTH` = 16/32/64/128/256 for m1..m5, measured in
the table above), because the chain budget and the pricing passes are
sequential-tier concepts. The packed column is monotone in depth and the time
column is not — on text a deeper walk is _faster_ as well as smaller, since the
long match it finds lets the priced DP commit and skip the positions it covers.
On the 4.88 MiB source tree at `-mt8` the ladder is 973,459 / 951,894 / 935,472
/ 923,202 / 914,457 B against a sequential 1,180,957 (m1, greedy) / 922,257 (m3)
/ 921,710 (m5), so m5 beats the sequential parse there while m1 stays the cheap
rung. The remaining known gap: the MT DP runs one pricing pass where the
sequential m3 runs two and m5 four, which is why the MT _default_ level is still
1.4% behind sequential on text (935,472 against 922,257 B); giving the MT DP the
level's pass count is the next lever, not a tuning constant.

### Issue 15 lever "cheaper incompressibility gate": landed (2026-09-18)

The probe's own cost was measured first: on a 4 MiB member it encodes 512 KiB +
3x256 KiB of samples at the member's own method — 18-28% of that member's total
encode time (text m1 18.1/83.6 ms, text m5 1234/6872 ms), while on random data
it is the short-circuit itself (35 ms vs the full parse). So the win is on
_compressible_ members, and the probe has to keep running on incompressible
ones.

`sample_is_incompressible` / `sample_is_incompressible_file` now run a cheap
structural screen over the same sample regions first: sampled 4-byte windows
every 16 bytes, all-distinct-windows = no structure (the statistic the MT tail
probe already used, now owned by
`codec/common/incompressible.rs::distinct_window_percent`). When _every_ region
shows repeated windows the sample encodes are skipped. Measured on 8 MiB corpora
(two binaries, interleaved): text m3 180 -> 155 ms, text m5 288 -> 256 ms (-13%
/ -11%), `wordtext`/`x86syn` likewise; `random` unchanged (34 ms, the screen
stays quiet). Ratios unchanged everywhere.

Why it cannot change an archive: skipping the probe only removes a verdict that
could have forced STORE, so the codec's own result decides and a member it
cannot shrink still falls back to STORE byte-identically — the archive keeps its
bytes or gets smaller, never larger (map's rule). A screen that misses a
structured region costs the old sample encodes; one that calls incompressible
data structured costs a full parse that still STOREs. The window test separates
the case order-0 entropy cannot: base64 of random bytes sits at ~6 bits/byte
like a real DLL, but its windows are all distinct. Pinned by
`structural_screen_only_skips_probes_that_would_say_compressible` and, end to
end, `writer_structural_screen_keeps_archive_bytes_identical` (packed streams
compared with the screen on and off).

### Issue 15 lever "cheap candidates + global DP": measured negative (2026-09-18)

The tempting way to make the windowed tier cheap is to search fewer positions:
the greedy tier only searches where a token starts and merely inserts the bytes
a match covers, while the windowed DP needs a candidate wherever it may start.
Striding the candidate search to every 16th byte (inserting the rest) makes it
_worse in both directions_ (dll.bin 12.5 MiB, m3, mt8, real CLI):

| candidate search                  | time    | packed               |
| --------------------------------- | ------- | -------------------- |
| greedy (today's MT)               | 1655 ms | 6,516,302            |
| every position (full windowed DP) | 3667 ms | **5,997,137**        |
| every 16th byte (grid)            | 2592 ms | **8,103,275 (+24%)** |

So the DP's gain is _made of_ dense candidates; there is no cheap-candidate
shortcut. The BT4 finding stands (re-inserting a slice's lookbehind costs ~5x
the member at the tree's measured 5.3 MiB/s, so a per-slice sequential parse is
slower than the sequential member) — but it only rules out _re-inserting_, and
that turned out to be avoidable: see the landed row index below, which keeps the
candidates dense by building the candidate source once for the whole member
(6,516,302 -> 5,897,174 B at 1.3x greedy's MT time).

Measured while sweeping: the buffered writer only parallelized a single member
at or above its `MT_MIN` (3 x 4 MiB), so a 4.88 MiB member ignored `-mt`
entirely. Text at m3 therefore reports seq numbers for every `-threads` value.

### Issue 15 lever "shared row index + priced DP": landed (2026-09-18)

The thing that makes MT-with-near-seq-ratio possible is not a cheaper search but
a candidate source with **no per-slice state**: a shared, read-only row index
over the whole member, built once, queried by every worker. It is a
counting-sorted CSR keyed on the _4-byte_ window (2^20 buckets, positions as
`u32`, ~4 B/byte plus a 4 MB table), so workers never build a lookbehind frame,
never insert, and can still reach the whole dictionary instead of an 8 MiB tail.
On top of it each slice runs the sequential path's own block-global priced DP
(`optimal_parse_tokens`), with the sequential collector's multi-run candidate
shape (every candidate that beats the best length gets its own run) and the
sampled long-range table for bytes older than the buffer (a solid chain's other
members, an earlier MT window).

Each step, measured on dll.bin 12.5 MiB m3 mt8 (greedy 6,516,302 B):

| step                                                       | time    | packed    |
| ---------------------------------------------------------- | ------- | --------- |
| chain candidates, one run per position (old tier)          | 3211 ms | 5,997,137 |
| row index, 3-byte hash, one run, depth 32                  | 2461 ms | 6,115,336 |
| + multi-run candidates, depth 64 (still 3-byte)            | 2404 ms | 5,999,488 |
| + 4-byte hash and 2^20 buckets, depth 32                   | 1719 ms | 5,897,159 |
| + DP block 1 MiB instead of 64 KiB (depth 64)              | 2184 ms | 5,999,545 |
| **landed: 4-byte hash, DP block 256 KiB, depth per level** | 2013 ms | 5,858,387 |

The 4-byte key is what made it cheap: every bucket member is then a potential
4-byte match, so no probe is spent on candidates that cannot reach the minimum
length (2^16 three-byte buckets over 12.5 MiB average ~190 entries). Depth is a
dial on dense binaries (128 -> 5,830,216 B, 256 -> 5,809,527 B) and is now the
per-level MT dial again (see the ladder section). An earlier version of this
file claimed depth "costs text nothing, because a 4-byte hash over 4.88 MiB of
source leaves ~5 entries per bucket"; that was measured while MT was not
engaging for that member at all (routing bug recorded below), and the real text
ladder is 951,894 / 935,472 / 923,202 / 914,457 B for depth 32 / 64 / 128 / 256.

Results (m3, dict 32m, `-mt8`, real CLI, this host):

| corpus                    | sequential (mt1)    | MT greedy (before)   | MT row index (now)        |
| ------------------------- | ------------------- | -------------------- | ------------------------- |
| dll.bin 12.5 MiB          | 5,751,821 / 6120 ms | 6,516,302 / 1769 ms  | **5,897,174 / 2328 ms**   |
| big.bin 75 MB (streaming) | 17,894,515 / ~28 s  | 20,021,162 / 7513 ms | **17,935,613 / 13047 ms** |
| src.txt 4.88 MiB          | 922,257             | 922,257              | **922,257** (identical)   |
| xml6 / mixed6             | unchanged           | unchanged            | **identical**             |

So the streaming tier now lands within 0.23% of the sequential parse at 2.1x its
speed (and 1.7x faster than the chain-candidate windowed DP for the same bytes),
while text and structured members are byte-identical to what the greedy tier
produced. Official UnRAR 6.23 and 7.23 both accept a 75 MB `-mt8` archive (`t`
OK) and 7.23 extracts it byte-identically. WinRAR's own mt8 on dll.bin is 450 ms
/ 5,645,746 B, so the remaining gap is still a faster finder, not the parse
tier.

Three defects this work surfaced, all now covered: the CSR cursor must not write
back through the bucket-end array (it collapses every range to empty and the
parse silently emits literals — 10,208,233 B on the DLL; unit-tested), the
long-range probe's `min_dist` must be "bytes the near source owns at the slice
start + offset", not the dictionary size (using the dictionary hid every
cross-member match, which `mt_tests::solid_members_share_the_window_through_mt`
caught), and the windowed driver needed multi-run candidates to match the
sequential collector's reach.

Superseded and removed in the same change: `RAR_RS_MT_WINDOW_DP`,
`mt_slice_symbols_windowed` and `windowed_chain_parse` (the chain-candidate
windowed DP is strictly dominated by the row index: same packed size, 25% slower
on the DLL), plus the three experimental switches of this work
(`RAR_RS_MT_ROW_INDEX`, `RAR_RS_MT_ROW_DEPTH`, `RAR_RS_MT_DP_BLOCK`). The
sequential path never reaches any of this, so the ratio contract is untouched.

### The MT floor was silently excluding 4-12 MiB unfiltered members (2026-09-18)

`MT_MIN = 3 * DEFAULT_CHUNK_SIZE` came in with the first mid-size MT change
(`7d7008c`) and spread to the batch and streaming sites. **It had no recorded
rationale**: that commit touched no docs, measured only a 19.5 MB text member
(711 -> 369 ms at `-mt8`, +40 bytes), and its message described the band as
"2–64 MiB" while the constant says 12 MiB.

It only ever guarded the _unfiltered_ in-memory path: members with a filter run
`encode_with_filters_mt`, which is parallel at any size. So a text/JSON/CSV/XML
member in the 4-12 MiB band (a very common size) took the sequential path even
with `-mt8` — measured: a 4.88 MiB source tree encoded identically, same bytes
and same milliseconds, at `--threads` 1 and 8.

Lowering the buffered (`add.rs`) gate to one chunk was the first attempt, and it
did not fix the CLI: `rar a` routes through `batch.rs` and never touches that
branch (instrumented — the `add.rs` print never fired). That is also why every
"MT on src.txt" number recorded earlier in this file was really a sequential
number. The batch gate is **wave-shape aware** now: a wave that already fills
the pool with members keeps the three-chunk floor (slicing there would nest MT
in a saturated pool), while a smaller wave — the common single-file `rar a` —
slices from one chunk. Measured on a 4.88 MiB source tree at `-m3`:
`--threads 8` now differs from `--threads 1` (935,472 B / 1133 ms against
922,257 B / 2669 ms), where before it produced byte-identical output at both
settings.

What the floor implicitly balanced, now measured: a slice re-inserts its
lookbehind (up to `NEAR_WINDOW_MAX` = 8 MiB) for a slice floored at 2 MiB, so
the band pays a large insert volume for only 2-5 slices. That insert is exactly
what the row index removed (above), which is why the floor could come down at
all. chunk (`add.rs`, single-member path only; the batch gate stays at 3 chunks
to avoid nested MT inside a pool wave) gives: 4.88 MiB text 2799 -> 2340 ms
(**+1.2x**) with **byte-identical** output, very compressible data unchanged (6
MiB XML: 617 vs 614 ms, same bytes — the sequential parse already steps over
it), DLL and sub-4 MiB members unchanged.

### Buffered members: the filter competition was half the runtime (fixed 2026-09-18)

Same defect as the streaming gate above, one layer down. On a 4.88 MiB source
tree at m3 the buffered writer spent **4434 ms** against **2198 ms** with
`-mc de-`, for an archive **53 bytes larger** than unfiltered: the sample-based
candidates each cost a whole-member encode, and the comparison did not count the
filter _records_ the winner then pays.

Bisected by filter: `-mc d-` (x86 auto only) reproduced the cost and the 53
bytes, `-mc e-` did not, and 1 MiB probes of a real DLL showed why a window
cannot judge x86: the prefix _loses_ 0.07% there while the member _wins_ 6%, so
x86's gain is a long-range whole-member effect. Delta's effect is local, so the
probe gate applies to delta only; x86 keeps its detection-based decision.

Sizes are all the codec's now:

| member           | before                | after                 | WinRAR 7.23 (m3 mt1)  |
| ---------------- | --------------------- | --------------------- | --------------------- |
| src.txt 4.88 MiB | 4434 ms / 922,310 B   | 2819 ms / 922,257 B   | 411 ms / 930,583 B    |
| dll.bin 12.5 MiB | 6495 ms / 5,751,821 B | 6898 ms / 5,751,821 B | 1892 ms / 5,645,705 B |
| 68 MB mixed      | 28,451,342 B          | 14,620,275 B          | —                     |
| 75 MB mixed      | 34,737,024 B          | 17,894,515 B          | 17,023,103 B          |

The text case is **-36%** (and smaller); the DLL pays +6% for the probe and
keeps its bytes exactly.

### The x86 detector needed a density floor (2026-09-18)

The 53-byte regression came from the _x86_ candidate, not delta: its cluster
scan only needs two opcodes within a few KiB, so text passes it (0.030% of its
bytes look like E8/E9) and then pays a whole-member encode; the archive came out
53 bytes _larger_ than unfiltered. `auto_x86_filter_ranges` now requires
**0.5%** opcode density for inputs at or above 1 MiB (below that the extra
encode is microseconds and synthetic inputs legitimately carry few opcodes).
Real code sits two orders of magnitude above the floor: a 12.5 MiB system DLL
measures 2.08%. After the floor, text at m3 goes 4434 -> 2819 ms and 922,310 ->
922,257 B, while the DLL's bytes are unchanged (5,751,821 B, x86 still applied).

### Streaming member filter gate: measured, not guessed (fixed 2026-09-18)

The streaming writer (members at or above the 64 MiB threshold) had to commit to
a delta/x86 filter _before_ compressing, and it decided from a 64 KiB head
sample. On real DLLs that sample looks delta-friendly (PE headers, import
tables) so delta was applied to the whole body — and delta destroys x86 match
structure, so the member packed far larger while still decoding correctly (the
transform is invertible, which is why every test stayed green).

Decisive measurement, same content, 1 MB apart across the threshold (m1):

| member       | path      | packed             |
| ------------ | --------- | ------------------ |
| 67,000,000 B | buffered  | 13,676,143 (20.4%) |
| 68,000,000 B | streaming | 28,451,342 (41.8%) |

Bisected by disabling one filter at a time: `--mc d-` restored parity (16.5 vs
17.7 MB), `--mc e-` kept the blowup. The 75 MB `big.bin` corpus went 34,737,024
-> 21,580,579 (-38%) after the fix.

The gate is now a measurement: every _auto_ candidate is transformed with the
production helper (the same `delta_stream_window` / `x86_stream_window` the
window loop uses) and packed at two probe points (head and middle, 512 KiB
each); it is kept only if it beats plain LZSS by `FILTER_TRIAL_MARGIN_PERCENT`
on **both**, and when both delta and x86 survive the smaller wins instead of
hitting the old `InvalidOption`. The margin is load-bearing: on incompressible
data the delta transform packed 524423 B against plain's 524424 B — a one-byte
"win" that a strict `<` accepted and that cost a whole member. Forced filters
(`-mcd+` / `-mce+`) skip the gate, so their bytes are unchanged (verified:
forced-delta output identical to before).

Verification: our extraction byte-identical, official UnRAR 7.23 and 6.23 `t`
pass on the fixed archive and on a 64 MiB delta-friendly member (still filtered,
64 MiB -> 5,531 B), `large_paths` green including the legit-delta round-trip,
and the RAR5 streaming interop suite (8 cases) green.

### Issue 15 lever "windowed priced DP": measured positive (2026-09-18)

The global-decision half of the priced tier works where the per-byte half did
not. `windowed_chain_parse` feeds the _sequential_ path's own optimal parse
(`optimal_parse_tokens` + `convert_tokens`, no new decision logic) with one
candidate run per position from the cheap chain finder, plus the shared
long-range probe and the estimated first-pass prices. Measured on `ntoskrnl.exe`
12.5 MiB, mt8, same binary via `RAR_RS_MT_WINDOW_DP`:

| mt8           | greedy (today)   | windowed DP         |
| ------------- | ---------------- | ------------------- |
| m1 (chain 4)  | 438 ms / 53.77%  | 754 ms / 49.98%     |
| m3 (chain 16) | 1117 ms / 52.05% | ~2.8-3.1 s / 48.48% |

Readings: (1) the windowed DP recovers ~3.6-3.8pp of the ~5.4pp MT-vs-seq gap,
i.e. MT can reach near-seq ratio (48.48% against seq's 46.67%) at ~2.3x the
speed of seq m3; (2) more usefully for speed, **m1 + windowed DP (754 ms /
49.98%) beats today's m3 greedy (1117 ms / 52.05%) on both axes**, so the MT
ratio floor is no longer ~52%; (3) it is a time-for-ratio trade at a fixed level
(1.7x at m1, ~2.5x at m3), which is why it stays behind the switch: changing an
existing `-m` level's MT timing/output is a product decision, not a free
optimization. Output decodes byte-identically (`collectbench` asserts it).

### Issue 15 lever "priced cheap tier": measured negative (2026-09-18)

Replacing the MT tier's raw-length lazy rule with a price-driven choice (the
finder's best, the two most recent distances and the repeat length, compared by
estimated bits per byte, plus a priced one-position lookahead — zstd's `opt0`
shape) was implemented behind `RAR_RS_MT_PRICED` and measured on `ntoskrnl.exe`
m3/mt8: 52.05% without it, 54.45% with it, at the same speed. Sweeping the
model's literal price (3, 5, 6, 9, 14 bits) moved the result only between 53.07%
and 54.45% — never below the baseline. Diagnosis: the static pre-block estimate
cannot see the table cost of the extra symbols a per-byte rule creates (it
fragments long matches into short cheap ones), so the length rule's "take the
longest, lazy-skip when the next is longer" is a better proxy. Reverted, no
switch left (same handling as issue 14's negative).

What that leaves for issue 15: a _global_ decision with _real_ prices — a
bounded/windowed DP over the cheap finder's single candidate per position,
re-priced from the statistics of a first cheap pass (zstd's `opt1` shape). The
unpriced full DP (`-m2`) is 6x the cheap walk; a windowed one is the unmeasured
middle point.

## Real head-to-head vs WinRAR 7.23 (2026-08, m3)

Corpus: text64 (68 MB repetitive text), rand64, mixed20 (text+random+text), dll
(ntoskrnl.exe 13 MB). Ours built with rar5/parallel; `-mt<N>` honored via
normalize_switch (`-mt8` → `--threads=8`).

| file    | ours mt1             | ours mt8           | winrar mt1 | winrar mt8 | ratio ours | ratio win |
| ------- | -------------------- | ------------------ | ---------- | ---------- | ---------- | --------- |
| text64  | 2.2 s                | 0.86 s             | 0.91 s     | 0.39 s     | 12681 B    | 8769 B    |
| rand64  | 0.19 s (STORE probe) | —                  | 15.8 s     | 2.7 s      | 100.0%     | 100.0%    |
| mixed20 | 2.7 s                | 2.7 s (filter seq) | 1.3 s      | 0.39 s     | 50.03%     | 50.11%    |
| dll13   | 6.5 s                | 6.5 s (filter seq) | 1.8 s      | 0.42 s     | 45.08%     | 43.06%    |

Findings: (1) filter members (x86) are strictly sequential — mixed/dll get no MT
benefit; the single filtered m3 encode of 13 MB dense binary is ~2 MiB/s vs
WinRAR ~7 MiB/s. (2) MT scaling is healthy on unfiltered files (2-64 MiB MT
landed 7d7008c; text 19.5 MB 711→369 ms). (3) ultra-repetitive text ratio gap
(12681 vs 8769). (4) incompressible probe keeps us 10-80x faster than WinRAR on
random data.

## Head-to-head after adaptive blocks + corruption fix (2026-09, m3 seq)

Same corpus, this machine (release). All roundtrips byte-verified; our archives
pass WinRAR `t`, WinRAR archives pass our `t`.

| file             | ours packed         | winrar packed                | winrar archive | verdict                                                        |
| ---------------- | ------------------- | ---------------------------- | -------------- | -------------------------------------------------------------- |
| text64           | 6058 B              | 8696 B (wr_text64.rar)       | —              | **-30% vs WinRAR** (was +44% worse)                            |
| dll (x86 filter) | 5751393 B (43.90%)  | 5870437 B (44.81%, dll8.rar) | —              | **-2% vs WinRAR**; previously CORRUPT (silent tree bug, fixed) |
| mixed20          | 10489643 B (50.02%) | 10492xxx B                   | —              | ≈ / slightly better                                            |
| rand64           | 67118715 B          | 67108940 B                   | —              | tie (STORE)                                                    |
| xml (1 MB)       | 88950 B             | 87656 B                      | —              | +1.5% (pre-existing parse gap, not block overhead)             |

text64 12681→6058 came from emitted-block merging (see PLAN.md): WinRAR writes
one block per member on stable data; we merge emitted blocks to 4 MiB with
local-drift closing. dll's corruption was the persistent tree wiping its son
array on chunk grows + rebase slot misplacement — fixed, with a
byte-verification net and a 129 KB x86 regression fixture.

Remaining gaps: dll single-thread parse speed (~6 s vs WinRAR 1.8 s — ratio now
wins), xml parse (~1.5%), text64 MT ratio (6554 vs 6058, the documented MT
divergence).

## Speed work (2026-09-01, current state)

Landed: tree hash 17->20 bits (mt1 -4-15%, byte-identical except xml m2 -1 B)
and adaptive MT slice size (target ~2x threads, floor 2 MiB) — the fixed 4 MiB
slice left a 13 MB member with only 4 slices and the pool mostly idle. CLI
head-to-head at m3/mt8 (user default):

| file       | ours mt8 | win mt8 | ours B   | win B    |
| ---------- | -------- | ------- | -------- | -------- |
| dll (x86f) | 2323 ms  | 433 ms  | 5940581  | 5640870  |
| text64     | 889 ms   | 400 ms  | 6527     | 8814     |
| mixed      | 912 ms   | 369 ms  | 10492341 | 10509703 |

Library core (encode_with_auto_x86_filter direct): dll mt8 2638 -> 1329 ms
(-50%); the CLI added ~1 s of overhead in this 2026-09-01 run (batch-wave +
delta attempt 129 ms + blake2 + container — the nesting of the wave pool and the
MT pool was the suspected largest chunk). **That ~1 s was later disproven**
(2026-09-07; re-measured 2026-09-11 in issue 09): the wave pool and the MT pool
are the same cached pool, and the CLI tracks the library within ~5-15%. MT
divergence on the dll grew from +2% to +3.3% (mt8 vs mt1) with the adaptive
slices; a denser LR (step 8) recovered only 2 KB of the +65 KB — the divergence
is the slice-boundary parse structure, not the LR sampling.

Next levers: the per-byte parse (collect 5.3 s of the 8.4 s seq time, the
DRAM-bound BT4 descent) — a cache-resident near-window chain finder with the
tree as the far fallback is the designed-but-unbuilt option (see issue 09); the
CLI's ~1 s overhead is **resolved** (2026-09-07, see issue 09) — CLI ≈ library
within ~5%, the residue is pipeline cost every caller pays. Issue 11's pipelined
first-step value-carry (BT4 descent, DRAM latency) is also **landed**
(2026-09-07): software-pipelined `seed_for` + `matches_seeded` in
`collect_block_matches`, byte-identical (unit-tested head/son equality +
ratios + 207 tests), tsc.exe m3/dict32: seq ~9.56→9.21 s (−3.6%), mt8 ~2.23→2.15
s (−3.7%). Batch cheats beyond one step are still barred by the BT4
insertion-order invariant.

MT near-window alignment (2026-09-07, issue 12): the near-window cap in
`encode_mt_slice` is now `NEAR_WINDOW_MAX` (8 MiB, matching the sequential
path), with the fresh-tail seed thinned to a stride over the old >2 MiB band
(`MT_FAR_SEED_STRIDE=16`, `MT_FAR_SEED_CHAIN=2`, frontier 2 MiB, MT-only via
`lr_shared`). Fixes the +5.2pp MT ratio divergence on compressible-tail + 2-8
MiB exact-copy data (blockdup, mt8 converges to seq to the byte), at ~1.4x of
the old 2 MiB-cap mt8 time (full 8 MiB seeding was 3.5x); text-class ratio
byte-identical with ~0% speed regression. `distant` (random + far copies) still
probes incompressible and stays at 59.14% — probe-length issue, see issue 12
待办. (Superseded 2026-09-08 by the MT low-step tier: workers no longer
build/seed the tree at all — see issue 13 verdict; the stride shape is dormant
in `find_matches_optimal`.)

Collect band/latency verdict + far-band budget frontier (2026-09-07, issue 13):
profiling (counters kept to single-thread runs; shared atomics polluted the mt8
timing 2x) shows seq is cache-mixed latency-bound (~46 ns/step) and mt8
saturates DRAM bandwidth (~121M steps/s ~ 31GB/s, scaling cliff 4->8 at 1.39x).
The ratio/traffic frontier of a far-band descent budget (`RAR_RS_FAR_BAND`,
TreeMatchFinder.far_start/far_cut, dormant off) peaks at 1 MiB/2 steps: tsc seq
-9% at +0.22pp, but mt8 only -4% (MT slices have few far-band candidates) — so
it is a seq speed option, not the mt8 fix. To close the WinRAR mt8 gap the
per-position steps must drop ~5x (architecture), see the issue 13 verdict row
below.

## Definitive head-to-head (2026-09-01, fixed CLI, m3, this machine)

After the rar-cli parallel-feature fix (the CLI silently ran single- threaded
when built standalone — see commit "fix(cli): enable the rar5 parallel feature")
and the u64 word compare:

| file       | ours mt1 | ours mt8 | win mt1  | win mt8 | ours B   | win B    |
| ---------- | -------- | -------- | -------- | ------- | -------- | -------- |
| text64     | 2275 ms  | 846 ms   | 927 ms   | 390 ms  | 6133     | 8771     |
| dll (x86f) | 8094 ms  | 3115 ms  | 1722 ms  | 418 ms  | 5751467  | 5640802  |
| mixed20    | 2716 ms  | 1032 ms  | 1254 ms  | 373 ms  | 10490667 | 10509703 |
| rand64     | 192 ms   | 186 ms   | 15836 ms | 2663 ms | 67108940 | 67109030 |

Ratio verdict: text64 **-30%** (win), mixed **-0.2%** (win), rand64 tie, dll
**+0.84%** (43.90% vs 43.06% — the fresh WinRAR run; the earlier dll8.rar
comparison was against a suboptimal WinRAR archive). Speed: 2.2-2.5x behind mt1
(text64/mixed), 4.7x on the dll mt1; mt8 2.2-7.5x.

DLL parse diff (both dict 2^7, m3): ours lit=2345249 / match=1082545 / filter
records=49 / 100 blocks @ 60 KB; WinRAR lit=1833699 / match=1279456 / filter=203
/ 207 blocks @ 28 KB. We emit ~520 K more literals — the remaining dll ratio gap
is the parse + the x86 filter scan (fewer, larger filter regions vs WinRAR's
finer scan).

## Filter members are MT now (2026-08, 2e21d0b)

encode_with_filters_mt: forward transform (unchanged) + windowed MT encode,
filter records lead the first slice. Both auto filters take a threads count.
Root cause found on the way: the MT long-range table is pre-built over the whole
window, so a slice's own positions shadowed the copy source it should match (a
random+exact-distant-copy 32 MiB file compressed to ~100% instead of 50%). Fixed
by keeping the previous occurrence per key (vals2) and probing with get_before;
the sequential encoder is byte-identical (its newest entry is always before the
chunk being parsed).

m3 results: dll 6.7 -> 3.1 s (-mt8, 36 KB smaller), mixed 2.7 -> 1.1 s, text
0.73 -> 0.39 s. Remaining gap vs WinRAR: per-byte parse speed on dense binaries
(~2-3x slower single-thread) and the ultra-repetitive-text ratio.

## Since v0.5.0 (napi/wasm release, 2026-08-29) -> HEAD (2026-09-02) — delta

基线 = v0.5.0 发版时状态（2026-08 头对头表，map 由 8666de0 记录；发版到该表之间
只有 7d7008c 一个 perf 变更，且不影响表中 filter 成员行）。本机 m3，对照 WinRAR
7.23。

### 落地的 perf 提交（31 个提交中 11 个 perf/codec 相关）

| commit  | 内容                                                                         | 效果                                                                                                             |
| ------- | ---------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| 3cd6b37 | 不可压缩输入提速（matchless DP 快路径 + collect 快模式门控 longest==0/256）  | 64 MiB 随机 mt1 5044→1751 ms、mt8 1253→486 ms；ratio 100.02%→100.01%                                             |
| 45fa1e0 | delta 候选通道扩到帧尺寸 [1,2,3,4,6,8,9,12,16] + 预门改 64 KiB 采样          | 32 位立体声 11%（原 ~18%）、24 位 3ch 14%、16 位 4ch 22% vs plain 84%；预门从全量扫描（63 MiB ~300 ms+）降到采样 |
| 47c5394 | 无匹配块无活 rep 时跳过重复距离探测                                          | 微优化                                                                                                           |
| 7d7008c | MT 扩展到中尺寸成员 + x86 过滤抽样选择 + 进度                                | text 19.5 MB mt8 711→369 ms                                                                                      |
| 2e21d0b | **过滤成员 MT**（此前严格串行）+ MT 长距离自影修复                           | dll mt8 6.7→3.1 s、mixed 2.7→1.1 s、text 0.73→0.39 s；随机+远端副本 32 MiB 从 ~100% 修复到 50%                   |
| faa4ec3 | 长距离探测越窗守卫                                                           | 正确性                                                                                                           |
| 308157b | **自适应发射块大小**（合并到 4 MiB + 局部漂移闭合）+ 持久树跨 chunk 损坏修复 | text64 12681→6058 B（-52%）；DLL 归档此前静默损坏（unrar/WinRAR checksum error）→ 字节级回环                     |
| dd2d01c | 树下降 u64 字比较                                                            | dll mt1 -10%                                                                                                     |
| 38a1131 | **CLI parallel feature 修复**（独立构建的 CLI 静默单线程）                   | CLI dll mt8 3.2→2.3 s                                                                                            |
| 4c2666b | 树哈希 17→20 bits                                                            | mt1 全域 -4-15%，输出字节级一致（xml m2 -1 B）                                                                   |
| 8d23948 | 自适应 MT 片大小（目标 ~2×线程，下限 2 MiB）                                 | 库核心 dll mt8 2638→1329 ms（-50%）                                                                              |

### 速度净变化（可比口径，m3）

| 文件                | v0.5.0 mt8                        | 现在 mt8 | 变化                                |
| ------------------- | --------------------------------- | -------- | ----------------------------------- |
| dll（x86 过滤成员） | 6.5 s（**无 MT 收益**，严格串行） | 3.115 s  | **-52%**（且并发）                  |
| mixed20             | 2.7 s（无 MT 收益）               | 1.032 s  | **-62%**                            |
| text64              | 0.86 s                            | 0.846 s  | ≈ -2%                               |
| rand64              | 0.19 s                            | 0.186 s  | ≈（对 WinRAR 2.66 s 保持 14× 领先） |
| 64 MiB 随机 mt1     | 5.04 s                            | 1.75 s   | -65%                                |

### 压缩率净变化

| 文件    | v0.5.0                             | 现在                                                                         | vs WinRAR 7.23   |
| ------- | ---------------------------------- | ---------------------------------------------------------------------------- | ---------------- |
| text64  | 12681 B（+44.6% 差于 WinRAR 8769） | 6133 B                                                                       | **-30%**（反超） |
| dll     | 45.08%（+2.02%）                   | 43.90%（+0.84%，对比新跑的 WinRAR 43.06%；对 WinRAR 次优归档 44.81% 为 -2%） |                  |
| mixed20 | 50.03%                             | 50.02%                                                                       | -0.2%            |
| rand64  | 100.0%                             | 100.0%                                                                       | 平               |

### 附带修复（本应属于发版质量）

- 持久树跨 chunk 静默损坏：密集 x86 成员产出 WinRAR/unrar 双双 checksum error
  的归档 → 修复 + 收集器字节验证安全网 + 129 KB 内核镜像回归 fixture（308157b）
- CLI 独立构建静默丢 MT（38a1131）——napi 绑定不受影响（依赖 feature 常开），但
  CLI 用户在发版时实际拿不到任何多线程
- MT 长距离自影：随机+远端精确副本从 ~100% 压缩到 50%（2e21d0b）

### 剩余差距（保持开放）

- dll 单线程解析 ~6-8 s vs WinRAR 1.8 s（4.7×），mt8 7.5×；issue
  09（缓存驻留近窗 finder 未建）
- xml m2/m3 +1.5%（解析差距，非块开销）
- text64 MT 片间分歧（6554 vs seq 6058）
- CLI ~1 s 未记账开销（batch wave/MT 池嵌套）——**2026-09-07 实测不成立**（见
  issue 09）：CLI ≈ 库 ≈ raw codec +0.3-0.5 s 管线开销（24.5 MB x86
  m3/mt8），CLI 与库写作路径相差 ~3-5%，剩余是每个调用方都付的容器/哈希开销；CLI
  口径（`-mt` 设进程全局）下 wave 与内层 MT 共用同一缓存池，无嵌套 spawn（仅
  `WriterOptions::threads`
  每归档覆盖且异于全局时会配对两个池，属库用法）。**2026-09-11 复测**（64 MiB
  文本 / 5.7 MiB DLL / 256×256 KiB 批，m3 -mt8）：CLI−writer ~70-75
  ms，writer−裸 codec ~50-260 ms，256 文件批 mt8 ~0.62 s vs mt1 ~1.9
  s（3x，字节同），覆盖已存归档 ~0.63 s

注：两代头对头表的 dll mt1（6.5 s vs 8.09 s）不可直接比——测法不同（库核心直调 vs
修好后的 CLI），且 CLI 修复后测的是当时以为含 ~1 s CLI 开销的口径（2026-09-11
复测该开销不成立）。上表可比行仅限同口径数字。
