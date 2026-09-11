# 13 — collect 带宽/延迟判定 + 远带候选预算（lever 4）前缘

## 背景

seq 解析比 WinRAR 慢 ~4-5x（tsc 9.05s vs 0.43s at m3/mt8）。goal：定位是 DRAM 带宽
受限还是延迟受限，并量化"减候选"（lever 4，往 WinRAR 级 m3 开关走）的 ratio↔speed 前缘。

## 工具改动（本次为测量做的临时仪器）

- `match_finder.rs`：`PROFILE_COLLECT`（编译期开关，默认 off）+ 三个全局原子计数器
  `STAT_DESCENT_STEPS`（`descent` 每步）、`STAT_QUERIES`（`matches()` 每次）、
  `STAT_SEED_INSERTS`（encoder seed 循环）。`dump_collect_stats` 在 `RAR_RS_COLLECT_STATS=1`
  时打印墙钟/步数/查询/步每查询/ns 每步，seq 与 MT 出口各一次。
- **教训**：测量阶段先开着原子计数器跑 mt8，text 卡 7242ms vs 干净 3426ms —— 统计原子
  行在 8 核 ping-pong，直接把测量污染 ~2x。结论：此类计数只能单人跑、或编译期开关切两档
  （单线程计数 / 干净测速）。
- `mtwin.rs`：增加 file 语料模式（`size file threads path`）+ 线程列表参数。已保留（通用）。

## profile 结果（text 24 MiB，m3）

干净缩放（无计数）：

| 线程 | 墙钟 | 缩放 vs mt1 |
|---|---|---|
| seq | 23254ms | — |
| mt1 | 17957ms | — |
| mt2 | 9335ms | 1.92x |
| mt4 | 4774ms | 1.96x |
| mt8 | 3426ms | 1.39x |

计数（单线程无争用）：seq 499M 步 / 22.9s = **45.9 ns/步**；mt1 436M 步 / 18.4s =
42.3 ns/步；mt8 聚合吞吐 **121M 步/s**（mt1 24M，8 核只放大 5x）。

**判定**

1. 单线程：45.9 ns/步介于 L2(~10ns) 与裸 DRAM(~90ns) 之间 —— 延迟受限但不少步击中
   L2/L3，非纯 DRAM。
2. 多线程：每次 descent 步散读 ~4 条 64B 行（son 对 + input 两侧 + head）；121M 步/s
   × ~256 B ≈ **~31 GB/s ≈ 内存带宽上限**。这就是 4→8 只涨 1.39x 的悬崖。
3. 结论熵：**杠杆 3（collect/DP 软件流水）价值低**（DP 在 mt8 只占 ~0.3s 可藏；
   单线程无法流水）。**杠杆 4 方向对**（砍步数=砍流量+砍延迟），但要砍在主瓶颈上。

## lever 4 实现：远带候选预算

`TreeMatchFinder` 增加 `far_start / far_cut`（运行时字段，默认 0,0=off；env
`RAR_RS_FAR_BAND=start,cut` 在 `parse_block_chunk` 构造 finder 处接线）。`descent`
中，候选距离 `pos - current > far_start` 进入远带后，第 `far_cut` 步像预算耗尽一样
密封两个挂点返回 —— 丢弃远带候选窗口尾部，近带全精度保留。off 时字节与现状完全一致
（已用 tsc 验证：off → 8093296 字节复现）。

## 前缘数据（tsc 23 MiB 真实 x86，m3，dict 16 MiB）

| config | seq ms | seq % | Δpp | mt8 ms | mt8 % |
|---|---|---|---|---|---|
| off | 9056 | 33.01% | — | 2377 | 33.50% |
| 1M/8 | 9019 | 33.02% | +0.01 | 2311 | 33.50% |
| 1M/4 | 8716 | 33.10% | +0.09 | 2300 | 33.50% |
| **1M/2** | **8235** | **33.23%** | **+0.22** | **2283** | **33.51%** |
| 256K/2 | 7456 | 33.55% | +0.54 | 2255 | 33.68% |

text 16 MiB：1M/2 → seq -16.9% / +0.39pp（文本类远带匹配密度高，截断损失大）；mt8
-3.8%。distant 16 MiB：1M/2 无损（seq 572ms 与 mt8 150ms 的 ratio 均逐字节不变 ——
深拷贝由 LR/头种子负责，深树下降本就 <2 步）。

## 结论

1. **1M/2 是前缘甜点**（每 pp 换 41% seq 速度），但任何时候都产生 seq 字节变化，不能做
   默认 —— 只能做 opt-in 速度档，且要对比照 WinRAR 的 ratio 余量（tsc 上余量未测）。
2. **它对 mt8 悬崖收效甚微（-4%）**：MT slice 树只覆盖片内 ~4MiB + 近窗，远带候选本就
   稀少，截断打不到 MT 的带宽大头。MT 已达 85% 效率，墙是"总步数 × ~4 行/步"的绝对
   DRAM 流量 —— 要像 WinRAR 快 5x 必须把**每位置步数**降 ~5x（架构级），而非削尾。
3. 因此 lever 4 的远带预算不是 mt8 的解药；它只是 seq 单线程的温和加速档。真正能降
   每位置步数的候选仍只有：issue 09 双环近存（近带 L2/L3 驻留，撞候选顺序墙）与
   **接受 MT 分歧、把 MT slice 解析做成独立的低步数搜索**（WinRAR 式 m3 检索）。

## 待办

- 决定方向：issue 09 双环重试（保候选顺序/降流的近存结构）；或 MT-only 低步数 m3 搜索
  （显式 MT 输出分歧，seq 契约不动）；或 RAR_RS_FAR_BAND 作为 opt-in 速度档就绪。
- 清理：PROFILE_COLLECT 与三个原子计数器、`RAR_RS_FAR_BAND` 开关在定论后移除或将
  远带预算升格为正式选项。

## 定论（2026-09-08）：MT-only 低步数 m3 搜索落地，方向 2 胜出

选了待办第一行第三个方向：**接受 MT 分歧、把 MT slice 解析做成独立的低步数搜索**
（WinRAR 式 m3 检索）。实现与 A/B：

### 实现

- `encode_mt_slice` 的低步数分支（`mt_slice_symbols_low_step`）：hash-chain
  greedy+lazy（`find_matches_in_range`），链预算 `MT_LOW_STEP_CHAIN = 16`
  （远景：m3 的 96 会重演被树替换前的老失败）；窗口帧仍是 `state.tail + slice`
  与 LR 共享只读表绝对锚点。seq 路径不动（`find_matches_optimal` 仍只在 seq 用）。
- 链数组跨 slice 复用：`MatchFinder::reuse`/`into_parts`（head/prev 两数组缓存进
  `EncoderState.chain_parts`），避免每 slice 一个 64 MiB `prev` 新分配——但随机数据
  的回归是搜索本身（链逐字节 insert vs 最优路径 matchless 快路径跳走），不是分配；
  实测复用对随机无变化。
- 由 env-gated A/B（`RAR_RS_MT_LOW_STEP=1`）测通后**升格为 MT 默认**（删 env 与
  flag 参数，直接调低步数分支）。

### A/B 结果（mt8 m3，本机，interleaved medians）

| 语料 | 最优 MT | 低步数 MT | 速度 | ratio Δ |
|---|---|---|---|---|
| tsc 24 MiB x86 | 2791 ms / 33.50% | 1569 ms (chain16) / 35.93% | **1.78x** | +2.43pp |
| text 17 MiB | 1075 ms / 4.01% | 238 ms / 4.50% | **4.5x** | +0.49pp |
| rand 16 MiB | 141 ms / 100.0% | ~420 ms (chain16) | 0.34x | ~0（STORE 兜底） |

- chain 8 再快一档（tsc 1340 ms / +2.85pp，text 205 ms）+0.42pp 更多——按用户
  决定维持 16（速度快、ratio 损失中位）。
- chain-8/chain-16 均 decode 字节级回环通过（tsc/text/rand）。
- vs WinRAR m3/mt8 锚点：text 251 ms/3.97% —— 低步数后我们**更快**（238/205 ms）、
  ratio +0.53pp；tsc 517 ms/30.89% —— 仍 2.6x 慢、ratio +5.04pp 差（DRAM 带宽悬崖，
  架构级，见下）。

### 结论

1. **mt8 每位置步数降 ~5x 的目标在 MT-only 档内兑现量级**：可压缩语料 1.8-4.5x，
   代价是 MT ratio 漂（tsc +2.43pp、text +0.49pp），且 MT 输出再进一步偏离 seq
   （本已是文档化接受的分歧）。WinRAR 的 tsc m3 是 517 ms/30.89% —— 我们既慢
   (2.6x) 又 ratio 差 (+5pp)，差距是引擎级（每次 descent 散读 ~4 行 vs WinRAR
   的链/紧凑窗口），低步数档已到 MT 收益前缘。
2. **远带预算（RAR_RS_FAR_BAND）不是 mt8 解药** 维持：MT slice 树远带候选稀少，
   削尾打不到带宽大头。它仍是 seq 的 opt-in 温和加速档（留档）。
3. **随机语料 3x 回归是有界且可接受的**：此类输入 ratio ~100% 走 STORE 兜底，
   实际写出路径不亏；回归源于链路径逐字节 insert 没有最优路径的 matchless
   快路径跳走（0.34x 是文档化 MT 行为的一部分）。
4. 遗留：PROFILE_COLLECT 原子计数器与 `RAR_RS_FAR_BAND` 仍 env-gated（off）——
   tsc 字节级复现已验证 off 状态安全，留作可选测量/速度档为定论后的合理状态。