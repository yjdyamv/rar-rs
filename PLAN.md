# rar-rs 计划

> 最后核对：2026-10-28 @ working tree；实现细节以源码为准。

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
  solid、 `-p` / `-hp`、旧命名分卷、归档注释（`-ma15`/`-ma2` 把注释嵌进主头， 与
  1.5/2.x 读取器同构，2.90 可原样读回；这类归档的注释只能创建时写，
  编辑一律拒绝）。
- **RAR4 编辑全补**（ADR 0005）：头/块级操作 + 非 solid 块拷贝 + solid 整档
  repack； `-hp` 与分卷（`rn`/`ch`/`k`/注释）均已支持。
- **命令面**：官方 `rar` 全部命令（含 `rv` 补恢复卷、`lb/lt/vb/vt`
  列表变体）；官方 7.30 新增的 `la`/`va`/`lba`/`vba` 与服务块列表已接。
- **成员顺序与目录形状对齐官方**：solid 归档按官方的分组规则排序（扩展名 →
  基本名 → 大小 → 全名，目录条目排最后且深度降序），`update`/`freshen` 追加的
  成员同样排序（`-m0`/`-ds` 保持收集顺序），目录成员不带尾分隔符（RAR5
  `FILE_FLAG_DIRECTORY`、RAR4 `0x10`），`rarfiles.lst` 的优先级与子集规则照抄。
  契约见 [`docs/PITFALLS.md`](docs/PITFALLS.md)「solid 成员的顺序」。
- **对外两组 API**：Rust 库是角色门面（`ArchiveReader` / `ArchiveWriter` /
  `ArchiveEditor`）；Node 绑定（`crates/rar-napi`）**手写入口 `rar-rs.js` +
  `rar-rs.d.ts`**，生成物只叫 `binding.*`，`tsc --noEmit` 在 CI
  把关。错误类别一路传到消费者（`ErrorCode` → CLI 退出码 /
  `RarError.rarCode`），提取可选逐成员容错（`ExtractErrorPolicy`）并带进度。
- **工程**：workspace 三 crate；CI 做 fmt / 路径分隔符守卫 / clippy
  `-D warnings` 与 rar-rs 各 feature 组合 / wasm 目标 `cargo check` / 确定性测试
  smoke / 版本一致性 / cargo deny / rustdoc，绑定侧的类型闸门 `tsc --noEmit` 在
  build 矩阵里跑一次；另有一条 `lint-windows`（clippy 全目标 + WinRAR 开关契约 +
  绑定单元测试），cargo deny 每周定时重跑一次以接住新的 RustSec 公告；七目标
  fuzz；取消钩子；QO 快路径；流式修复； 零填充分卷。
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
- **测试归档零提交**：legacy 套件（RAR 1.3–4.x、`.rev`、VM 过滤器、恢复记录）
  全部由 `.cache/winrar/` 的官方 `Rar.exe`
  现场生成（`tests/support/generate.rs` + `corpus.rs`），RAR 1.3/1.4 与
  v26/v36/v70 用我们自己的 writer 或改 `unp_ver` 字节；`tests/data/`
  只留两份原始载荷。代价：没有 tool cache 的机器上这些套件 打 `SKIP:`
  跳过（本地闸门覆盖，CI smoke 不覆盖）。

## 下一步

### 发布（暂缓：用户决定先不发 crates.io）

版本面已经收口：五处版本一致（三个 crate 的 `[package] version`、
`crates/rar-napi/package.json`、根 `Cargo.toml` 的 workspace 依赖）、两个
`Cargo.lock` 已刷新、`cargo package` 校验通过、三个 crate 的
`readme` / `keywords` / `documentation` / `categories` 元数据已补齐、tag
`v0.13.0` 已在。**发布本身按用户要求先不做**，恢复时只剩执行：

- [ ] **`cargo publish` 按顺序**：`rar-rs` → `rar-cli` / `rar-rs-napi`
      （后两者依赖 workspace 内的 `rar-rs`，必须先有前者）。
- [ ] **照「发布清单」逐项过**（本文件末段；清单之外没有隐藏步骤）。
- [ ] **发布后做一遍下游实测**：从 crates.io 干净拉取 `rar-cli` 的临时工程，
      `cargo install rar-cli` 能跑；`rar-rs-napi` 的 `.node` 与 wasm 产物照 CI
      release job 的路径复核一次。

### 文档人体工学（已做，保持即可）

- [x] **单一来源归位**：历史修复日志移出 `PLAN.md`，可长期复用的规则收进
      [`docs/PITFALLS.md`](docs/PITFALLS.md)（一条一行、由测试钉住），架构级不变量留在
      [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)，退出码进
      [`docs/CLI.md`](docs/CLI.md)。`PLAN.md` 从 995 行收到 200 行量级。
- [x] **公开 API 契约也归 PITFALLS**：错误类别贯通、`ExtractErrorPolicy` 的
      Abort/Collect 语义、提取进度粒度、生成物与手写物的分名、WASI
      路径映射表按名维护——见 [`docs/PITFALLS.md`](docs/PITFALLS.md)「公开 API
      契约」段。
- [ ] **保持**：新结论写进
      PITFALLS（规则）或本文件（下一步），**不要把过程写回来**。
      文档锚点（`最后核对`）在改行为时同步刷新。

### 测试覆盖（对齐 CI）

- [x] **CI 拉官方工具跑生成套件**：`scripts/fetch_winrar.py`（标准库，双平台）
      是唯一下载入口，两个 GitHub job 都先跑它；`lint-windows` 用
      `SA_REQUIRE_WINRAR=1` 跑全套（覆盖 2.90/3.00 配方），Ubuntu job 跑同一批
      target 但允许自跳（rarlab 的 Linux 版从 3.93 起）。 注意上限仍是 RAR4 写入
      6.23、`cf` 4.20、`rv3` 4.20。

### 通用库的对外面（已做一半，剩打包）

- [x] **错误分类贯通到 JS**：库的 16 个 `ErrorCode` 经消息标记 + `rar-rs.js`
      再水化，变成 `RarError.rarCode`；此前 17 类被压成 2 个 N-API
      status，"密码错→重问" 与"锁定→只读"无法分辨。
- [x] **逐成员失败与提取进度**：`ExtractErrorPolicy`（`Abort` 默认 /
      `Collect`）、
      `ExtractionReport::failures`、`ExtractOptions::on_progress`；绑定侧
      `ExtractArchiveOptions.collectErrors` 与 `ExtractionResult.failures`。
- [x] **手写类型 + 编译闸门**：`rar-rs.d.ts` 为公开面，`npm run typecheck`
      （`tsc --noEmit` + `__test__/types.test-d.ts` 的编译期断言）进 CI。
- [ ] **npm 打包（明确暂缓，别自行推进）**：多平台 `optionalDependencies`、 wasm
      入口、`exports` 子路径、发布工作流。**按用户要求先不做**。开工前需定一个
      **scope 名**：napi-rs 官方文档警告无 scope 的平台包名（如
      `rar-rs-win32-x64-msvc`）可能触发 npm 的 spam 检测，建议平台包放 scope
      下、主包保持 `rar-rs`。
- [ ] **CI 首跑**：跨平台 build 矩阵与新增的 typecheck 步骤只做过静态修改，
      首次跑通必须由 CI 证明（本地无法覆盖）。

### 已知差异（已量化，追平需先逆向公式）

- [ ] **`-qo` + 分卷的 8 字节零尾**：布局已追平（每卷各一个 QO 服务块 +
      每卷主头的 `LOCATOR` 记录，条目 `rel` 按「本卷 QO 块 → 本卷分片头」
      重算，已实现并有官方 `UnRAR` 对拍），只剩官方每卷 ENDARC 后留的
      8 字节零尾：我们填满不留，两边都能被官方 `t`/`x` 接受，属“有意不追平”。
      事实与实测数字见 [`docs/PITFALLS.md`](docs/PITFALLS.md) 的 QO 段。
- [ ] **`-rr` + `-v` 的零尾**：官方按预留量写记录再补 0 填满卷，
      松弛量实测不规则（32 KiB 卷 `-rr1%`/`3%`/`5%`/`20%` 都是 7 字节，
      `-rr10%` 是 393 字节，256 KiB 卷 5 字节），两边分块因此差这几字节。
      官方 `t`/`rc` 两边都接受，`.rev` 本体逐字节相同，属于“有意不追平”。

### 性能（已 park，未关闭议题）

- [ ] **issue 09 — DLL 单线程解析速度**：真实 DLL 上 m3 `-mt1` 落后 WinRAR 约
      5.9x，瓶颈是 BT4
      下降步数（结构锁定：`HASH_BITS`、dict-log、提交阈值、近/远带宽四个旋钮已验证弹回）。
- [ ] **issue 15 — 价格驱动解析提速**（2026-09-18
      立项）：目标是**压缩速度**——给解析器补中间档，把每位置代价从更深的搜索换成更省的价格计算。杠杆 3 的「DP 块顺延价格表」已落地（第 17 项，MT 比率白送 0.1–0.8pp）；剩下的断崖是 seq 的 `-m1`/`-m2` 之间。四个杠杆、上游事实与验收判据见
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
| 13 | 远带候选预算（原 `RAR_RS_FAR_BAND`） | 仅 seq opt-in 有效（−9% @ +0.22pp），非 mt8 解药；已删除，不留开关         |

已落地（别再重新论证）：01 matchless-block DP 快路径（字节相同）、02 collector
fast-mode 门（`longest==0`，阈值 256）、03 delta 候选通道 + 采样预门、05
流式路径 auto delta/x86、08 持久树跨 chunk 增长的损坏修复、11 BT4 首步
value-carry（−3.6%）、 12 MT 近窗对齐（已被 13 取代）、 16 哈希链走查预取
（`match_finder.rs`：提前载入下一候选并 `prefetch` 比较字节，字节相同，
RAR4 `-m5` −26%、RAR5 `-m5` −18%，solid 持平——候选已并行、带宽吃满）、
17 MT 的 DP 块顺延价格表（`windowed_priced_parse` 把上一块的码长表交给下一块首次
定价，替代「多跑一遍重定价」，text l3 −0.51pp / 30 MB 文本 −0.6~0.8pp，耗时持平；
MT 本来就是已接受的字节分歧，packed 只变小）。

**当前基线**（2026-10-27，`ntoskrnl.exe` 12.5 MiB、`-ma5`、本机 16 核、release，
每档两次取最快；「MT」= 默认线程、「seq」= `-mt1`）

| `-m` | 我们 MT      | 我们 seq     | 官方 7.23    | MT 相对官方 |
| ---- | ------------ | ------------ | ------------ | ----------- |
| 1    | 2.3 s 45.24% | 1.8 s 51.46% | 0.3 s 55.00% | −9.8pp      |
| 2    | 3.2 s 44.85% | 13.7 s 44.09% | 0.5 s 47.16% | −2.3pp      |
| 3    | 3.7 s 44.55% | 13.3 s 43.88% | 0.7 s 43.03% | +1.5pp      |
| 4    | 4.3 s 44.34% | 16.0 s 43.80% | 0.9 s 42.90% | +1.4pp      |
| 5    | 5.2 s 44.19% | 18.6 s 43.77% | 0.8 s 42.85% | +1.3pp      |

`-m1`/`-m2` 我们**比官方小**（官方的这两档是「尽量快」的弱解析，我们最低档也跑 DP），
`-m3..-m5` 差 1.3–1.5pp；`-mt1` 能把差距收到 0.85–0.95pp，代价是 13–19 s。**速度仍是主要差距**：MT 比官方慢 5–7×、seq 慢 19–26×
（issue 09 的结论：再降步数只剩显式取舍，流水化原型已实测为负）。

**剩余公开差距**：DLL 单线程解析约 6–8 s vs WinRAR 1.8 s（其中 mt8 7.5×，见
issue 09）；xml m2/m3 +1.5%（解析差距，非块开销）；text64 MT 片间分歧（6554 vs
seq 6058 B）。各日期、各口径的实测表是**过程记录**，需要时
`git log -- docs/issues/compression-perf/` 找回。

### 功能缺口（按需，不阻塞发布）

- [x] **RAR4 solid 归档 MT**：成员级并行不需要跨成员共享窗口——真正共享的是
      *进入成员之前*的链状态，而每个候选（plain LZ / 逐个 `-mc` 过滤 / PPMd
      试压）对它都只读，所以候选之间可以并行、胜负仍按串行顺序裁决，字节不变
      （2026-10-27 已实现，`search_solid_member_parallel`，契约
      `solid_search_parallel_matches_serial`）。30 MB 文本 solid m5：23.2 s →
      14–17 s（16 核）；剩下的差距在 match finder
      本身（官方 1.0 s），不是并行度。
- [ ] **逐文件与上游对拍**：`rars` 移植的出处目前是 in-tree claim（每个文件头 +
      inventory 表），尚未与上游 diff。不影响已定的许可表达式。

### 有意不做（设计决定，别当缺口修）

- **老编码器块级 MT（v15/v20 单个大成员）**：v20 已有「多窗口并行解析 +
  顺序写位流」 的骨架，但需要每窗口独立的 match finder
  状态与确定性窗口边界才能保证字节一致，收益仅「老格式大成员的创建速度」（官方
  7.23 已移除 `-ma4`）；v15 另受自适应表限制。
- **PPMd 块级MT**：单自适应模型，切块会改变输出（结构上不可行）；成员级并行已有。
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
  不自动填**；库侧给了 `WriterOptions::estimated_size(bytes)`，给出预计大小即
  按官方分档预留。已对拍：`-m0 -qo+` 的单成员归档在 4 / 5 / 6 字节档（1 KiB、
  100 KiB、8 MiB 成员）都与官方**逐字节相同**
  （`locator_width_follows_official_size_tiers`）。
  不给 `estimated_size` 则沿用历史定长 5 字节，比官方 +2 字节（最小档）到 −1
  字节（≥256 MiB 档）。
- **官方 QO 的默认阈值**：官方默认只给「相对大的文件」拷头（手册原文，阈值随
  版本变），`-qo-` 关、`-qo+` 强制全拷；`WriterOptions::quick_open(true)` 的语义
  等于 `-qo+`，所以对拍 QO 布局时官方必须也带 `-qo+`。只影响字节外观（官方多一条
  QO 服务块），不影响读取——无 QO 时双方都回退全扫。

## 开放议题

`docs/issues/<feature>/` 只留**未关闭**议题；关闭时判决并入本文件或
[`docs/PITFALLS.md`](docs/PITFALLS.md)，并删掉文件。

- [`compression-perf/`](docs/issues/compression-perf/) —
  04（窗口级不可压缩跳过）、 09（DLL 解析速度）、15（价格驱动解析），均对应上面
  「性能」段的清单项。

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

- **`lb` 分片成员**：官方 `Rar.exe lb` 对跨卷成员不打印该成员名，官方
  `UnRAR.exe lb` 与我们一致；我们随 UnRAR。
- **同一次 `a` 里的重复成员**：同一个文件既当显式参数、又落在目录参数里时，
  官方按 `-o+` 的覆盖语义把它挪到末尾，我们保留原位（其余排序规则已对齐，
  见 [`docs/PITFALLS.md`](docs/PITFALLS.md)「solid 成员的顺序」）。
- **`-ts` 的「字母+数字」组合**、**`-rr<N>%` 取整**、**`rar rr` 恒 3%**、
  **`rar r` 不写 `fixed.<name>`**、**无控制台时的覆盖询问**：口径与理由见
  [`docs/PITFALLS.md`](docs/PITFALLS.md)「有意不追平官方」。
- **RAR5 元数据仍差两处**（字节外观，双向读写一致）：locator
  偏移字段宽度的**默认值**（给了 `estimated_size` 即对齐）与「官方 QO 默认
  阈值」，见上面「暂缓」段。
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
