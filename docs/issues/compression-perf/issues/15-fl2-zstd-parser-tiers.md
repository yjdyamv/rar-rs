# issue 15 — 借 Fast LZMA2 / zstd 的价格驱动解析思路提速

状态：开放（2026-09-18 立项，有意做）。目标：**压缩速度**；比率按
[`map.md`](../map.md) 的契约守住（默认档 packed 不得变大）。

## 参照物（上游事实）

Fast LZMA2（`conor42/fast-lzma2`，README 原文）基于 Igor Pavlov 的
LZMA2（7-Zip），用**并行缓冲的 radix match-finder** 加一部分 zstd
的优化，目标是相对 7-Zip 默认 LZMA2 "a 20% to 100% speed gain at the higher
levels"，代价是 "a small loss in compression ratio"。

它的匹配器是 _block_ 算法：要拿到与 7-Zip
相当的比率需要约双倍字典（抬高解压内存），同字典下产物大约大 1%–5%；另有一个
high-compression
选项在较小字典上换回比率。好处是匹配器能用简单线程模型多线程、不把输入切成需要复制表/链的大块，每线程额外内存
"typically no more than a few megabytes"。v0.9.1 还加了 incompressibility
checker，对高熵数据约快 2x。

zstd 侧（`zstd_opt.c`）：最优解析的价格每次从频次数组重算（`ZSTD_getMatchPrice`
/ `ZSTD_litLengthPrice`），上游有把价格分量制表缓存的提议；`optLevel`
用于分级（低档偏好小 offset，利于解压时的缓存局部性）。FL2 对外提供 fast / opt /
ultra 三档策略。**实现前先读上游源码核对**档位命名与价格表公式。

## 我们的现状（已核对源码）

- 顺序/未过滤（`codec/modern/lzss_huff/encoder/chunked.rs`、`codec/modern/lzss_huff/encoder/parse.rs`）：`-m1`
  = 哈希链 greedy+lazy（`find_matches_with_tail`）；`-m2`–`-m5` = 一次 DP 加
  `OPTIMAL_PARSE_PASSES`（0/2/2/3/4）次重定价，匹配用 BT4 风格
  `TreeMatchFinder`（`son` 树），长程靠采样表 `LongRange`。

- 过滤成员（x86/delta 等，`codec/modern/lzss_huff/encoder/filter.rs`）：level ≥
  2 走 `find_matches_optimal`，level 1 走哈希链。

- MT：所有档位都由 `mt_slice_symbols_low_step` 以哈希链 greedy+lazy（链预算常量
  16）解析切片——即 map 里「文档化接受的分歧」，也是低步数档位的落点。

- 价格：`TokenPrices::match_cost` 对每个候选调 `EncoderMatchState::encode_match`
  现场算 slot/extra，再查码长。

## 杠杆

### A. 不改产物字节（先做；验收 = 逐字节相同）

1. **价格分量制表**：把 `len_slot` / `dist_slot` / `ldc`
   相关分量按当前价格表缓存成表，内循环只做查表与加法；对应上游那个制表提议。

2. **可压性预检更便宜**：降低 `whole_member_is_incompressible`（缓冲成员全扫）与
   `sample_is_incompressible_file`（采样探针）的成本，并让同一份采样逻辑同时充当
   issue 04 的窗口级跳过判据。

### B. 改产物字节（只能是新档或 MT 档，须文档化并过比率门）

3. **价格驱动的中间档**：填 `-m1` 与 `-m2` 之间的断崖——「有界候选 +
   一次定价的便宜 DP」。优先级最高的是装进 MT 低步数层：同速换比率，把 map
   记录的 ~0.6–2.9pp 收回来（MT 偏离 seq 已是接受的分歧，seq 不受影响）。

4. **匹配器行化**：给快档加「按哈希分行的有界深度 finder」（RAR15
   已用同形状的扁平 CSR 行索引），BT4
   继续留给默认比率档；不得为速度改默认档的下降语义（issue 09）。

## 硬约束与验收

- 默认 `-m` 各级产物字节不动（map：「比率是契约」）；改字节的档位先以
  `RAR_RS_FAR_BAND` 这类 env 开关做实验，再决定是否提升为文档化档。

- 验收：`examples/mtprobe.rs`、`examples/ratiocheck.rs`、`examples/perfbench.rs`
  加 map 的对拍语料；默认档 packed
  不得变大；新档必须给出成对的「同速下比率」或「同比率下速度」数字；MT
  的跨线程确定性不变。

- **不搬** FL2/zstd 的熵编码器与格式：RAR 的流是 LZSS+Huffman
  表、价格按码长而非频次分布，能搬的只有解析器/匹配器/线程模型与预检这类工程手法。FL2
  的「1%–5% 比率损失换速度」只允许出现在新档，默认档不接受。
