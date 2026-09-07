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