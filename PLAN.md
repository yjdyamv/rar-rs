# rar-rs 计划

> 最后核对：2026-11-21 @ `f84e340`；实现细节以源码为准。

本文件只留**下一步**与**当前判断**。规则、契约与「别改回去」的地雷在
[`docs/PITFALLS.md`](docs/PITFALLS.md)；模块地图与设计不变量在
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)；术语在
[`CONTEXT.md`](CONTEXT.md)；命令行与退出码在
[`docs/CLI.md`](docs/CLI.md)。**过程与逐批验证记录在 git 历史** （按主题
`git log -- <path>`，按结论 `git log -S<string>`），本文件不维护 CHANGELOG。

## 现状

- **RAR5 / RAR7**：创建与读取全功能对齐 WinRAR 7.23——压缩（m0–m5、DP
  最优解析）、 `-hp` 头加密、分卷、solid、内联恢复记录、`.rev`
  恢复卷、quick-open（默认按官方的自动阀值写，`-qo` / `-qo-` / `-qo+` 三档）、
  NTFS ADS、三时间戳、owner、`-mt` 多线程、长距离匹配、v70 大字典。
- **老容器族读取（RAR 1.3–4.x）**：三代解码器（RAR29/20/15）+ PPMd + 五大标准 VM
  过滤器 + 通用 RARVM 解释器，solid 链、分卷、`-hp`、各代数据解密。
- **RAR4 创建全能力**：LZSS m1–m5 + PPMd + 六大标准 VM 过滤器 + `-hp` + 多卷 +
  solid （链内亦应用 VM 过滤器与 PPMd 模型延续）+ 并行 batch + 单大成员块级
  MT（字节同等）+ NEWSUB 恢复记录。能力表见
  [`docs/rar4-creation-spec.md`](docs/rar4-creation-spec.md)。
- **RAR 1.3 / 1.4 / 1.5 / 2.x 创建**：`-ma13` / `-ma14` / `-ma15` / `-ma2`，含
  solid、 `-p` / `-hp`、旧命名分卷、归档注释（`-ma15`/`-ma2` 把注释嵌进主头， 与
  1.5/2.x 读取器同构，2.90 可原样读回；这类归档的注释只能创建时写，
  编辑一律拒绝）。
- **RAR4 编辑全补**（ADR 0005）：头/块级操作 + 非 solid 块拷贝 + solid 整档
  repack； `-hp` 与分卷（`rn`/`ch`/`k`/注释/`d` 整集重打包）均已支持；
  PROTECT_HEAD（0x78）与 stored 成员混代的边角也已解除。
- **命令面**：官方 `rar` 全部命令（含 `rv` 补恢复卷、`lb/lt/vb/vt`
  列表变体）；官方 7.30 新增的 `la`/`va`/`lba`/`vba` 与服务块列表已接。
- **成员顺序与目录形状对齐官方**：solid 归档按官方的分组规则排序（扩展名 →
  基本名 → 大小 → 全名，目录条目排最后且深度降序），`update`/`freshen` 追加的
  成员同样排序（`-m0`/`-ds` 保持收集顺序），目录成员不带尾分隔符（RAR5
  `FILE_FLAG_DIRECTORY`、RAR4 `0x10`），`rarfiles.lst` 的优先级与子集规则照抄。
  契约见 [`docs/PITFALLS.md`](docs/PITFALLS.md)「solid 成员的顺序」。
- **solid 共享窗口按全批最大成员定档**（2026-11-21）：RAR5 solid 链的字典取
  **全批最大成员**的 2× 档（`add_batch` 记 `CompressionSettings::dict_basis`，
  `solid_dict_params` 据此建链首字典），与 WinRAR 一致（实测官方 2×600 KiB
  的两个成员挑 `-md=2m` 而非请求的 32m，故是「最大成员」而非「首个成员」也不是
  「总量」）；链内每个成员都声明链首字典（读者只按链首建一个窗口）。此前链首取
  写完的**首个**成员，8 个 System32 DLL 的 `-s -md32m` 因此被压成 8m。实测
  87 MB 语料 `-m3 -s -mt1` 38,681,741 → 37,761,273 B（−2.4%，且优于官方固实的
  37,222,347 与自己的非固实 37,518,912）。回归
  `rar50_roundtrip::solid_multivolume_roundtrips_with_exact_volumes`。
- **对外两组 API**：Rust 库是角色门面（`ArchiveReader` / `ArchiveWriter` /
  `ArchiveEditor`）；Node 绑定（`crates/rar-napi`）**手写入口 `rar-rs.js` +
  `rar-rs.d.ts`**，生成物只叫 `binding.*`，`tsc --noEmit` 在 CI
  把关。错误类别一路传到消费者（`ErrorCode` → CLI 退出码 /
  `RarError.rarCode`），提取可选逐成员容错（`ExtractErrorPolicy`）并带进度。
- **工程**：workspace 三 crate；CI 做 actionlint（workflow 文件本身）+
  shellcheck （`scripts/**/*.sh`）/ fmt / 路径分隔符守卫 / clippy `-D warnings`
  与 rar-rs 各 feature 组合 / wasm 目标 `cargo check` / 确定性测试 smoke /
  版本一致性 / cargo deny / rustdoc / 声明的 MSRV（`rust-version = 1.95`， 用
  1.95 真跑 `cargo check`），绑定侧的类型闸门 `tsc --noEmit` 在 build
  矩阵里跑一次；另有一条 `lint-windows`（clippy 全目标 + WinRAR 开关契约 +
  绑定单元测试），cargo deny 每周定时重跑一次以接住新的 RustSec 公告；七目标
  fuzz；取消钩子；QO 快路径；流式修复； 零填充分卷。**发布路径不再只在打 tag
  时才第一次执行**：资产收集与 release notes 已抽成
  `scripts/ci/collect-release-assets.sh` / `scripts/ci/release-notes.sh` 并由
  `scripts/ci/test-release-scripts.sh` 用 fixture 在每次 push 上验证（addon /
  WASI 清单、跨 target 生成的 loader 一致性、license 扁平化碰撞、notes
  分组与上一个 tag 的取法）；`build` 的 9 条矩阵移入
  `scripts/ci/build-matrix.js`，PR 只跑其中 1 条（带 `tsc` 类型闸门），每周
  定时跑满 9 条 + wasi bundle。job 图、每个 job 允许证明什么、成本与取舍见
  `docs/ci.md`。
- **格式对拍**：`crates/rar/tests/format_parity.rs` 把同一批载荷分别喂给我们的
  writer 与官方 `Rar.exe`，逐块逐字段比对（自带 RAR4/RAR5 头解析器，不依赖库内
  解析代码）：STORE 档要求整档逐字节相同（RAR5 / RAR4 / RAR2 三族），压缩档只放
  过 pack 长度与覆盖它的校验和；另有 locator、QO、solid 链、RAR2 无 ENDARC
  的专项 契约。分卷同样要求逐卷逐字节相同（RAR5 store、RAR4 新旧两套卷名、RAR2
  两套卷名）； 加密成员只放过密文与 KDF 盐。`.rev` 由 `rar-cli`
  侧对拍：同一份逐字节相同的 数据卷喂给两边的 `rv3`，产出的 `.rev`
  必须逐字节相同，且两边 `rc` 都能补回
  被删的中间卷（`rar5_recovery_volumes_match_winrar_byte_for_byte`）。
  恢复记录 + 分卷（`-rr` + `-v`）只能字段级对拍：官方每卷预留并把剩余空间补零，
  我方不补（`recovery_records_repeat_in_every_volume`）。
- **主头 locator 与 QO 默认形态已对齐**：locator 偏移字段宽度按**输入总大小**
  分档（CLI 把文件大小求和后交给 `WriterOptions::estimated_size`），默认 QO
  按官方自动阀值（成员数据块 > 4096 字节）写，没挣到记录的卷仍预留 locator
  （偏移 0），`-qo-` 才整条不写。契约
  `locator_width_follows_official_size_tiers` /
  `quick_open_automatic_matches_official`。
- **测试归档零提交**：legacy 套件（RAR 1.3–4.x、`.rev`、VM 过滤器、恢复记录）
  全部由 `.cache/winrar/` 的官方 `Rar.exe`
  现场生成（`tests/support/generate.rs` + `corpus.rs`），RAR 1.3/1.4 与
  v26/v36/v70 用我们自己的 writer 或改 `unp_ver` 字节；`tests/data/`
  只留两份原始载荷。代价：没有 tool cache 的机器上这些套件 打 `SKIP:`
  跳过（本地闸门覆盖，CI smoke 不覆盖）。

## 下一步

### 待办总览（未完成项）

- **CI 首跑/复核**：跨平台 build 矩阵、绑定 `typecheck`、release 上传与跨 target
  的二进制加载只能由 CI 证明；`workflow_dispatch` 带一个**已发布**的
  `release_tag` 复核 skip 分支（OIDC 真发已由 `v0.15.3` 证明）。
- **性能**：issue 09（DLL 单线程解析）、issue 15（价格驱动解析中间档 `-m1.5`?
  待决策）、issue 04（MT 窗口级不可压缩跳过）。
- **溯源**：`rars` 移植的逐文件与上游对拍（in-tree claim，不影响许可表达式）。

### 已知差异（已量化；`-qo`/`-rr` 分卷已对齐）

- [x] **`-qo`/`-rr` + 分卷的「卷尾补零 / 每卷预留量」**（**已实现并对拍**）：布局早已追平（每卷 QO 服务块 + 主头 `LOCATOR`、条目 `rel` 重算，有官方 `UnRAR` 对拍）；唯一差异是每卷尾部 trailer 的预留与补零。旧记录里的「末卷差 ~20–22 字节」「`-rr` 松弛量不规则」都是下面这套规则的副产物。
      **机制（`Rar7.exe` 7-23）**：`FUN_14003631c(stream,size)` = `seek`+`SetEndOfFile`，把每个非末卷预分配到计划大小，扩出来的零就是卷尾；多卷驱动 `FUN_14005be6c` 在 L428 对每卷调用它，大小取自 `[base+vol*0x40+0x30]`。每卷数据预算在 `FUN_14007de58(ctx,volume_size)`；`FUN_14007d704` 是前缀搜索；`FUN_140054100(type=3,pct,prefix)` = `plan_inline_recovery(prefix,pct).payload_size() + 0x16`（官方按**固定 22 字节**估 RR 服务头，实际头是 19/23/25，差额落在零尾）；`FUN_140054dfc` 是 RR 计划（data_shards 上限 200、`(2*pct*ds)/200`、group_count 取偶、header_size=`ds*8+0x48`、shard=`blocks*header+gc`，与我们 `plan_inline_recovery` 逐项一致）；`FUN_1400542cc` 发射 `{RB}` 块（magic `7b52427d`、64 KiB 分块）。
      **完整预算模型**（设 `V = volume_size`）：① `bound = V − 8`（8 = RAR5 END；`-hp` 时 32）。② 无 `-rr` 时 `term = bound`；有 `-rr` 时取满足 `u + plan_inline_recovery(u,pct).payload_size() + 0x16 ≤ bound` 的**最大前缀** `u`，再 `term = u − 8`（`FUN_14007de58` 里 `ctx[0x1f80] = lVar3 − lVar6` 的 `−8`）。③ `QO 预留 = 有 QO ? quick_open::entry_len_upper(V, header_len) + 8 : 0`（我们现在只到 `+0`）。④ 成员数据预算 `data = term − QO预留 − (8 签名 + 主头 + 文件头)`。⑤ **补零规则**：`content = 8 + 主头 + 文件头 + data + QO实际 + RR实际 + END`；非末卷**当且仅当 `V − content ≤ max(V/128, 1024)`** 时补零到 `V`，否则停在 `content`（末卷永不补）。
      **对拍**（`-m0` STORE，`-v{32k…512k} × -rr{1..100}%`，取每例的非末卷）：实现后**卷文件大小 / 成员 `data` / `content` 全部 1000/1000**。`-qo-` 无 RR 时本就逐字节相同。
      **实现（2026-11-22）**：`volume_term()` 复刻 `FUN_14007de58` 的 term；`volume_tail_reserve()` 用 `QO预留(+8) + (V − term)`；`pad_volume_tail()` 复刻「`V − content ≤ max(V/128,1024)` 才补零」；顺带把服务块 `data_size`/`unpacked_size` 改为走成员宽度分档（实测官方在 >7.5 KiB 时也宽 3 字节，QO 与 RR 都如此）。`format_parity` 的 `-rr` 分卷用例已从「字段级」升级为「padding 与 chunk 全等」，`cargo test --workspace --all-features` 全绿。复现脚本 `.scratch/{fastsweep,verify_ours}.mjs`；Ghidra 结论在 `Rar7.exe` 的 `rar5-volume` note。

- [x] **solid 里的滤波：换逐块选择器（2026-11-21）**：solid 成员改用非 solid 的
      逐 64 KiB 块选择器（`best_combined_specs`），拿回整员探针漏掉的 x86——87 MB
      DLL 语料 `-m3 -s -mt1` 37,761,273 → 37,167,723 B（比官方固实 37,222,347
      小）。切换时暴露并修掉了一个链 bug：`try_solid_filters` 在共享链状态上编码，
      候选**落败**时调用方的无滤波通路会从这个已被推进的状态继续编码，于是打包器
      发出读端没见过的距离（`match distance 262144 exceeds the 65536-byte window`，
      近随机成员首块 delta 假赢即可复现，单卷即触发）。现在落败候选**就地 reset
      链状态**（链首字典保留），改走 STORE，不再产出坏档。逐块选择器**整员全否**时
      回退整员探针 `solid_filter_specs`（覆盖率启发式漏掉某些 `E8 rel32` 串，整员
      探针仍能选中）。回归：
      `rar5_edge_cases::rar5_solid_filters::solid_archive_filters_are_applied_and_roundtrip`。

- [ ] **`-mt` 的 solid 体积差主要是 walk 解析档**（非 solid 同样存在）：`-m3
      -s -mt24` 41.4 MB vs `-mt1` 37.8 MB（官方 mt24 37.2 MB）。MT 各档走
      walk（`-m3` 位置链深度 8），顺序档走定价 DP，二者在 DLL 语料上差约 9%；
      `RAR_RS_MT_DP=1` 可把 MT 拉回 DP：`-m5 -s -mt24` 36.75 MB（官方 36.86 MB，
      代价 1.6–2.1x 时间）。这是 issue 09/15 的已知权衡，不是 solid 特有。

### 性能（进行中，未关闭议题）

- [x] **MT
      档位改成「全档走法」（2026-11-17，`exp/non-dp-official-shape`）**：当前基线见
      本文件上表，过程与被排除的方案见
      `docs/issues/compression-perf/handoff-2026-11-17.md`。内核镜像上
      `-m3`~`-m5` 与官方 同速（0.96 / 1.00 / 0.99x），`-m1`/`-m2`
      的比率好于官方，其余档差 +1.2…+3.2 pp。 同一分支上继续追比率的结果是：**非
      DP 路的杠杆已穷举**（候选结构、判决规则、
      前进方式、惰性阈值），要拿比率只能走定价 DP（同一批候选
      41.94%/41.29%，比官方低 0.2–0.8 pp，代价 1.4–1.8x）。注意下面 2026-11-14
      那条否决的是「把顺序解析器换成 哈希结构」，与本轮的 MT
      候选结构不是同一件事。

- [x] **匹配器/解析器整体重写——已尝试，**否决并回退**（2026-11-14，ADR
      0010）**： 把 BT4 + `RowIndex` 换成哈希环 / 位置索引链 + 融合单遍
      DP，三种形态全部 实测：环 48.18%/4848 ms、链 47.83%/13930 ms、混合（BT4
      候选源 + 融合 单遍 DP）46.43%/5101 ms，均未达到约定门槛（`ntoskrnl` m3
      ≤43.7% 且 ≤5030 ms），而旧内核是 46.29%/4679 ms。代码回到重写前，结论留在
      ADR 0010（Status: rejected）与 `PITFALLS.md`「压缩内核」两条。
      **别再走这条路**：哈希结构能给每位置便宜的候选，但给不了"每前缀长度的
      最近邻"，而后者正是树的价值；要提速只能降低树的每步单价。

- [x] **解析器单遍化 + 逐块缓冲区复用（2026-11-14）**：删掉 seq 的多遍重定价
      （m2/m3 各多跑一遍 DP、m4 两遍、m5 三遍），只付出 0.23–0.26pp 体积
      （`ntoskrnl` m3 41.64% → 41.90%），换来 m3 `-mt1` 6101 → 4711 ms
      （−23%）、m5 8544 → 4769 ms（−44%）。同时把 `BlockMatches` 与 DP 的 五张
      per-position 表（32 B/位置，64 KiB 块约 2 MB）搬进按 chunk 复用的
      `ParseBuffers`/`DpArrays`，再省约 2%；字节结果与复用前完全一致。 `-m3`
      现在 41.90% / 4711 ms，对官方 7.23（42.30% / 1466 ms）还差 3.2x 时间。

- [ ] **issue 09 — DLL 单线程解析速度**：真实 DLL 上 m3 `-mt1` 落后 WinRAR 约
      5.9x，瓶颈是 BT4
      下降步数（结构锁定：`HASH_BITS`、dict-log、提交阈值、近/远带宽四个旋钮已验证弹回）。
- [ ] **issue 15 — 价格驱动解析提速**（2026-09-18
      立项）：目标是**压缩速度**——给解析器补中间档，把每位置代价从更深的搜索换成更省的价格计算。杠杆
      3 的「DP 块顺延价格表」已落地（第 17 项，MT 比率白送 0.1–0.8pp）；seq 的
      `-m1`/`-m2` 断崖已于 2026-11-01
      定位并做完（结论：断崖在**收集器预算**上，不在定价上；中间档 = 小 cut +
      一次定价，梯度光滑，开关 `RAR_RS_SEQ_PRICE_TIER`
      默认关闭）。**待决策：是否提升为文档化档（`-m1.5`
      ?）**。四个杠杆、上游事实、验收判据与全部实测数字见
      [`docs/issues/compression-perf/15-fl2-zstd-parser-tiers.md`](docs/issues/compression-perf/15-fl2-zstd-parser-tiers.md)。
- [ ] **issue 04 — MT 随机数据窗口级不可压缩跳过**：成员级 STORE
      兜底已让随机数据领先 WinRAR 10–80x；窗口级跳过有把边界成员从压缩翻成 STORE
      的比率风险，需先有边界语料量化。

**压缩性能契约（动解析器之前先读）**

- **比率是契约**：任何提速必须让标准语料（text / mixed / xml / sparse +
  random）的 packed 字节不变或变小。
- **优先字节相同的快路径**，而不是启发式。
- **先测再优化**：`mtprobe` / `ratiocheck`
  示例是回归闸门，热点需先由探针确认；`ratiocheck` 现在同时打印 packed 流的 FNV
  指纹，用来证明「字节相同」而不只是「大小相同」。
- **发射块策略只有一个 owner**：`EMITTED_BLOCK_SIZE` + `find_block_end_adaptive`
  在 `codec/modern/lzss_huff/encoder/parse/block.rs`。
- **`-mt` 低步数搜索是已接受的取舍**（ratio 换速度），不是待修的 bug。

**已否决方向（实测为负或结构不可行，别重试）**

| #  | 方向                                                  | 判决                                                                                                                                                                                                                        |
| -- | ----------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 07 | 字面量密集块跳过重定价                                | 否决：闸门只在解析本来就很便宜处触发，却改变了 DLL 字节                                                                                                                                                                     |
| 10 | 2–3 字节短匹配                                        | 否决：短匹配槽码长依赖频率 bootstrap，两遍定价无法安全复现，各变体都掉比率                                                                                                                                                  |
| 14 | 两制近存（近带 L3 驻留 + 远树重插）                   | 实测为负（seq −75%、mt8 −98%），废弃                                                                                                                                                                                        |
| 06 | 成员级 solid MT                                       | 结构不可行（无法跨成员并行共享窗口）；chunk 级 MT 已落地                                                                                                                                                                    |
| 09 | BT4 字节级流水                                        | 已到顶（首步 value-carry −3.6% 即全量）；再降步数只剩显式取舍项                                                                                                                                                             |
| 13 | 远带候选预算（原 `RAR_RS_FAR_BAND`）                  | 仅 seq opt-in 有效（−9% @ +0.22pp），非 mt8 解药；已删除，不留开关                                                                                                                                                          |
| 15 | 行索引替 BT4 / 砍步数预算 / 砍近窗（都是给 seq 提速） | 否决：`RowIndex` 单测全位置 collect 在 depth ≥ 16 时比树慢（437–904 vs 166–362 ns/位置）；`MAX_PARSE_STEPS_PER_POSITION` 12→4 在 `ntoskrnl` 上时间与比率都不动；近窗 w4/w2/w1 掉 0.6–1.6pp 且 `mixed` 更慢。数字见 issue 15 |

已落地（别再重新论证）：01 matchless-block DP 快路径（字节相同）、02 collector
fast-mode 门（`longest==0`，阈值 256）、03 delta 候选通道 + 采样预门、05
流式路径 auto delta/x86、08 持久树跨 chunk 增长的损坏修复、11 BT4 首步
value-carry（−3.6%）、 12 MT 近窗对齐（已被 13 取代）、 16 哈希链走查预取
（`match_finder.rs`：提前载入下一候选并 `prefetch` 比较字节，字节相同， RAR4
`-m5` −26%、RAR5 `-m5` −18%，solid 持平——候选已并行、带宽吃满）、 17 MT 的 DP
块顺延价格表（`windowed_priced_parse` 把上一块的码长表交给下一块首次
定价，替代「多跑一遍重定价」，text l3 −0.51pp / 30 MB 文本
−0.6~0.8pp，耗时持平； MT 本来就是已接受的字节分歧，packed 只变小）、18 RAR5
解码热路径 （`BitReader` 移位寄存器、`DecodeTable::quick` 单装载、`copy_match`
分块 memmove、 `append_output_into` 免中转；`ntoskrnl` m5 提取 47 → 60
MB/s，字节不变）、 19 符号流的两笔结构账（`SymbolReader::next` 改返回两字宽的
`Option<Symbol>`，错误走 `take_error()`；热路径六个函数强制
`#[inline(always)]`、`decode_non_literal` 强制外联； `consumed_bits()`
替掉每符号的除余。同机交替 A/B：`decode_bench` 总体 +9%， `ntoskrnl` 无过滤 m5
−17%、text 13 MiB −15%，字节不变）；另加一项 20「`next_symbol`
不再外联」（`read_block_header` 改
`#[inline(never)]`，每符号一次的函数调用消失， `run_engine` 里的
`callq next_symbol` 归零；`decode_bench` 三语料中位数 +6~7%， 真实 m5 归档 −1~2%
在噪声内——asm 判据与实测口径见 `docs/PITFALLS.md` 第 ⑧ 条）、 21 `copy_match`
的自我重叠走**倍增**（`dist < len` 时先搬一个周期再翻倍搬运， memmove 次数从
`len/dist` 降到 `log2(len/dist)`；真实归档无变化——重叠仅 0.26%——
但长游程极端输入：13 MB 全零 0.106→0.047 s（−56%）、13 MB `abcd` 重复 −25%）、
22 **创建侧 DP 的两笔账**（`same_price_run_end` 直接用 `plan`
已算好的缓存/加成， 不再每步回扫
`state.reps`；重定价收敛即早退。字节相同，同机交替 A/B：`ntoskrnl` m5/m3
`-mt1`、30 MB 文本 m5、`-mt16` 全部 −4.5~−8.9%，20 组归档 sha256 全等；
收敛早退只在二进制上触发。已否决的后续尝试与实测口径见 `docs/PITFALLS.md` 的
「DP 解析的两笔账」条）、 23 **槽步进定价的融合**（一个 run 的距离侧（缓存槽码长
/ 新鲜距离的槽 + 额外位、 长度加成、哪个长度走 repeat）对 run
里每个长度都相同，`SlotPricer::for_run` 一次 分类好；`step` 每步只剩一次
`ilog2`、一次移位和两次查表，替掉原来每步的 `MatchShape` 解析 +
`encode_length_slot` + `distance_bits`。唯一例外是 repeat 截断的
边界长度（可能落在更靠后的槽），每 run 预解一次。字节逐位相同：4 语料 × 3 档
packed 流 FNV 全等（`examples/ratiocheck.rs` 的 hash 列），`slot_pricer_tests`
把融合版与保留为 `#[cfg(test)]` 的参考实现逐长度对拍。同机交替 A/B：DP 阶段
−9.4%（wordtext m3）/ −9.6%（m5），端到端 wordtext m3 −8%、xml −6.4%、dll-like
−6.3%、x86syn −3.0%、 text −1.4%、mixed/random 持平。边界与「别改回去」见
`docs/PITFALLS.md` 「槽步进定价的融合版」条）。

24 **并行校验/解压**（`t` 走 `verify_indices_parallel`：RAR5 与 RAR4（RAR
1.3/1.4 除外）的非固实成员、≥4 成员、≥8 MiB 解压量、无进度回调、packed+unpacked
≤256 MiB 时，RAR5 先在串行 `Engine` 游标上读出并解密全部负载，再交给 Rayon
并行解码+校验；RAR4 由工作线程各自打开卷句柄读取，因此分卷/跨卷成员同样并行。
解码结果算完 CRC/Blake 立即丢弃，逐成员错误仍按目录顺序汇总。8 个 System32
DLL、87 MB、`-m3`：RAR5 `t -idq` 327 ms → 205 ms（官方 317 ms）；RAR4 单卷 ~400
ms → 300 ms（官方 402 ms），RAR4 15 卷 332 ms（官方 406 ms）。整档 `x`
是**解码与落盘流水线**：工作线程解码，主线程按目录顺序做 `-o-`/`-kb`/覆盖裁决
并写盘（有界 sync_channel + 小重排窗，资格条件同上），RAR5 384 → 306 ms （官方
343 ms），RAR4 单卷 337–375 ms（官方 430 ms），RAR4 多卷 382 ms。 `parallel`
特性下生效，`cargo test --features parallel` 覆盖（含 RAR4 分卷
verify/extract）；门槛与顺序语义见 `docs/PITFALLS.md`「并行校验的资格门槛」条。
内存门槛按"RAR5 预读 packed + 同时在解码的成员解压缓冲 ≤512 MiB"节流并发
（`live_workers_for`），既不按 packed+unpacked 总量一刀切，也不整档回退串行：
4×90 MB 的档（360 MB 解压、100 MB packed）`t` 从串行 ~1.4 s 降到 553–611 ms，
官方 UnRAR 1080 ms；`x` 1196 ms（官方 1208 ms，写盘受限）。8×90 MB / 200 MB
packed 的档预算只够 3 路并发，`t` 1358 ms（串行口径）→ 1245 ms，官方 1964 ms。

25 **固实档滤波**（`-ma5 -s`）：成员在共享 LZ 窗口里照常整员试 Delta→E8E9，
命中后把滤波记录写到该成员数据前部，链状态继续（解码端按
`block_start - member_start` 还原成员内坐标，分块/跨卷也成立）。探测在改动共享
`EncoderState` 之前完成——落选方案无法从链里回滚；`-mc-`/`-mcd-`/`-mce-` 与
`-mcd<N>+` 均生效（`solid_filter_specs` 读 `FilterOptions`）。87 MB 的 8 个
System32 DLL、`-m3`：`-s` 26,918,320 → 24,631,445 B（−8.5%），同语料非固实
24,261,788 B，官方固实 22,934,902 B。回归
`rar5_solid_filters::solid_archive_filters_are_applied_and_roundtrip`。注意
`add_bytes`（内存 API）仍不跑滤波，只有文件路径会试；官方 RAR 7 的固实压缩
本身多线程（mt8 1.73 s / mt1 7.74 s，体积仅差 0.006%），我们的链仍逐成员串行，
分段并行是后续项。

26 **跳过无滑动的 desc 复核**：collect 对每个 tree report 的 `match_length_at`
复核只为「帧滑动（`rebase`）后链路可能指向改过的字节」而存在；member ≤ 字典时
从不滑动，report 已精确。加 `TreeMatchFinder::frame_slid`，在 `links_are_exact()`
时只推 desc 的长度、不再复测（滑动时照旧复核）。字节不变；`collectbench`
ntoskrnl 13.3 MiB m3 seq **4604 → 3502 ms（−24%）**，ratio 44.59% 不变，602 个
lib 测试 + `decode OK` 全过。详见
[`docs/issues/compression-perf/09-dll-parse-speed.md`](docs/issues/compression-perf/09-dll-parse-speed.md)。

**解压基线**（`cargo run --release --example decode_bench`，自建语料，packed
比例 29%/41%/35%）：text 8 MiB 80 MB/s、mixed 4 MiB 91 MB/s、code-ish 4 MiB 74
MB/s， 总体约 81 MB/s。同一批归档交给官方 `UnRAR.exe` 并扣掉其 0.10 s 进程启动：
150 / 340 / 165 MB/s——即**纯解码**我们落后 1.9–3.7×。端到端的 `x`/`t`
对比没有意义 （官方那 0.10 s 常数比 8 MiB
的解码时间还长），看进度只能看这个进程内基线。当前本机（第 1–24 项落地后）：
text 315 / mixed 336 / code-ish 266 MB/s，总体约 300
MB/s（同上三语料、release）。

**当前基线**（2026-11-17，`exp/non-dp-official-shape`；`ntoskrnl.exe` 13,917,568
B、`-md32m -mt24`，我们取三次最小、官方 7.23 取两次最小）

档位之间靠**候选结构 + 候选深度**区分：`-m1`~`-m3` 用**官方形状的位置索引**
（一趟顺序填充 `prev[pos] = pos - 上次同哈希位置`，没有计数排序；深度 2 / 4 /
8）， `-m4`/`-m5` 用排序好的行索引（桶内是连续内存，指针追踪换成顺序读，深度 128
/ 384， 并加逐块滤镜扫描）。定价 DP 退到 `RAR_RS_MT_DP=1` 后面。
**可达距离一律按 `-md`
整窗**——早期把它当档位旋钮（128K/256K/512K）在有长距冗余的 数据上是灾难：9.2 MB
markdown 上 32 KiB 前瞻产出 3,246,179 B，整窗只要 244,295 B。

| `-m` | 我们（内核镜像） | 官方 7.23       | dRatio   | 时间  |
| ---- | ---------------- | --------------- | -------- | ----- |
| 1    | 196 ms / 46.94%  | 129 ms / 54.34% | −7.40 pp | 1.52x |
| 2    | 224 ms / 46.10%  | 197 ms / 46.59% | −0.49 pp | 1.14x |
| 3    | 307 ms / 44.85%  | 245 ms / 42.30% | +2.55 pp | 1.25x |
| 4    | 369 ms / 43.56%  | 359 ms / 42.14% | +1.42 pp | 1.03x |
| 5    | 393 ms / 43.32%  | 376 ms / 42.10% | +1.22 pp | 1.05x |

`-m3` 起保留逐块滤镜扫描：`-m1`/`-m2` 的整成员 E8E9
记录在代码类数据上更快更好，但它 **无法选 delta**，而 delta 友好的数据（一段 64
KiB ramp）在扫描下是 22,594 B、整成员 E8E9 下是 42,169
B。这个差距在内核/源码/markdown 三份语料上都看不到，是
`cli_mc_filter_policy_controls_filters` 抓到的；恢复扫描把 `-m3` 的比率从 +3.17
pp 收到 +2.55 pp，代价是那一档从 1.01x 变成 1.25x。

**`-m1`/`-m2` 的比率好于官方，`-m4`/`-m5` 与官方同档（1.03–1.05x），`-m3` 慢
25%**（付给 滤镜扫描）。markdown（9,251,209 B）与 Rust 源码（5,491,472
B）上各档与官方的差距在 ±0.8 pp 内，时间 0.9–1.3x。

**比率侧：一步惰性阈值是最便宜的一档（`WALK_LAZY_MAX`）。**
实测（内核镜像，内部时间/大小） `-m3` 与 `-m5`：8 →
157/6,361,369、236/6,070,109；32 → 162/6,346,001、250/6,046,079； 64 →
169/6,345,598、259/6,045,537；128 → 167/6,345,539、249/6,045,396。取 64：把
`-m1`~`-m5` 的比率各压低 0.1–0.25 pp，代价是 `-m3`/`-m5` 各 +12/+23
ms（仍与官方同档）。
判决规则本身（官方接受的"更近的短匹配"闸门、比特定价、收紧距离闸门）三样实测都是负的，
证据与逆向读到的判决循环见 handoff。 `-m1` 只在**内核镜像**上慢（1.52x，另两份
0.97 / 1.05x），而且差距不在索引结构：官方 `FUN_140043670` 把链按 `window * 4`
分配、`FUN_1400468d4` 按"位置模窗口"走链，与我们 一样是每位置 4 字节，另外还多了
prev1/2/4 距离表。剩下的是**每位置的解析开销**
（短匹配源、惰性探针、距离缓存排序各占一段，实测 waves 里没有一个超过 20%）。见
[`docs/issues/compression-perf/handoff-2026-11-17.md`](docs/issues/compression-perf/handoff-2026-11-17.md)。

**位置索引的构建与预表**：链没法分片建——每片头表从空开始，walk
走到"该片内首次出现" 就断，而断的正是长距匹配（实测 markdown：精确 244,419 B，2
片 393,459，8 片 1,231,478）。
但它是**可扩展**的：头表就是全部状态，按波序一段段追加即可。`-m1`~`-m3`
现在就是这样： 第 w 波读 `prev[..end_w]`，同一个 scope 里另一个任务填
`prev[end_w..end_{w+1}]`，两段
不重叠，输出逐字节不变，于是这部分不在关键路径上。

预表里最大的一块是 **`LongRange::push`**（长程采样表）。而独立成员根本不查那张表
（`lr_q` 为
None），所以它现在**推迟到"确实还有下一个成员"时才做**：状态里留住上一成员
的字节（超过历史上限的部分只记数量），下一个成员开始时再索引。**单成员归档因此不再付
这笔钱**，多成员只多留一个成员。输出**逐字节不变**——单成员和两成员 solid
都逐字节核对过。

深桶的查询是另一回事：链是指针追踪，深度 16 起就比排序桶的连续读贵（内核镜像 164
vs 110 ms 的 waves）。因此 `-m1`~`-m3` 用浅深度，需要深度的 `-m4`/`-m5`
用行索引。

`RAR_RS_MT_DP=1` 把 DP 放回 `-m4`/`-m5` 可到 41.49%/41.29%（比官方小 0.65/0.81
pp）， 代价 1.6–2.1x。

DP 阶段的成本结构（第 22 项调查时用临时 stage 计时器 +
计数器量出，未留在代码里）： collect 38%、DP 55%、convert 2%、emit 1%；DP
内部每位置约 170 ns（4 遍重定价下 每位置 4.4 个长度槽步、1.44
个候选），其中**两笔重复距离探测占 27%**（每位置两次
远地址读，几乎是纯缓存缺失），把这两次探测拿掉会掉 0.3pp 比率，所以只能减少不能
删除。想把它们并到缓存或预取里都实测为负（见 `docs/PITFALLS.md`「DP
解析的两笔账」）。

二进制语料上（`ntoskrnl` m2/m3、BT4 cut 16 + 两 pass）比例反过来：**collect 占
55–60%**、dp + passes 约 36%，且 collect 吞吐随尾部窗口增长而恶化（每 chunk 8.06
→ 5.69 → 4.32 MiB/s @ cut4，166 → 362 ns/位置 @ cut64）——这就是 issue 15
把中间档的旋钮定在**收集预算**而不足定价上的原因。

### 功能缺口（按需，不阻塞发布）

- [x] **RAR4 solid 归档 MT**：成员级并行不需要跨成员共享窗口——真正共享的是
      *进入成员之前*的链状态，而每个候选（plain LZ / 逐个 `-mc` 过滤 / PPMd
      试压）对它都只读，所以候选之间可以并行、胜负仍按串行顺序裁决，字节不变
      （2026-10-27 已实现，`search_solid_member_parallel`，契约
      `solid_search_parallel_matches_serial`）。30 MB 文本 solid m5：23.2 s →
      14–17 s（16 核）；剩下的差距在 match finder 本身（官方 1.0
      s），不是并行度。
- [x] **无恢复记录时的重建改为原样搬块**（2026-11-17）：`rar r` 在既无 `.rev`
      也无内联记录时，先前是「逐成员解压、再以 STORE 重新加入」，528,797 B
      的归档 重建出 665,288
      B，成员的时间戳和属性也一并丢掉。现在先**解码验证**每个成员
      （模块原有承诺，不信任仅头部 CRC
      正确的块），通过后**原样拷贝该成员的头+数据 块**：RAR5
      文件块是连续跨度且不存绝对偏移，所以搬块无需任何修正。实测同一 归档重建为
      **528,663 B**（官方 528,789），成员时间戳逐字节保留。带坏头的
      归档、以及任何无法用「重新打开并逐成员解码」验证的产物，一律退回到解码+STORE
      路径（宁可变慢也不能搬错）；legacy 源仍走 STORE（`unp_ver` 20
      的既有理由）。 测试
      `archive::reconstruct::tests::rebuild_keeps_the_members_compression`。
- [ ] **逐文件与上游对拍**：`rars` 移植的出处目前是 in-tree claim（每个文件头 +
      inventory 表），尚未与上游 diff。不影响已定的许可表达式。

### 有意不做（设计决定，别当缺口修）

- **老编码器块级 MT（v15/v20 单个大成员）**：v20 已有「多窗口并行解析 +
  顺序写位流」 的骨架，但需要每窗口独立的 match finder
  状态与确定性窗口边界才能保证字节一致，收益仅「老格式大成员的创建速度」（官方
  7.23 已移除 `-ma4`）；v15 另受自适应表限制。
- **PPMd
  块级MT**：单自适应模型，切块会改变输出（结构上不可行）；成员级并行已有。
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

## 已完成

已发布 `0.15.1` / `0.15.2` / `0.15.3`（GitHub Releases、crates.io、npm
`@yjdyamv/rar-rs`）。发布流水线、Trusted Publishing、npm 单包决策与 CI 形态见
[`docs/ci.md`](docs/ci.md)；测试语料、`SA_REQUIRE_WINRAR` 与版本上限见
[`docs/testing.md`](docs/testing.md)；`rar-cli` 的空 lib 目标为何存在见
[`crates/rar-cli/src/lib.rs`](crates/rar-cli/src/lib.rs)。

## 开放议题

`docs/issues/<feature>/` 只留**未关闭**议题；关闭时判决并入本文件或
[`docs/PITFALLS.md`](docs/PITFALLS.md)，并删掉文件。

- [`compression-perf/`](docs/issues/compression-perf/) —
  04（窗口级不可压缩跳过）、 09（DLL 解析速度）、15（价格驱动解析），均对应上面
  「性能」段的清单项。

## 一致拒绝（别"修"）

- **分卷 append**：官方 `rar` 拒绝（`Cannot modify volume`），我们**故意不跟**——
  RAR4 与 RAR5 的 `a` / `u` / `f` 都与分卷删除共用整集重打包（原卷大小、编号、
  固实、注释、逐卷 RR、`.rev` 比例全部保留，暂存整集后换卷），因此卷集可以增长
  而不变形；追加的成员另起一条非固实链。显式改卷大小（对已有档 `-v`）仍拒绝。
  分卷的 `rn` / `ch`、`k`、归档注释 **不是**拒绝项：官方支持，我们也支持。
- **分卷删除**：官方 `rar` 拒绝（`Cannot modify volume`），我们**故意不跟**——
  `d` 走整集重打包（原卷大小/编号/固实/注释/逐卷 RR 保留，暂存后整体换卷），
  因为卷不能缩而不移动后续成员与 parity；这是可验证的正确语义，测试
  `multivolume_delete_*`、`official_unrar_validates_rar4_multivolume_delete`。
- **分卷 + 内联恢复记录（`-rr`）**：**创建时已实现**——`-v` 配 `-rr`
  时每卷各带一份记录，`.rev` 可同时用（RAR5 与 legacy RAR4
  同形）；编辑一个**已带记录**的卷集时按原强度重建记录，不是拒绝项。
- **在 RAR4 卷集上显式改变恢复强度**（`rar rr <set> 20`、`-rr10`
  配已有卷集）：**已实现**——与分卷删除/append 共用整集重打包，按新强度逐卷
  重建记录（`-rr<N>` 每卷 N 个 parity 扇区，`-rr<N>%` 按每卷比例），并同步重建
  `.rev`；官方对自己的卷集一律 `Cannot modify volume`，所以这是超出官方的行为，
  测试 `multivolume_rr_*`。
- **PROTECT_HEAD（RAR 2.5 时代）记录**：已支持就地编辑/追加/重打包，按原强度
  重建为 NEWSUB `Protect+`（见 [`docs/PITFALLS.md`](docs/PITFALLS.md)）。
- **`-rv<N>` 的数量**：与官方一致——按请求如数生成（4 个数据卷配 `-rv9` 出 9 个
  `.rev`，`-rv12` 出 12 个），总量也逐字节一致（`-rv3`：12,583,137 B、
  `-rv9`：37,749,411 B，13.9 MB 载荷 `-m0 -v4m`）；生成耗时 115 ms vs 官方 97
  ms。 历史上我们把它截断到数据卷数，`create.rs` 里的 `.min(nd)`
  已按官方行为去掉， 回归测试 `recovery_volume_count_follows_the_request`。
- **恢复能力不足时的退出码**：数据卷缺得比 `.rev` 多时双方都恢复不了，但我们打印
  `N volume(s) missing but only M recovery volume(s) available` 并退 2，官方静默
  退 0——看起来像成功，实际什么都没重建。这一处**故意不跟**官方。

## 已知小差异（记录，互操作无碍）

### 仍存在的差异

- **同一次 `a` 里的重复成员**：同一个文件既当显式参数、又落在目录参数里时，
  官方按 `-o+` 的覆盖语义把它挪到末尾，我们保留原位（其余排序规则已对齐， 见
  [`docs/PITFALLS.md`](docs/PITFALLS.md)「solid 成员的顺序」）。
- **`-ts` 的「字母+数字」组合**、**`-rr<N>%` 取整**、**`rar rr` 恒 3%**、
  **`rar r` 不写 `fixed.<name>`**、**无控制台时的覆盖询问**：口径与理由见
  [`docs/PITFALLS.md`](docs/PITFALLS.md)「有意不追平官方」。
- **RAR5 主头 locator 偏移宽度（CLI 已全路径一致）**：不传
  `WriterOptions::estimated_size` 时主头 locator 偏移字段取**默认 5 字节**
  （`DEFAULT_OFFSET_WIDTH`，不是「最小
  3」），官方按**输入总大小**分档（3/4/5/6）。 CLI 的 `a` **和 `m`**
  都把输入文件大小求和后传进去（压缩只会变小，输入总量是 安全上界；`m` 同时按
  `a` 默认开 quick-open），所以**所有 CLI 建档路径**都逐字节 一致（实测 ours
  `a`/`m` 与 6.23/7.23 的 `a`/`m` sha256 相同）。只有**库调用者 不传**
  `estimated_size` 时才看得到差异——且库**无法自行推导**：主头在成员之前
  写入，写头时还不知道总量。
- **Windows 联接点的 redirect 目标字符串**：见
  [`docs/PITFALLS.md`](docs/PITFALLS.md)「有意不追平官方」。
- **`p`/`-so` 的残余差异**：成员选择器我们更宽松——`selector.rs` 统一把 `\` 与
  `/` 归一后再匹配，所以 `p sub/c.txt`（正斜杠）能命中官方需反斜杠的归档名（官方
  6.23/7.23 都要求**分隔符精确**，`p sub/c.txt` 退 10）；这是刻意的跨平台便利
  （Unix 档存 `/`）。`-so` 是我们的**扩展**（等价 `p`）：7.23 `Rar.exe` **与**
  `UnRAR.exe` 都报 `Unknown option: so`（退 7），6.23 忽略该未知开关径自解到磁盘
  ——保留作便利，不追。（头加密归档缺口令的退出码**已对齐**：现在退 12，与 7.23
  一致；密码错仍退 11。）
- **SFX 内嵌档案的字节（官方 `s` 会「重序列化」内嵌档）**：**已对齐**。
  `crates/rar/src/format/rar5/sfx.rs` 复刻官方 `s` 的重序列化：①每个 FILE_HEAD 的
  `DataSize`/`UnpackedSize` 改到**最短 vint 宽度**（值 13 从保留宽度 `8D 00` 收到
  `0D`），重算该头 `HeaderSize`+CRC；②**按自动规则重建 QO 目录**——只为 **packed
  数据 run 超过 4096 字节**的成员建条目、任一达标才写记录（官方 `s` 连 `-qo-` 源
  也按此自动规则补写，实测 `-qo-` 内嵌后与 `-qo+` 同形）；③**丢弃**内联恢复记录
  （SFX 不带 RR）；④丢弃源 QO、按新位置重算条目 `rel`、按档大小档位重算主头
  LOCATOR 宽度并回填 QO 偏移。逐字节贴平已在
  basic/multi/many/`-rr5%`/`-rr20%`/`-qo-`/`-qo+`/`-tsa -tsc` 上对 7.23 实测全中
  （RAR5、`-m0` 基准；压缩档因编码器 packed 字节不同仍不可比）。头加密/多卷源不
  重序列化（stub 原样前置，仍是合法 SFX）。回归：`cli_sfx_reserializes_embedded_archive`。
- **`a` 的 QO 条目选择（多成员档）**：**已对齐**（单卷）。我们的 `a` 此前在记录被
  触发时**缓存每个成员的头**；官方只缓存 **packed 数据 run > 4096 字节**的成员
  （`multi` 档：官方 QO 负载 47 字节=1 条，我们曾 145 字节=3 条）。现在
  `record_quick_open_entry` 与卷尾 QO 预留都按成员**总 packed run** 过阈值过滤
  （`-qo+` 仍全缓存、`-qo-` 不缓存），单卷多成员档（含 `mixed -v16k`）逐字节一致。
  **仍存**：多卷且每卷含多个缓存条目时，卷尾 QO 预留与官方差约几十字节（`-qo+` 下亦
  复现，与条目过滤无关，属既有多卷预算模型缺口）。回归：
  `cli_quick_open_skips_small_members`。
- **`-v` 恰好单卷时（官方降级为普通档案）**：**已对齐**。`a -v<size>` 若内容恰好装进一卷，官方写的就是不带 `-v` 的那份普通档案：名为 `{base}.rar`（不是 `{base}.part1.rar`）、主头不带 `ARCHIVE_FLAG_VOLUME`。`commit_pending` 现在在 `nd == 1` 时把主头的 VOLUME 位清掉（重算头 CRC；清低位不改变 vint 宽度，故头长与后续偏移不变）、最终名定为 `{base}.rar`；`-hp`（明文头无法解析）与带 `.rev` 的集合不降级。`-m0`/`-qo-`/`-qo+` 单卷实测与官方逐字节一致。**仍存**（均与降级无关的既有差异）：单成员 `-s` 档官方不置 SOLID 而我们置了；`-rr` 单卷的内联恢复记录载荷两边仍不同。回归：`cli_single_volume_dash_v_is_a_plain_archive`。
- **`-os` NTFS 流（STM）（packed 字节仍不同）**：两边 STM 服务块名字 /`unpSize`/
  `dataCrc` 相同；`compInfo` 现在**都是 `384`**（version 0、solid 0、
  **method=3**）——`write_stream_record` 先用 method 3（dict log 0）压缩流数据，
  `-p`/`-hp` 时再逐流加密（加密在压缩之后，CRC 仍为明文 CRC；不受 `-m0` 影响）。
  **packed 字节仍不同**（自研编码器输出长度与 WinRAR 差若干字节，如 3800 字节流
  我们 37、官方 36），故归档/`s` 仍非逐字节一致；但**语义一致**、`compInfo`/明文
  CRC/解出的 ADS 逐字节相同、**双向互操作**（官方 6.23/7.23 `UnRAR` 能还原我们的
  压缩流、`Rar t` 通过；我们也能解官方的压缩流）。回归：
  `os_stream_data_is_compressed`、`os_streams_interop_with_winrar`。

### 已对齐（留存结论，防回归）

- **`lb` 与分卷成员**：`lb` 对跨卷成员在**首卷**打印一次、在**每个后续卷**也打印
  该成员名（该卷只有这一个名字）。实测 WinRAR **4.20/6.23/7.23** 的 `Rar.exe` 与
  `UnRAR.exe` 全都如此，与我们一致（旧记录「`Rar.exe lb`
  不打印跨卷成员」对有符号 版本不成立，已更正；回归测试
  `cli_bare_list_prints_split_member_on_every_volume`）。 唯一残留：我们 `lb`
  也接受套系的**基名**（`set.rar`，经 `discover_volumes`
  解析），官方对新编号套系要求 `set.part1.rar`（基名退 10）——属放宽便利。
- **`u`/`f` 的成员顺序（含 solid）**：`rar u`/`f` 对**单卷 RAR5** 档**就地替换**
  被刷新的成员（保持原位、内容/CRC/mtime 更新），与官方 6.23/7.23 一致——**含
  solid 档**：被替换成员所在的 solid 链会被**就地重压**（先解码旧内容推进共享
  **解码**窗口，再用新内容推进共享**编码**窗口，后继成员仍正确解码）。回归测试：
  `cli_update_keeps_refreshed_member_in_place`、
  `cli_update_solid_refresh_keeps_member_in_place`、库
  `editor_replaces_a_member_in_place`、`editor_replaces_a_solid_member_in_place`。
  实现：`EditPlan::replace` / `RewriteOp::Replace` +
  `SolidChainState::replace_solid_member`。新增成员仍**追加到既有成员之后**（目录
  条目最后）。**仍回退「删旧+追加」**（被刷新成员落到档尾）：多卷、
  `-ol`/`-oh`/`-oi` 链接重定向、`-ver` 版本控制、目录成员、重名成员——`t` 通过、
  数据无损。
- **`-oh` 硬链接 redirect 的声明大小**：把硬链接存为 redirect 时，官方把该成员的
  平铺大小写为**目标文件大小**（`lt` 显示 `Size: 19`、`Packed size: 0`）；我们
  现在也写目标大小（`LinkRedirect.size`），`lt` 与官方逐字段一致。解压出的硬链接
  在两边都正确共享 inode（`fsutil hardlink list` 均两条）。`-ol`
  的符号链接（固定 mtime + 相对目标）元数据与官方逐字段一致。
- **SFX 产物扩展名**：`rar s` 现在写 `<base>.exe`——与官方
  **3.93/4.20/6.23/7.23** 一致（也与本仓库 create-time `-sfx` 一致）；`s-` 同时
  识别 `.exe`/`.EXE`/`.sfx`/`.SFX`。同时对齐了官方两处安全行为：`s` 在无 `-y` 时
  不覆盖已存在的 `<base>.exe`（官方非交互下保留原文件，我们此前会静默覆盖真实
  程序）；`s-` 先校验签名之后确有**合法主头**才写，非 SFX 的可执行文件（哪怕偶然
  含 RAR 魔数）不再产出垃圾 `.rar`（官方报 `Bad archive`）。`sfx_offset_of` 亦
  识别 7 字节 RAR4 签名，`s-` 因此能剥离 RAR4 SFX（`s` 本就接受
  RAR4）。回归测试：
  `cli_sfx_roundtrip_with_module`、`cli_sfx_strip_rejects_non_archive`、
  `cli_sfx_refuses_to_clobber_existing_exe`、`cli_sfx_roundtrip_rar4`。
- **文件头编码与 WinRAR 版本无关**：固定 mtime 下，ours / WinRAR **6.23 / 7.23**
  的 RAR5 `-m0` 档案 **sha256 完全相同**（保留的 2 字节 vint、Windows FILETIME
  的 FILE_TIME 记录、`compInfo=0` 三方一致）；RAR4 文件头 **4.20 与 6.23
  也逐字节 相同**。`s` 内嵌档如今也逐字节一致（见上）；`-os` 流现在两边都用
  method 3，差别只剩编码器输出字节，与 版本无关。（缓存里没有 5.x；4.20 不认
  `-ma4`、7.23 已移除 `-ma4`，故 RAR4 只能 拿 4.20 默认档与 6.23 比。）
- **RAR4 奇数秒 mtime 的 `FHD_EXTTIME`（按容器版本决定）**：extended-time 区是
  RAR 3.0+ **容器**特性，判据应是容器 `unp_ver`（`rar4_unp_ver`），而不是成员
  codec 版本。`-ma4`（容器 29，即使 `-m0` store 成员的 codec 版本是 20）现在对
  奇数秒 mtime 写 2 字节 `PRESENT|ADD_SECOND`——与官方 4.20/6.23 **逐字节相同**、
  解压还原奇数秒；`-ma2`/`-ma15`（pre-3.0 容器 20/15）不写，保持 2 秒精度，使
  RAR/UnRAR **2.90** 仍能读（2.90 见到 v20+EXTTIME 会报「the file header is
  corrupt」，实测）。因此只有含奇数秒成员的 `-ma4` 档会让 2.90 报 corrupt——与
  官方 `-ma4` 行为一致；偶数秒 `-ma4` 与 `-ma2`/`-ma15` 不受影响，无需开关。

## 发布清单（改版本号时逐项过）

1. 五处版本一致：`crates/rar`、`crates/rar-cli`、`crates/rar-napi` 的
   `[package] version`，`crates/rar-napi/package.json`，根 `Cargo.toml` 的
   `[workspace.dependencies] rar-rs`。
2. 刷新 `Cargo.lock` 与 `fuzz/Cargo.lock`（两个 CI 检查都带 `--locked`）。
   只改版本号时不要整个 `cargo update` fuzz 工作区——那会顺带升级一堆依赖， 把
   `fuzz/Cargo.lock` 里 `rar-rs` 那一行的 version 改掉就够了。
3. 本地跑测试闸门：`SA_REQUIRE_WINRAR=1 cargo test --workspace --all-features`
   （CI 只跑 smoke；不设这个变量时依赖官方工具的语料套件会静默跳过）。
4. 确认三个 crate 目录里的 `LICENSE` / `NOTICE` / `THIRD_PARTY_LICENSES.md` /
   `LICENSES/` 与根目录逐字节相同（`bash scripts/ci/check-crate-licenses.sh`）：
   `cargo package` 看不到根目录，缺副本就会发出一份没正文的许可声明。
5. 打 tag `vX.Y.Z`（Release job 校验它等于 `crates/rar-napi` 的 Cargo.toml 与
   package.json）。
6. **发 crates.io / npm 都不用手工**：tag 推上去后 `release` 发 GitHub 资产，
   紧接着 `publish` job 用 Trusted Publishing 换临时 token 跑
   `scripts/ci/publish-crates.sh`（依赖顺序、轮询稀疏索引、已存在版本跳过），
   `publish-npm` job 并行走 `scripts/ci/publish-npm.sh`（单包，全部平台二进制
   在一个 tarball 里）。 只有**新名字首次**仍需手工/token：crates.io 要求 crate
   先存在才能挂可信 发布者（token 需 `publish-new`）；npm
   同理，`@yjdyamv/rar-rs` 首发已手工 完成，之后可挂 Trusted Publisher 去掉
   `NPM_TOKEN`。 发布失败只需重跑 `workflow_dispatch` 带同一个
   `release_tag`：已发出的版本 会被跳过。
7. 发布后复核：`cargo install rar-cli --locked` 能装能跑、
   `https://docs.rs/{rar-rs,rar-cli,rar-rs-napi}/X.Y.Z/<crate_with_underscores>/`
   三条 200（`/status.json` 一律
   404，别拿它当判据）；`npm view
   @yjdyamv/rar-rs version` 与
   `npm i @yjdyamv/rar-rs` 后 `require('@yjdyamv/rar-rs')` 能加载。
8. 刷新本文件与 `README` / `CONTEXT` / `ARCHITECTURE` / `CLI` / `testing` 的
   `最后核对` 锚点。
