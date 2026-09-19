# 架构与模块布局

> 最后核对：2026-09-19 @ `c2db547`（提取侧 set-ID 规则、junction 重建为真 NTFS
> 挂载点为本轮改动）；实现细节以源码为准。

库 crate `crates/rar`（crate 名 `rar-rs`）的模块地图与设计不变量。

- 用户面向的概览：[`../README.md`](../README.md)
- 磁盘格式权威参考：[`FORMAT_RAR5_RAR7.html`](FORMAT_RAR5_RAR7.html)
- 命令行面：[`CLI.md`](CLI.md)
- 工程状态与下一步：[`../PLAN.md`](../PLAN.md)（功能矩阵与限制在那边，本文件不复制）

## 1 · Workspace

| 路径                | 作用                                                                        |
| ------------------- | --------------------------------------------------------------------------- |
| `crates/rar`        | 库 crate `rar-rs`                                                           |
| `crates/rar-cli`    | binary crate：`rar` + `unrar`                                               |
| `crates/rar-napi`   | napi-rs 绑定（原生 `.node` + `wasm32-wasip1-threads`）；不发 npm，CI 把产物 |
| `fuzz/`             | cargo-fuzz 目标，独立 crate（不是 workspace 成员）                          |
| `crates/rar/tests/` | 共享集成夹具与互操作测试                                                    |

三个 crate 都不依赖外部 RAR/UNRAR 二进制；库 crate 只用纯 Rust 基础依赖（CRC /
AES / HMAC / SHA / rand / zeroize，`parallel` / `simd` 可选）。

**版本号一致（发布不变式）。** 必须相等的四处：三个 crate 的
`[package]
version`，以及 `crates/rar-napi/package.json` 的
`version`。绑定产物（原生 `.node` 与 wasm）都出自
`rar-rs-napi`，**没有独立版本号**——「napi 版本」就是 「wasm 版本」。另有两处要与
`crates/rar` 对齐：

- 根 `Cargo.toml` 的 `[workspace.dependencies] rar-rs` 的 `version`：不等时
  `cargo package` 直接拒绝。
- 发布 tag `vX.Y.Z`：Release job 只校验它等于 `crates/rar-napi` 的 Cargo.toml 与
  package.json。

改版本号还要刷新 `Cargo.lock` 与 `fuzz/Cargo.lock`（两个 CI 检查都带
`--locked`）。 四者一致由 `lint` job 的 "Workspace versions agree"
步骤守住。**当前同为 `0.9.0`。**

## 2 · 模块地图

### 门面与状态（顶层 + `archive/`）

- `lib.rs` — 公开面：角色门面 + 选项/错误/版本 + `wire` 工具箱（块信封 / varint
  / 模型结构 / 恢复构建 / 加密原语）。
- `crc32.rs` — crate 级 CRC32（`pub(crate)`）。
- `archive/reader.rs` / `writer.rs` / `editor.rs` — 读 / 写 /
  改三个角色门面（共用 `EntryId::resolve` 身份约定与单一 catalog token）。
- `archive/mod.rs` — `RarArchive` 共享状态与生命周期（模块内部；公开面只有经
  crate 根 `pub use` 的角色门面）。
- `archive/state.rs` — `ReadState`/`WriteState` 及其
  `solid`/`rar4`/`compression`/`meta`/`locator`/`output` 组，加
  `Mode`/`PendingCommit`/`StreamRecord`。
- `archive/transaction/` — 手术式 delete / rename（字节级重写；
  `multivolume`/`edit`/`plan`/`execute`/`solid`/`header` 角色模块）。
- `archive/create.rs` / `entry.rs` / `discovery.rs` —
  写生命周期、条目类型、分卷发现。
- `archive/rar4_edit/` — RAR4 编辑（rename / delete / comment / RR / lock /
  append / solid repack，含
  `-hp`；`layout`/`headers`/`comment`/`engine`/`repack` 角色模块）。
- `model/` — 格式中立模型（`entry.rs` / `chunk.rs`）。
- `version.rs` — `ArchiveVersion` 单一版本表（v14–v70）；`LegacyCodec`
  （`pub(crate)`）折叠 legacy 别名并做读/写/repack/密码分派。
- `options.rs` / `error.rs` / `features.rs` / `write_progress.rs` — 选项、错误、
  能力报告、进度。
- `fs/` — 原子暂存、有界读取、卷命名、安全路径。
- `parallel.rs` — `parallel` 特性的 Rayon 池。`detect.rs` — 签名 / SFX 扫描。

`name_policy`（`-ep` / `-x` / `-n` 的路径收集与掩码）**不在库里**，在
`crates/rar-cli/src/name_policy.rs`——CLI 是它唯一的消费者。

### 核心子系统

- `format/rar5/` — 内部（`wire` 导出受支持子集）：常量与词汇（`mod.rs`）、
  `create.rs`（字典字段策略）、`headers/{parse,serialize,locator,quick_open}`、
  `payload.rs`（MemberDecoder）、`vint.rs`、`blake2sp.rs`、`extract/` （读路径
  `open`/`solid`/`decode`/`verify`；`members`/`dest`/`read` 在
  `format/shared/extract/`）、`write/{mod,add,emit,stream,batch,engine,
  filter_policy,layout,windows}`。
- `format/rar4/` — 内部：老容器族。`envelope.rs` 是块信封与 `-hp` 头解密的**唯一
  读取器**；另有扫描 / 头解析、解码门面、写管线。
- `format/rar13/` — 内部：DOS 时代 `RE~^` 容器。读取（旧命名分卷拼装）与创建
  （单卷 + `.rar/.rNN` 分卷、solid / 注释 / `-p`）。
- `format/shared/` — 内部：跨格式读写。读编排（`extract/`：`open`/`members`/
  `dest`/`read` + 每操作唯一 family match，并行抽取仅 RAR5）、跨卷分片合并
  （`split.rs`）、legacy 时间换算（`legacy_time.rs`）、通用 writer 适配器
  （`engine.rs`）、流访问（`stream.rs`）、格式中性成员写门面（`write_ops.rs`：
  `add*` 分发 + solid 链重置）。
- `codec/modern/lzss_huff/` — **公开**：RAR5 LZSS+Huffman 编解码器。
  `codec/mod.rs` 重导出整个模块（`encode*` / `decode*` / `analyze_stream` /
  `trace_stream` / `FilterSpec` / `EncodeOptions` / `DecoderState` 与 Huffman
  常量），crate 根再重导出 `encode` / `decode` / `decode_standalone` /
  `encode_chunked`（`parallel` 下另有 `#[doc(hidden)]` 的 `EncoderState` /
  `encode_chunked_mt`）；`examples/` 两者都用。ADR 0003 决策 3 明确保留。
- `codec/legacy/`、`codec/common/` — `pub(crate)`：老代编解码器与 PPMd；`lz.rs`
  （RAR20/RAR29 共享位读器、规范 Huffman、滑窗 history）、`encode_core.rs`
  （LENGTH/SHORT 表、槽查找、level 表 token）、bitstream / huffman / filters /
  incompressible / match_finder / window。
- `crypto/` — 内部（AES/KDF 原语经 `wire` 导出）：`rar50`（AES-256-CBC + KDF +
  hash-key MAC）、老族 `rar13` / `rar15` / `rar20` / `rar30`。
- `recovery/` — 内部（受支持入口在 crate 根与 `wire` 重导出）：`rar50`（内联
  RR）、 `parity`（`.rev` / 重建卷的 staged 安装值）、`rev50`（RAR5
  `.rev`）、`rev3` （RAR 1.5–4.x `.rev`，GF(2^8)）、`legacy`（PROTECT_HEAD /
  NEWSUB 修复）。受支持 入口如
  `repair_archive_path`、`rebuild_missing_volumes`、
  `build_recovery_volumes_for_set`。

三棵树默认 `pub(crate)`（2026-09 删除 `raw` feature 后永久如此）：它们是 wire
级与 底层原语，不属于受支持的 API；外部真正需要的子集（块信封 + varint +
模型结构 + 恢复构建 + 加密原语）经常驻 `wire` 模块导出。理由见
[`adr/0007-raw-feature-retired.md`](adr/0007-raw-feature-retired.md)。

**写路径。** `format/rar5/write/*` 增量发射块——`engine.rs` 处理 payload 与
（可选）CBC 发射，`layout.rs` 决定字典大小并探测 STORE 回退。
`archive/transaction/` 是手术路径：delete / rename 字节级复制保留的块、只重发射
变更的块，丢弃内联恢复记录并重建 quick-open 记录。

## 3 · 设计不变量

**有界内存。** 成员从不整块进内存。

- STORE（含加密 STORE）按 1 MiB 块直拷进归档。
- 压缩成员走 **spill 文件**（`SpillGuard`，`*.spill-*`）：逐窗压缩，压缩字节先写
  进归档旁的临时文件，同时算明文 CRC/BLAKE2；等 packed 大小与校验和齐了才写成员
  头，再把 spill 流式拷入。所以磁盘上有一份 packed 大小的中间文件，内存只驻留
  **编码器状态 + I/O 缓冲**。
- 内存量级：近程 tail ≤ `min(dict, NEAR_WINDOW_MAX = 8 MiB)`；短程 match finder
  的 head/prev（或 BT4
  son，页按插入惰性提交）与近程窗口同阶；长程采样历史（`-mcl` 风格）≤
  `min(dict, LONG_RANGE_MAX = 128 MiB)` 字节 + 每 16 B 一个样本、≤50% 负载
  的采样表。并行（`parallel` + `-mt>1`）时成员按
  `clamp(8 MiB × 线程数, 24 MiB, 64 MiB)` 切片、每片带 ≤ 8 MiB tail 上下文；顺序
  路径每 4 MiB 读块立即 flush（≈ 4 MiB 读 + 1 MiB BufReader）；拷贝阶段 1 MiB。
- 实测（release，1 GiB 高度可压成员，默认 32 MiB 字典）：m1 顺序峰值 ≈ 130 MB，
  m3/m5 顺序 ≈ 365 MB（BT4 son 数组 + 长程采样历史为主），`-mt8` m3 ≈ 840 MB。
  **内存与成员大小无关，但随线程数增长。**
- 压缩发射块合并到 ≤ 4 MiB（每块独立 Huffman 表，符号流分布漂移时提前闭合）；
  解析侧分块预算上限 128 KiB（`MAX_BLOCK_SIZE`，只为价格局部化）。
- 内存 API（`encode()` / `encode_chunked`，ADR 0003 决策 3 保留的公开面）仍物化
  输入与输出。提取侧对称：解码直接写目标并裁剪窗口（legacy `MAX_HISTORY` 1/4
  MiB， RAR5 = 字典），不物化成员。
- RAR4 各代流式：v29 用其 LZ 流式引擎；v20 用**窗口多块**编码器
  （`encode_member_windowed_streaming`，64 KiB 一块、块间位连续、`ParseState`
  续传匹配状态）；v15 与 RAR13 用 `Unpack15Encoder::encode_member_streaming`
  （单一自适应流改成增量编码：滚动窗口 + 一个读取块 + 前视，跨块续用自适应表，
  字节与整成员编码相同）。加密由 `format/rar4/write/cbc.rs` 的**范围密码发射器**
  分代发射，因此密码不再迫使成员进内存。

**安全提取（默认开启）。** 名字清洗（拒绝 `..`、绝对路径、盘符 / UNC、NUL；
Windows 上还拒绝尾点 / 尾空格、`:` 组件与保留设备名）→ 校验解析后的路径落在目标
目录内（**先校验后建目录**）→ 单文件与总大小上限（CLI 可用 `--max-unpacked` /
`--max-total-unpacked` 收紧）→ 临时兄弟文件 + 完整性校验后
rename。链接目标同样过清洗与包含性校验：junction 重建为真 NTFS 挂载点，非 root
提取时剥掉文件的 set-ID 位（同官方 UnRAR）。加密成员校验 MAC'd 校验和，损坏密文
必定被检出。**只有对可信归档才应放宽这些默认。**

**Solid 与 `-mt`（写路径）。** 连续压缩成员共享一个 LZ 窗口（更好的比率）。RAR5
solid 链同样走 chunk 级 MT（`encode_chunked_mt`）：窗口经共享 tail
与长距离表延续， 只有解析层与顺序路径分歧（已文档化的小幅 ratio 差异）。非 solid
成员各自独立窗口， `add_batch_parallel` 只在非 solid 时启用；RAR4 老编码器的
solid 链保持串行。结论与 实测见
[`issues/compression-perf/map.md`](issues/compression-perf/map.md) 的 issue 06
判决行。

**Quick-open 与取消。** `open_quick` 只读主头 + QO 记录，列目录是 O(QO) 而非
O(归档)；没有 QO 时回退全扫。长任务通过共享 `AtomicBool`
协作取消，下一个检查点返回 `RarError::Cancelled`。

**流式修复。** `repair_archive_path` 在磁盘上修复受损归档，内存里只持有恢复数据
（不是整个归档），因此能修远大于 RAM 的归档；完好时不写输出，失败时不留残留。

**多卷提交是一个事务。** `fs::atomic::commit_files` 先把已存在的目标卷 park
到隐藏 旁路，再安装暂存的卷集；任一步失败就整体回滚（安装过的退回暂存名、park
的原件 归位），所以失败的提交只呈现「完整新集」或「原样旧集」，不会新旧混排。
`StagedSet::park` 还能把既有 final 按调用方命名入 journal（rev3 损坏卷 →
`*.bad`）：rollback 还原、成功保留，kill 落在 park 与 install 之间也由 recovery
还原。更短的覆盖还会 retire 旧集的残留分卷与旧 `.rev`。提交写 journal +
committed 标记，进程在 rename 序列中途被 kill
时，下次写打开会回滚未完成的提交或收尾已完成
的提交（`atomic::recover_interrupted_commit`；**只在写路径触发**，读打开不动盘）。

## 4 · CLI 与测试布局

CLI crate `crates/rar-cli` 产出 `rar` 与 `unrar`：

- `common.rs` — WinRAR 开关/配置兼容核心。
- `input.rs` / `password.rs` / `output.rs` / `time.rs` / `conout.rs` /
  `error.rs` / `listfile.rs` — 输入、口令、输出、时间、控制台、错误、`@listfile`
  展开。
- `selector.rs`（成员选择）、`name_policy.rs`（路径收集与掩码）、`ops.rs`
  （两二进制共享的打开/提取/列表/打印编排，`l` / `v` / `lt` 按 WinRAR 表形态）。
- `bin/rar/` — 按角色拆分（`main` / `args` / `create` / `edit` / `update` /
  `list` / `extract` / `comment` / `recovery` / `sfx` / `filters` / `links` /
  `log` / `transaction`）；`bin/unrar.rs` 是 `unrar` 入口。

测试：`crates/rar/tests/`（`rar50_roundtrip`、`format_assertions`、`rewrite_tests`、
`official_interop`、`rar4_*`、`rar13_*`、`cancel_flag`、`quick_open_listing`
等； 官方 rar/unrar 由 `SA_OFFICIAL_RAR` / `SA_OFFICIAL_UNRAR` 门控）；
`crates/rar-cli/tests/cli_behavior/` 与 `winrar_interop/`（按域拆分 +
`support`， 后者需本机 WinRAR）。怎么跑、耗时与坑见
[`testing.md`](testing.md)；fuzz 目标见
[`../fuzz/README.md`](../fuzz/README.md)；CI 见
[`.github/workflows/CI.yml`](../.github/workflows/CI.yml)。
