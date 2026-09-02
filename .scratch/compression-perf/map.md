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

## Speed work (2026-09-01, current state)

Landed: tree hash 17->20 bits (mt1 -4-15%, byte-identical except xml m2 -1 B)
and adaptive MT slice size (target ~2x threads, floor 2 MiB) — the fixed
4 MiB slice left a 13 MB member with only 4 slices and the pool mostly
idle. CLI head-to-head at m3/mt8 (user default):

| file | ours mt8 | win mt8 | ours B | win B |
|---|---|---|---|---|
| dll (x86f) | 2323 ms | 433 ms | 5940581 | 5640870 |
| text64 | 889 ms | 400 ms | 6527 | 8814 |
| mixed | 912 ms | 369 ms | 10492341 | 10509703 |

Library core (encode_with_auto_x86_filter direct): dll mt8 2638 -> 1329 ms
(-50%); CLI adds ~1 s of overhead (batch-wave + delta attempt 129 ms +
blake2 + container — the nesting of the wave pool and the MT pool is the
largest chunk, unaccounted). MT divergence on the dll grew from +2% to
+3.3% (mt8 vs mt1) with the adaptive slices; a denser LR (step 8) recovered
only 2 KB of the +65 KB — the divergence is the slice-boundary parse
structure, not the LR sampling.

Next levers: the per-byte parse (collect 5.3 s of the 8.4 s seq time, the
DRAM-bound BT4 descent) — a cache-resident near-window chain finder with
the tree as the far fallback is the designed-but-unbuilt option (see issue
09); the CLI's ~1 s overhead deserves a profile pass (wave/MT pool nesting).

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

## Since v0.5.0 (napi/wasm release, 2026-08-29) -> HEAD (2026-09-02) — delta

基线 = v0.5.0 发版时状态（2026-08 头对头表，map 由 8666de0 记录；发版到该表之间
只有 7d7008c 一个 perf 变更，且不影响表中 filter 成员行）。本机 m3，对照 WinRAR 7.23。

### 落地的 perf 提交（31 个提交中 11 个 perf/codec 相关）

| commit | 内容 | 效果 |
|---|---|---|
| 3cd6b37 | 不可压缩输入提速（matchless DP 快路径 + collect 快模式门控 longest==0/256） | 64 MiB 随机 mt1 5044→1751 ms、mt8 1253→486 ms；ratio 100.02%→100.01% |
| 45fa1e0 | delta 候选通道扩到帧尺寸 [1,2,3,4,6,8,9,12,16] + 预门改 64 KiB 采样 | 32 位立体声 11%（原 ~18%）、24 位 3ch 14%、16 位 4ch 22% vs plain 84%；预门从全量扫描（63 MiB ~300 ms+）降到采样 |
| 47c5394 | 无匹配块无活 rep 时跳过重复距离探测 | 微优化 |
| 7d7008c | MT 扩展到中尺寸成员 + x86 过滤抽样选择 + 进度 | text 19.5 MB mt8 711→369 ms |
| 2e21d0b | **过滤成员 MT**（此前严格串行）+ MT 长距离自影修复 | dll mt8 6.7→3.1 s、mixed 2.7→1.1 s、text 0.73→0.39 s；随机+远端副本 32 MiB 从 ~100% 修复到 50% |
| faa4ec3 | 长距离探测越窗守卫 | 正确性 |
| 308157b | **自适应发射块大小**（合并到 4 MiB + 局部漂移闭合）+ 持久树跨 chunk 损坏修复 | text64 12681→6058 B（-52%）；DLL 归档此前静默损坏（unrar/WinRAR checksum error）→ 字节级回环 |
| dd2d01c | 树下降 u64 字比较 | dll mt1 -10% |
| 38a1131 | **CLI parallel feature 修复**（独立构建的 CLI 静默单线程） | CLI dll mt8 3.2→2.3 s |
| 4c2666b | 树哈希 17→20 bits | mt1 全域 -4-15%，输出字节级一致（xml m2 -1 B） |
| 8d23948 | 自适应 MT 片大小（目标 ~2×线程，下限 2 MiB） | 库核心 dll mt8 2638→1329 ms（-50%） |

### 速度净变化（可比口径，m3）

| 文件 | v0.5.0 mt8 | 现在 mt8 | 变化 |
|---|---|---|---|
| dll（x86 过滤成员） | 6.5 s（**无 MT 收益**，严格串行） | 3.115 s | **-52%**（且并发） |
| mixed20 | 2.7 s（无 MT 收益） | 1.032 s | **-62%** |
| text64 | 0.86 s | 0.846 s | ≈ -2% |
| rand64 | 0.19 s | 0.186 s | ≈（对 WinRAR 2.66 s 保持 14× 领先） |
| 64 MiB 随机 mt1 | 5.04 s | 1.75 s | -65% |

### 压缩率净变化

| 文件 | v0.5.0 | 现在 | vs WinRAR 7.23 |
|---|---|---|---|
| text64 | 12681 B（+44.6% 差于 WinRAR 8769） | 6133 B | **-30%**（反超） |
| dll | 45.08%（+2.02%） | 43.90%（+0.84%，对比新跑的 WinRAR 43.06%；对 WinRAR 次优归档 44.81% 为 -2%） |
| mixed20 | 50.03% | 50.02% | -0.2% |
| rand64 | 100.0% | 100.0% | 平 |

### 附带修复（本应属于发版质量）

- 持久树跨 chunk 静默损坏：密集 x86 成员产出 WinRAR/unrar 双双 checksum error 的归档
  → 修复 + 收集器字节验证安全网 + 129 KB 内核镜像回归 fixture（308157b）
- CLI 独立构建静默丢 MT（38a1131）——napi 绑定不受影响（依赖 feature 常开），但 CLI 用户
  在发版时实际拿不到任何多线程
- MT 长距离自影：随机+远端精确副本从 ~100% 压缩到 50%（2e21d0b）

### 剩余差距（保持开放）

- dll 单线程解析 ~6-8 s vs WinRAR 1.8 s（4.7×），mt8 7.5×；issue 09（缓存驻留近窗 finder 未建）
- xml m2/m3 +1.5%（解析差距，非块开销）
- text64 MT 片间分歧（6554 vs seq 6058）
- CLI ~1 s 未记账开销（batch wave/MT 池嵌套）

注：两代头对头表的 dll mt1（6.5 s vs 8.09 s）不可直接比——测法不同（库核心直调 vs 修好
后的 CLI），且 CLI 修复后测的是含 ~1 s CLI 开销的口径。上表可比行仅限同口径数字。
