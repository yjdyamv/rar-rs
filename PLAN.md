# rar-rs 计划

> 最后核对：2026-09-28 @ `a5e6685`；实现细节以源码为准。

本文件只留**下一步**与**当前判断**。规则、契约与「别改回去」的地雷在
[`docs/PITFALLS.md`](docs/PITFALLS.md)；模块地图与设计不变量在
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)；术语在
[`CONTEXT.md`](CONTEXT.md)；命令行与退出码在
[`docs/CLI.md`](docs/CLI.md)。**过程与逐批验证记录在 git 历史** （按主题
`git log -- <path>`，按结论 `git log -S<string>`），本文件不维护 CHANGELOG。

## 现状

- **RAR5 / RAR7**：创建与读取全功能对齐 WinRAR 7.23——压缩（m0–m5、DP
  最优解析）、 `-hp` 头加密、分卷、solid、内联恢复记录、`.rev`
  恢复卷、quick-open、NTFS ADS、三时间戳、owner、`-mt` 多线程、长距离匹配、v70
  大字典。
- **老容器族读取（RAR 1.3–4.x）**：三代解码器（RAR29/20/15）+ PPMd + 五大标准 VM
  过滤器 + 通用 RARVM 解释器，solid 链、分卷、`-hp`、各代数据解密。
- **RAR4 创建全能力**：LZSS m1–m5 + PPMd + 六大标准 VM 过滤器 + `-hp` + 多卷 +
  solid （链内亦应用 VM 过滤器与 PPMd 模型延续）+ 并行 batch + 单大成员块级
  MT（字节同等）+ NEWSUB 恢复记录。能力表见
  [`docs/rar4-creation-spec.md`](docs/rar4-creation-spec.md)。
- **RAR 1.3 / 1.4 / 1.5 / 2.x 创建**：`-ma13` / `-ma14` / `-ma15` / `-ma2`，含
  solid、 `-p` / `-hp`、旧命名分卷。
- **RAR4 编辑全补**（ADR 0005）：头/块级操作 + 非 solid 块拷贝 + solid 整档
  repack； `-hp` 与分卷（`rn`/`ch`/`k`/注释）均已支持。
- **命令面**：官方 `rar` 全部命令（含 `rv` 补恢复卷、`lb/lt/vb/vt`
  列表变体）；官方 7.30 新增的 `la`/`va`/`lba`/`vba` 与服务块列表已接。
- **工程**：workspace 三 crate；CI 做 fmt / 路径分隔符守卫 / cargo check（含
  wasm）/ 确定性测试 smoke / 版本一致性 / clippy `-D warnings` / cargo deny /
  rustdoc；七目标 fuzz；取消钩子；QO 快路径；流式修复；零填充分卷。

## 下一步

### P0 发布收口（唯一阻塞项）

- [ ] **发布 `rar-rs` 0.12.0**：`cargo package` 已通过校验，三个 crate 的
      `readme` / `keywords` / `documentation` / `categories`
      元数据已补齐。`rar-cli` 与 `rar-rs-napi` 依赖 workspace 内的
      `rar-rs`，**必须先发布 `rar-rs`** 才能发布另外两个。
- [ ] **给发布建一张检查单**（本文件的「发布清单」段已列出五处版本一致性、两个
      `Cargo.lock`、tag
      校验、下游发布顺序与文档锚点刷新）。清单化之后每一步都可勾选，
      不再依赖记忆。
- [ ] **发布后做一遍下游实测**：从 crates.io 干净拉取 `rar-cli` 的一个临时工程，
      `cargo install rar-cli` 能跑；`rar-rs-napi` 的 `.node` 与 wasm 产物照 CI
      release job 的路径复核一次。

### P1 文档人体工学（本次已做，保持即可）

- [x] **单一来源归位**：历史修复日志移出 `PLAN.md`，可长期复用的规则收进
      [`docs/PITFALLS.md`](docs/PITFALLS.md)（一条一行、由测试钉住），架构级不变量留在
      [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)，退出码进
      [`docs/CLI.md`](docs/CLI.md)。`PLAN.md` 从 995 行收到 200 行量级。
- [ ] **保持**：新结论写进
      PITFALLS（规则）或本文件（下一步），**不要把过程写回来**。
      文档锚点（`最后核对`）在改行为时同步刷新。

### P2 性能（已 park，未关闭议题）

- [ ] **issue 09 — DLL 单线程解析速度**：真实 DLL 上 m3 `-mt1` 落后 WinRAR 约
      5.9x，瓶颈是 BT4
      下降步数（结构锁定：`HASH_BITS`、dict-log、提交阈值、近/远带宽四个旋钮已验证弹回）。
- [ ] **issue 15 — 价格驱动解析提速**（2026-09-18
      立项）：目标是**压缩速度**——给解析器补中间档，把每位置代价从更深的搜索换成更省的价格计算。四个杠杆、上游事实与验收判据见
      [`docs/issues/compression-perf/15-fl2-zstd-parser-tiers.md`](docs/issues/compression-perf/15-fl2-zstd-parser-tiers.md)。
- [ ] **issue 04 — MT 随机数据窗口级不可压缩跳过**：成员级 STORE
      兜底已让随机数据领先 WinRAR 10–80x；窗口级跳过有把边界成员从压缩翻成 STORE
      的比率风险，需先有边界语料量化。

**压缩性能契约（动解析器之前先读）**

- **比率是契约**：任何提速必须让标准语料（text / mixed / xml / sparse +
  random）的 packed 字节不变或变小。
- **优先字节相同的快路径**，而不是启发式。
- **先测再优化**：`mtprobe` / `ratiocheck` 示例是回归闸门，热点需先由探针确认。
- **发射块策略只有一个 owner**：`EMITTED_BLOCK_SIZE` + `find_block_end_adaptive`
  在 `codec/modern/lzss_huff/encoder/parse/block.rs`。
- **`-mt` 低步数搜索是已接受的取舍**（ratio 换速度），不是待修的 bug。

**已否决方向（实测为负或结构不可行，别重试）**

| #  | 方向                                | 判决                                                                       |
| -- | ----------------------------------- | -------------------------------------------------------------------------- |
| 07 | 字面量密集块跳过重定价              | 否决：闸门只在解析本来就很便宜处触发，却改变了 DLL 字节                    |
| 10 | 2–3 字节短匹配                      | 否决：短匹配槽码长依赖频率 bootstrap，两遍定价无法安全复现，各变体都掉比率 |
| 14 | 两制近存（近带 L3 驻留 + 远树重插） | 实测为负（seq −75%、mt8 −98%），废弃                                       |
| 06 | 成员级 solid MT                     | 结构不可行（无法跨成员并行共享窗口）；chunk 级 MT 已落地                   |
| 09 | BT4 字节级流水                      | 已到顶（首步 value-carry −3.6% 即全量）；再降步数只剩显式取舍项            |
| 13 | 远带候选预算 `RAR_RS_FAR_BAND`      | 仅 seq opt-in 有效（−9% @ +0.22pp），非 mt8 解药；现休眠                   |

已落地（别再重新论证）：01 matchless-block DP 快路径（字节相同）、02 collector
fast-mode 门（`longest==0`，阈值 256）、03 delta 候选通道 + 采样预门、05
流式路径 auto delta/x86、08 持久树跨 chunk 增长的损坏修复、11 BT4 首步
value-carry（−3.6%）、 12 MT 近窗对齐（已被 13 取代）。

**剩余公开差距**：DLL 单线程解析约 6–8 s vs WinRAR 1.8 s（其中 mt8 7.5×，见
issue 09）；xml m2/m3 +1.5%（解析差距，非块开销）；text64 MT 片间分歧（6554 vs
seq 6058 B）。各日期、各口径的实测表是**过程记录**，需要时
`git log -- docs/issues/compression-perf/` 找回。

### P3 功能缺口（按需，不阻塞发布）

- [ ] **RAR4 solid 归档 MT**：legacy solid
      链保持串行；成员级并行需跨成员共享窗口，属结构性代价（RAR5 的 chunk 级 MT
      已兑现）。
- [ ] **逐文件与上游对拍**：`rars` 移植的出处目前是 in-tree claim（每个文件头 +
      inventory 表），尚未与上游 diff。不影响已定的许可表达式。

### 有意不做（设计决定，别当缺口修）

- **老编码器块级 MT（v15/v20 单个大成员）**：v20 已有「多窗口并行解析 +
  顺序写位流」 的骨架，但需要每窗口独立的 match finder
  状态与确定性窗口边界才能保证字节一致，收益 仅「老格式大成员的创建速度」（官方
  7.23 已移除 `-ma4`）；v15 另受自适应表限制。
- **PPMd 块级
  MT**：单自适应模型，切块会改变输出（结构上不可行）；成员级并行已有。
- **RAR13 非 solid 成员级 batch**：成本低但价值最低（DOS 时代格式）。
- **把容器族 / recovery 做成编译期 feature**：不把 legacy 族（`codec/legacy` +
  `format/{rar13,rar4}`，21,000 行 ≈ 29%）或 recovery（`.rev`/RR，7,037 行 ≈
  10%）做成可选 feature。它们是产品范围本身——默认必须开启，feature 化对本仓库的
  CI、本地构建与发布产物**零收益**；代价是 50–90 处新 `#[cfg]`、CI clippy
  矩阵翻倍。真需要「只读 RAR5」的消费者应该用裁剪的 fork。（同类先例：ADR 0007
  删掉 `raw` feature。）

### 暂缓（等决策，不自行推进）

- **STORE 成员竞态**：单遍 STORE 先 `hash_file`
  再重读同一路径，同尺寸改写真可能写出旧 CRC/BLAKE2；回填头需要 patching（`-hp`
  还要重加密），已接受。
- **RAR5 主头 locator 的偏移宽度**：官方按写主头时对最终大小的内部估计预留（实测
  3–6+ 字节，阈值 2^9/2^16/2^23），那套估计无法由我们自身的头字节推出，**CLI
  不自动填**；库侧给了
  `WriterOptions::estimated_size(bytes)`，给出预计大小即按官方分档预留（`-m0`
  小归档与官方逐字节相同），不给则沿用历史定长 5 字节。默认仍比官方 +2 字节
  （最小档）到 −1 字节（≥256 MiB 档）。
- **官方默认为较大归档写 QO 记录**（实测 ~8 KB 起写），我们只按 `-qo`
  写。只影响字节外观（官方多一条 QO 服务块），不影响读取——无 QO
  时双方都回退全扫。`docs/CLI.md` 的「console 默认不写 QO」只对小归档成立。

## 开放议题

`docs/issues/<feature>/` 只留**未关闭**议题；关闭时判决并入本文件或
[`docs/PITFALLS.md`](docs/PITFALLS.md)，并删掉文件。

- [`compression-perf/`](docs/issues/compression-perf/) —
  04（窗口级不可压缩跳过）、 09（DLL 解析速度）、15（价格驱动解析），均对应上面
  P2 的清单项。

## 一致拒绝（别"修"）

- **分卷 append / 分卷删除**：官方 `rar`
  同样拒绝（`Cannot modify volume`）。分卷的 `rn` / `ch`、`k`
  与归档注释**不是**拒绝项：官方支持，我们也支持。
- **分卷 + 内联恢复记录（`-rr`）**：**创建时已实现**——`-v` 配 `-rr`
  时每卷各带一份记录，`.rev` 可同时用（RAR5 与 legacy RAR4
  同形）；编辑一个**已带记录**的卷集时按原强度重建记录，不是拒绝项。
- **在 RAR4 卷集上显式改变恢复强度**（`rar rr <set> 20`、`-rr10`
  配已有卷集）：我们拒绝并提示「分卷用 `.rev` 恢复卷」。官方对自己的卷集一律
  `Cannot modify volume`，连逐卷记录都不重建，所以这里没有可对齐的行为。
- **PROTECT_HEAD（RAR 2.5 时代）记录**：不可就地编辑/追加，报错要求重建归档（见
  [`docs/PITFALLS.md`](docs/PITFALLS.md)）。

## 已知小差异（记录，互操作无碍）

- **RAR4 solid 归档的成员排序**：solid
  时官方按名字/扩展名启发式排序，我们按参数顺序，因此 solid
  归档无法逐字节对拍（载荷与链字典同样按各自实现）。非 solid 的头字段与 `-m0`
  已逐字节对齐。
- **`lb` 分片成员**：官方 `Rar.exe lb` 对跨卷成员不打印该成员名，官方
  `UnRAR.exe lb` 与我们一致；我们随 UnRAR。
- **目录条目名带尾斜杠**；**RAR4 solid 且无 `rarfiles.lst`**
  时的排序同上面第一条。
- **`-ts` 的「字母+数字」组合**、**`-rr<N>%` 取整**、**`rar rr` 恒 3%**、
  **`rar r` 不写 `fixed.<name>`**、**无控制台时的覆盖询问**：口径与理由见
  [`docs/PITFALLS.md`](docs/PITFALLS.md)「有意不追平官方」。
- **RAR5 元数据的仍差两处**（字节外观，双向读写一致）：locator
  偏移字段宽度与「官方默认为较大归档写 QO 记录」，见上面「暂缓」。
- **Windows 联接点的 redirect 目标字符串**：见
  [`docs/PITFALLS.md`](docs/PITFALLS.md)「有意不追平官方」。

## 发布清单（改版本号时逐项过）

1. 五处版本一致：`crates/rar`、`crates/rar-cli`、`crates/rar-napi` 的
   `[package] version`，`crates/rar-napi/package.json`，根 `Cargo.toml` 的
   `[workspace.dependencies] rar-rs`。
2. 刷新 `Cargo.lock` 与 `fuzz/Cargo.lock`（两个 CI 检查都带 `--locked`）。
3. 本地跑测试闸门：`cargo test --workspace --all-features`（CI 只跑 smoke）。
4. 打 tag `vX.Y.Z`（Release job 校验它等于 `crates/rar-napi` 的 Cargo.toml 与
   package.json）。
5. 按顺序 `cargo publish`：`rar-rs` → `rar-cli` / `rar-rs-napi`。
6. 刷新本文件与 `README` / `CONTEXT` / `ARCHITECTURE` / `CLI` / `testing` 的
   `最后核对` 锚点。
