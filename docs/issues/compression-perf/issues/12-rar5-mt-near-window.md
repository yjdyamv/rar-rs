# 12 — RAR5 MT: 为什么字节同等不可行 + 近窗对齐实验

## 背景

RAR29 单大成员已按 **字节同等** 原则做块级 MT（analyze 并行 + serialize 顺序，
`crates/rar/src/codec/legacy/rar29_encoder.rs`）。问：RAR5 的 MT 能否同样重构？

**结论：不能。** RAR29 的格式是 **块本地** 的，RAR5 的格式不是。

## 结构差异（判断依据）

| | RAR29 (RAR4 codec) | RAR5/RAR7 (modern codec) |
|---|---|---|
| match 距离编码 | 流里显式（distance 树），解码器无运行缓存 | 隐式 rep cache：`dist_cache[4]` + `last_length` 复用 |
| 解码器状态 | 无 | 每次 token 后 `dist_cache_touch/push`（`decoder.rs:240-353`） |
| token 分类 | 与位置无关 | `Match` vs `CacheRef` vs `Repeat` 是格式内容，依赖运行 cache |
| 解析依赖 | f(窗口, 输入) | f(窗口, 输入, 运行 cache)；cache(i) = 之前所有发射的函数 |
| 块间链态 | 输出侧：424 B levels + keep/delta 决策（序列化层） | 内容侧：rep cache 影响 token 本身 |
| 并行拆分 | analyze（token+表）纯独立，serialize 顺序 | 解析是带长程状态的全序列 DP，无干净块分解 |

RAR29 拆法成立是因为 analyze 的产物（token 流）由窗口独立导出；RAR5 的 analyze
产物本身就是顺序依赖的。严格字节同等 ⟺ 整成员串行（WinRAR 自己的单文件 MT 也是
按片分窗、输出随线程数变化）。

## 现有 MT 的三个分歧源（相对 seq）

1. **每 slice 空 rep cache 起解析**（`encoder.rs:579`、`:669-670`）：slice 开头本可
   `CacheRef/Repeat` 前片距离被迫写成显式 match —— 主要分歧源（text64 6554 vs
   seq 6058、dll mt8 +3.3% 的主因）。
2. **近窗 2 MiB vs seq 8 MiB**（`encoder.rs:649` `want = (2 MiB).min(dict)` vs
   `NEAR_WINDOW_MAX`=8 MiB，`find_matches_optimal` 的 `keep` 到 `NEAR_WINDOW_MAX`）：
   slice 内 2-8 MiB 距离的 match 只能靠采样的共享 LR 表，可能丢失。
3. **slice 边界强制断块**（`:701` `find_block_end_adaptive` 作用在 slice 内部 symbol
   流上；自适应 slice 尺寸 `cs = data.len()/(threads*2)` ≠ seq 的 4 MiB chunk）。

## 实验：近窗对齐

嫌疑：把 `encode_mt_slice` 的近窗从 2 MiB 提到 `NEAR_WINDOW_MAX`（8 MiB）——树窗口
与 seq 对齐，2-8 MiB 距离的 match 不再依赖 LR 采样。代价：MT 每 slice 是 fresh 树 +
满尾 seed（budget-limited `chain_len.min(4)`），8 MiB 尾 seed 成本是 2 MiB 的 ~4 倍
（文本/dll 走完整 seed，随机数据被 `mt_tail_is_incompressible` 门控跳过）。

度量（`crates/rar/examples/mtwin.rs`，dict_log 7=16 MiB，m3/m5，mt1/mt8 vs seq）：
- `text`（48 MiB 散文）：观测代价（速度）与小幅收益。
- `distant`（48 MiB 随机 + 1/2/4/6 MiB 深处周期性拷贝）：针对 2-8 MiB 带宽的收益。

## 实验结果（24 MiB、dict_log 7、m3/m5，mtwin）

### 基线（近窗 2 MiB，改动前）

| 语料 | level | seq | mt1 | mt8 |
|---|---|---|---|---|
| text | m3 | 22483ms · 14.70% (3698500) | 17659ms · 15.23% (3832586, +134086) | 3307ms · 15.52% (3906677, +208177) |
| text | m5 | 34002ms · 14.65% (3687906) | 28127ms · 15.15% (3811435, +123529) | 5250ms · 15.41% (3879275, +191369) |
| distant | m3 | 696ms · 11.75% (2957565) | 590ms · 34.14% (8592820, +5635255) | 202ms · 59.14% (14882505, +11924940) |
| blockdup | m3 | 1380ms · 40.67% (10235809) | 1508ms · 45.36% (11415052, +1179243) | 650ms · 45.88% (11546531, +1310722) |

`distant` = 随机段 + 1/2/4/6 MiB 深处精确拷贝（探针判尾不可压缩 → 连树 seed 都跳过，
近窗大小无关）；`blockdup` = 每窗 256 KiB 图案头 + 随机 + 2/4/6 MiB 精确拷贝（尾可压缩）。

### 近窗 8 MiB，全量 seed（`want = NEAR_WINDOW_MAX`）

| 语料 | level | seq | mt1 | mt8 |
|---|---|---|---|---|
| text | m3 | 23162ms · 14.70% | 18343ms · 15.23% | 3736ms · 15.52% |
| blockdup | m3 | 1208ms · 40.67% (10235809) | 3676ms · 40.67% (10235943, +134B) | 2254ms · 40.67% (10235765, -44B) |

- text / distant：输出 **逐字节不变**（近处 match 获胜时多 6 MiB 树无影响；distant 探针
  门控不种树，连带轮廓不动）。text 单纯变慢 mt8 3307→3736ms（+13%）。
- blockdup：MT 质量**收敛到 seq**（mt1 +134B、mt8 -44B），但 mt8 2254ms（基线 650ms，
  ~3.5x 慢），仍 2.1x 快于 seq。

### 远端 seed 参数化（stride / chain / frontier），保持 8 MiB 可达

`MT_NEAR_TIGHT_FRONTIER=2 MiB`（近端 dense）、`MT_FAR_SEED_CHAIN=2`，仅 MT 路径
（`lr_shared.is_some()`），seq 维持 dense 全量 seed（契约）。

| stride | blockdup m3 mt8 | blockdup m3 mt1 | ratio (mt8 vs seq) |
|---|---|---|---|
| 1 (全量) | 2254ms | 3676ms | 字节收敛 (-44B) |
| 4 | 1060ms | 2080ms | 字节收敛 (-44B) |
| 8 | 871ms | 1837ms | 字节收敛 (-44B) |
| 16 | ~900ms | ~1700ms | 字节收敛 (-44B) |

text m3（stride 16）：mt8 3404ms（近基线 3307，回归 ~3%）、ratio 与基线逐字节相同；m5
同构。distant 因探针门控不动（mt8 59.14%，另行处理）。

**取舍结论**：步长 16 保留 8 MiB 可达带来的质量（blockdup 类收敛到 seq（+5.2pp 修复）），
把全量 8 MiB 的 3.5x 速度代价压到 ~1.4x（相对 2 MiB 基线）。远端拷贝锚定只缺 stride 内
几个前缀字节并被最优解析扩展，语料级输出字节不变。

## 待办（若继续）

- distant 的剩余修法：把 `mt_tail_is_incompressible` 探针从"只看尾头 256 KiB"扩到全尾
  采样，可压缩尾部区域（如深处的精确拷贝本身会形成重复窗口）即可触发 seed，才能让近窗
  8 MiB 作用于随机介质类语料。
- rep cache 缝（分歧源 1）仍是最大头，不属本 issue。

## 结论（论据扩展）

- **保留** 8 MiB 近窗 + 远端 stride 16 / chain 2 / frontier 2 MiB。MT 质量对 blockdup 类
  （可压缩尾 + 2-8 MiB 精确拷贝）从 +5.2pp 回归到**与 seq 字节收敛**，代价 mt8 ~1.4x
  speed 于 2 MiB 基线（全量 8 MiB 是 3.5x，text 类语料 ~0% 代价）。
- rep cache 缝的修复（wave 内 cache 携带 = 串行，无收益；缝头 re-emit = 启发非精确）
  不作为字节同等途径。
- WinRAR 自身的单文件 MT 同样是"已知分歧"模式，字节同等不是工业常规。

## 已被 issue 13 取代（2026-09-08）

MT 已切到低步数链 tier（`mt_slice_symbols_low_step`，issue 13 定论）：worker 不再建/
种树，本 issue 的近窗对齐实验（8 MiB 近窗 + stride 远种）作为历史留档；stride 形状在
`find_matches_optimal` 里休眠（其 `lr_shared` 门在纯 seq 调点上恒 false）。近窗对齐的
核心收益——2-8 MiB 精确拷贝不落 LR 采样——由低步数 finder 直接种 8 MiB tail 承载。