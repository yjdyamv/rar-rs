# ADR 0012：把距离/rep 裁决推到串行 emit 阶段（两段式解析）

- 状态：提议（未实现）
- 相关：ADR 0011（官方式匹配器）、`docs/issues/compression-perf/handoff-2026-11-16.md`

## 背景

官方 WinRAR 7.23 的多线程压缩把「找匹配」和「定距离码」分成两段：

- 并行段（`FUN_140044794`，经 `FUN_140045b70` 提交到线程池）每个 worker 只扫自己那个
  128 KiB 子块，命中就写一条**记录**到自己的缓冲 `job[3]`（记录结构：`+0x1a0` 绝对
  位置、`+0x1a8` 长度、`+0x400` 类别 0/1/2/3）；
- 串行段（`FUN_140044600`，按子块顺序调用）扫描这些记录：只有落在本子块区间
  `[param_2, param_2 + len)` 内的记录才处理，用 `FUN_140047d20(param_1, distance, …)`
  输出距离码，再往记录缓冲 `+0xb4b8`（容量 `+0xb4c0`、游标 `+0xb4c4`，每条 8 字节，
  低 4 字节是操作码 `0x30007`/`0x50007`）写 token：类别 0 → 长度表 + 距离表各一条，
  类别 1/2 → 只写 `0x30007` 并把 last-distance 槽设成 1 或 2，类别 3 → 走
  `LAB_14004476a` 的第三类编码。之后 `FUN_140044410(param_1 + 0xb4a8, buffer, len, 1)`
  对这块记录做熵编码收尾。

因此距离码与 rep 缓存的选择是**全局串行**的，切片数量不影响它：实测官方
ntoskrnl mt1 5,886,435 → mt24 5,886,560，只差 **125 B**。

我们的结构相反：`TokenPrices` 与 `dist_cache` 在**解析阶段**就用掉了，每个 MT 切片
必须从平估重新起价、rep 缓存清零，于是：

| 成员 | 顺序 | mt24（24 片） | 差 |
| ---- | ---- | ------------- | -- |
| ntoskrnl 13.9 MiB | 5,780,169 | 5,911,145 | +131 KB |
| ntdll 2.5 MiB（`RAR_RS_MT_MIN_LEN=262144` 时） | 990,282 | 1,014,263 | **+24 KB（+2.4%）** |

其中 ntdll 的损失里 0.8 pp 来自候选质量（行索引 depth 64 vs BT4 树，`MT_DEPTH=256`
可收回），剩下 **1.6 pp 来自每片重新起价**。

ADR 0011 曾否掉「per-class candidates」，原因是那版把候选分类后仍在**解析期**定距离，
于是每片依旧要自己重置状态。本 ADR 是同一目标的正确形态：裁决挪到串行段。

## 决策（待实施）

把 MT 路径改成两段：

1. **并行段**：每个切片产出 `Token { pos, len, distances: SmallVec<[u32; 4]> }`——
   只要候选距离列表，**不做价格、不选距离、不动 `dist_cache`**。DP 的匹配部分仍可
   保留（长度选择用固定单价即可），但距离留到第 2 段选。
2. **串行段**：按绝对位置扫过所有切片的 token 流，维护**跨片连续的**
   `dist_cache`（4 个 rep 距离）与 `TokenPrices`，为每个 token 选距离、写符号，
   并在块边界落 Huffman 表。

关键不变量：第 2 段只在 token 数（ntoskrnl ≈ 1.35 M）上迭代，而第 1 段在位置数
（13.9 M）上迭代，所以串行段的成本远小于现在的解析段。

## 预期与验收

- 比例：ntdll 的 `+1.6 pp` 应收回到 `+0.3 pp` 以内；ntoskrnl 的 mt1→mt24 差应从
  +131 KB 收到 +20 KB 量级（官方 125 B）。
- 速度：并行段不变（仍是最贵的一段），串行段预计 < 40 ms（1.35 M token × ~25 ns），
  比现在每片内部的距离定价更省，因此 mt24 的总 wall 不应变差。
- 回退：如果串行段超过 60 ms 或比例改善不到 0.5 pp，放弃并回到现结构。

## 落地步骤

1. `crates/rar/src/codec/modern/lzss_huff/encoder/parse/optimal.rs`：
   `optimal_parse_tokens` 增加「只给长度、不给距离」的模式（距离槽位写 `u32::MAX`
   占位，候选距离另存）；`windowed_priced_parse` 把结果按切片返回。
2. `crates/rar/src/codec/modern/lzss_huff/encoder/chunked.rs`：
   `mt_slice_symbols_row_index` 返回 token 流而不是字节；`encode_chunked_mt_with_progress`
   在 wave 收集后串行跑第 2 段（复用 `EncoderState` 的 `dist_cache` 与价格表）。
3. `crates/rar/src/codec/modern/lzss_huff/encoder/filter.rs`：
   `encode_with_filters_mt` 同样两段化（它现在是 `encode_with_filters` 的整段并行版）。
4. 先只对**单成员、非 solid** 生效（`entry_len == 0 && !is_final_chain`），solid 链
   继续走现有路径，避免一次改动牵扯两处状态机。
5. 验收脚本：`RAR_RS_MTTIME=1` 交叉重测（基线-改动-基线），并对比 ntdll 的
   `5,886,435 → 5,886,560` 这一档官方差距。

## 后果

- 正面：MT 的比例损失降到官方量级，`RAR_RS_MT_MIN_LEN` 可以从 8 MiB 降到 1–2 MiB，
  1–8 MiB 档的成员也能吃到 MT 的 3x 速度。
- 负面：解析与 emit 之间多一份 token 缓冲（每 token ~24 B，ntoskrnl ≈ 32 MB）；
  两段式让「解析期定价」这个 ADR 0011 的核心假设作废，`TokenPrices` 的用法要重新界定。
