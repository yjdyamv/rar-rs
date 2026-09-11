# 14 — 两制近存（issue 09 双环设计）实测为负；代码已移除，本文档为负例记录

## 背景

issue 13 判定 mt8 带宽受限（121M 步/s × ~256 B/步 ≈ 31 GB/s ≈ DRAM 上限），seq 是
延迟/缓存混合受限（~46 ns/步）。issue 09 曾设计"双环近存"：近带字节序小树（窗口
W_n，设计默认 256 KiB）L3 驻留 + 远树（窗口余量）随到达过期插入，候选并集完备无门控
（不撞候选顺序墙）。本次实测该设计。

## 实现（env-gated，默认 off）

`TreeMatchFinder` 增加 `near_window / far: Option<Box<TreeMatchFinder>> /
pending: VecDeque<u32> / scratch`；`descent` 拆分 `node`（插入位置）与 `pos`（查询
波前）参数（环链/前缀比较用 node，距离预算/距离报告用 pos）；新增 `far_query`（远树
只读下降，镜像 fused 的候选序列但不重建树）与 `drain_expired`（到龄 t=pos−W_n 时把
near 树里刚复用的位置以自身字节重新 `descent` 进远树）；`matches`/`matches_seeded`
尾部分发近+远并集（near 候选先 push，far 后 push，距离严格递增）。`grow_to` 两制分支：
near 环 = W_n（分配也按 W_n 紧缩至 L3）、far 环 = 全窗口（dist=全窗口）；
`rebase`/`clear_head` 递归 far + shift 丢弃 pending。env `RAR_RS_TWO_TIER=<bytes>`
在 encoder 构造 finder 处接线（默认关）。off 路径与重构前字节完全一致（tsc seq
8093296 B / 33.01% 精确复现，区别于只加了 `>= dist` 守卫——语义与旧 `> mask` 相同）。

## 测量（tsc 23 MiB x86，m3，dict 16 MiB，env 开时 W_n=256 KiB）

| 阶段 | seq ms | seq % | mt8 ms | mt8 % |
|---|---|---|---|---|
| 控制（env off） | 9783–10190 | 33.01% | 5198 | 33.50% |
| 两制 + 远插满预算 | 18037 | 33.11% (+0.10pp) | 10269 | 33.38% |
| 两制 + 远插 budget=4 | 12704–12893 | 33.55% (+0.54pp) | 8654–8843 | 33.60% |

（首版发现 `drain_expired` 以 `cut=0` 调 `descent`，预算守卫首迭代即返回——远树实际
一直是空的，远带候选缺失 → seq 33.89%/+0.88pp。修后并集恢复，ratio 回到 +0.10pp。）

## 结论

1. **远树重插是毒药**：每个过期位置都必须以自身字节完整重新 `descent` 进远树，每位置
   ≈ +1 次额外下降（query 之外）。seq 是步数×延迟，mt8 是步数×带宽——两制把"每位置
   步数"做大了 ~2-3x（近下降 + 远查询 + 远插），不是缩小。实测 seq -75%、mt8 -98%，
   方向性地败。
2. **近带 L3 驻留卖点不成立**：单树近带步本就读同一 son 数组里位置局部的一段（结点槽
   = 位置 & mask，近带候选的槽恒在数组内 0.5 MiB 热段），今日已近似 L3 命中；两制额外
   付出的远查询/远插没有对价。
3. budget=4 的采样式远插削掉了远插大部分开销，但 ratio 漂到 +0.54pp（超 +0.5pp 预算）
   而 mt8 仍 -66%——采样本身又回到 lever 4 的"削尾换 ratio"老路（且环比远带预算更差）。
4. 任何"树内降步数"的尝试都需要付出重建树的代价；今日单树 8-16 MiB son 的热段已驻留。
   以目前的测量，**两制方向废弃**。

## 收尾（2026-09-07，用户拍板"保留文档移除代码"）

- 全部两制实现代码已移除：`near_window / far / pending / scratch / dist` 字段、
  `set_two_tier / with_far_band 以外的 grow_to 两制分支 / grow_ring / far_query /
  drain_expired / two_tier_finish` 方法、`descent(node,pos)` 拆分已还原为单一 `pos`
  （守卫还原为 `> mask`，与旧语义一致）、`matches`/`matches_seeded` 的两制分发、
  encoder 的 `RAR_RS_TWO_TIER` env 接线、`STAT_TWO_TIER_QUERIES / STAT_FAR_WINS /
  TWO_TIER_NEAR` 探针（含 dump_collect_stats 的 two-tier 打印）。off 路径与移除前字节
  恒等（rs 测试全绿：65 codec 单测 + 34 rar50_roundtrip + 3 format_assertions +
  4 quick_open_listing + 25 rewrite；clippy 仅剩预存在 9/7 告警）。
- 保留：`RAR_RS_FAR_BAND`（lever-4 探测，seq opt-in 速度档待排期）与 `RAR_RS_COLLECT_STATS`
  (`dump_collect_stats`)、`PROFILE_COLLECT` 直方图仪器（发布前移出或保持默认关）。
- 本文档保留作为负例与复现路径（含上述测量表）。

## 待办

- 鉴权结论归档：接近悬崖的可行杠杆只剩 issue 13 两点——(a) seq opt-in `RAR_RS_FAR_BAND`
  速度档就绪待排期；(b) 接受 MT 分歧、把 MT slice 解析做成独立的低步数 m3 搜索（每位置
  步数降 ~5x 才够），这是确定性方向但改动面在架构级。
- 提交姿态：`RAR_RS_FAR_BAND` + `dump_collect_stats`（`RAR_RS_COLLECT_STATS=1`）均为
  实验仪器，发布前应移出或至少保持默认关。