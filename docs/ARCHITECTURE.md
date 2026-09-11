# 架构与模块布局

库 crate `crates/rar`（crate 名 `rar-rs`）的内部模块地图与设计笔记。

- 用户面向的概览：[`../README.md`](../README.md)
- 磁盘格式权威参考：[`FORMAT_RAR5_RAR7.html`](FORMAT_RAR5_RAR7.html)
- 命令行面：[`CLI.md`](CLI.md)
- 工程状态与差距清单：[`../PLAN.md`](../PLAN.md)（功能矩阵在那边，本文件不复制，避免双份漂移）

## 1 · Workspace

| 路径 | 作用 |
|---|---|
| `crates/rar` | 库 crate `rar-rs` |
| `crates/rar-cli` | binary crate：`rar` + `unrar` |
| `crates/rar-napi` | napi-rs 绑定 `rar-rs-napi`（原生 `.node` + `wasm32-wasip1-threads`）。不发布 npm 包，CI 把产物与 `SHA-256SUMS` 挂到 GitHub Releases，消费方按 SHA-256 固定下载 |
| `fuzz/` | cargo-fuzz 目标，独立 crate |
| `crates/rar/tests/` | 共享集成夹具与互操作测试 |

只有库 crate 有运行时依赖，且不需要任何 RAR/UNRAR 二进制。

## 2 · 模块地图

### 门面与状态（顶层 + `archive/`）

| 模块 | 作用 |
|---|---|
| `lib.rs` | 公开面：角色门面 + 选项/错误/版本 + `raw` 门控的 wire 面 |
| `crc32.rs` | crate 级 CRC32 实现（`pub(crate)`） |
| `archive/reader.rs` / `writer.rs` / `editor.rs` | 读 / 写 / 改三个角色门面 |
| `archive/mod.rs` | `RarArchive` 共享状态与生命周期（内部） |
| `archive/transaction.rs` | 手术式 delete / rename（字节级重写） |
| `archive/create.rs` / `entry.rs` / `discovery.rs` | 写生命周期、条目类型、分卷发现 |
| `archive/rar4_edit.rs` | RAR4 编辑（rename / delete / comment / RR / lock / append / solid repack，含 `-hp`） |
| `model/` | 格式中立模型（`entry.rs` / `chunk.rs`） |
| `version.rs` | `ArchiveVersion` 单一版本表（v15–v70） |
| `options.rs` / `error.rs` / `features.rs` / `write_progress.rs` | 选项、错误、能力报告、进度 |
| `fs/` | 原子暂存、有界读取、卷命名、安全路径 |
| `parallel.rs` | `parallel` 特性的 Rayon 池 |
| `detect.rs` | 签名 / SFX 扫描 |

`name_policy`（`-ep` / `-x` / `-n` 的路径收集与掩码）不在库里，它在
`crates/rar-cli/src/name_policy.rs` —— CLI 是它唯一的消费者。

### 核心子系统

| 模块 | 可见性 | 内容 |
|---|---|---|
| `format/rar5/` | **`raw` 门控** | 常量与词汇（`mod.rs`）、`create.rs`（字典字段策略）、`headers/{parse,serialize,locator}`、`payload.rs`（MemberDecoder）、`vint.rs`、`blake2sp.rs`、`extract.rs`（读路径）、`write/{mod,engine,layout,windows}` |
| `format/rar4/` | **`raw` 门控** | 老容器族：扫描 / 头解析、解码门面、写管线 |
| `codec/modern/lzss_huff/` | **公开** | RAR5 LZSS+Huffman 编解码器。ADR 0003 决策 3 明确保留（`examples/` 依赖根上的 `encode` / `decode` / `EncoderState` / `encode_chunked*`） |
| `codec/legacy/`、`codec/common/` | `pub(crate)` | 老代编解码器与 PPMd；bitstream / huffman / filters / incompressible / match_finder / window |
| `crypto/` | **`raw` 门控** | `rar50`（AES-256-CBC + KDF + hash-key MAC）、`rar15` / `rar20` / `rar30` |
| `recovery/` | **`raw` 门控** | `rar50`（内联 RR）、`rev50`（.rev 恢复卷）、`legacy`（PROTECT_HEAD / NEWSUB 修复）。受支持的入口在 crate 根重导出（`repair_archive_path`、`rebuild_missing_volumes`、`build_recovery_volumes_for_set` 等） |

`raw` 默认关闭：这些是 wire 级与底层原语，不属于受支持的 API，显式开启才可达。
理由与取舍见 `PLAN.md`「为什么要有 `raw` feature」。

**写路径**：`format/rar5/write/*` 增量发射块 —— `engine.rs` 处理 payload 与（可选）CBC
发射，`layout.rs` 决定字典大小并探测 STORE 回退。`archive/transaction.rs` 是手术路径：
delete / rename 字节级复制保留的块、只重发射变更的块，丢弃内联恢复记录并重建 quick-open
记录。

## 3 · 设计笔记

**有界内存。** 大成员从磁盘流式处理：STORE 直接流式，压缩走 4 MiB 分块 + 共享 LZ 窗口，
提取时流式写出而不物化整个文件。压缩发射块合并到 ≤ 4 MiB（每块独立 Huffman 表，符号流
局部分布漂移时提前闭合），解析侧分块预算上限 128 KiB（`MAX_BLOCK_SIZE`）。最坏内存约等于
packed 大小加一个发射块，而不是正比于整个文件的符号表。

**安全提取（默认开启）。** 名字清洗（拒绝 `..`、绝对路径、盘符 / UNC、NUL，Windows 上还
拒绝尾点 / 尾空格与保留设备名）→ 校验解析后的路径落在目标目录内（先校验后建目录）→ 单文件
与总大小上限 → 临时兄弟文件 + 完整性校验后 rename。加密成员校验 MAC'd 校验和，损坏密文必定
被检出。只有对可信归档才应放宽这些默认。

**Solid 与 `-mt`（写路径）。** 连续压缩成员共享一个 LZ 窗口（更好的比率）。RAR5 solid 链
同样走 chunk 级 MT（`encode_chunked_mt`）：窗口经共享 tail 与长距离表延续，只有解析层与
顺序路径分歧（已文档化的小幅 ratio 差异）。非 solid 成员各自独立窗口，`add_batch_parallel`
只在非 solid 时启用；RAR4 老编码器的 solid 链保持串行。结论与实测见
`docs/issues/compression-perf/map.md` 的 issue 06 判决行。

**Quick-open 与取消。** `open_quick` 只读主头 + QO 记录，列目录是 O(QO) 而非 O(归档)；
没有 QO 时回退全扫。长任务通过共享 `AtomicBool` 协作取消，下一个检查点返回
`RarError::Cancelled`。

**流式修复。** `repair_archive_path` 在磁盘上修复受损归档，内存里只持有恢复数据（不是整个
归档），因此可以修复远大于 RAM 的归档；完好时不写输出，失败时不留残留。

**多卷提交是一个事务。** `fs::atomic::commit_files` 先把已存在的目标卷 park 到隐藏旁路，
再安装暂存的卷集；任一步失败就整体回滚（安装过的退回暂存名、park 的原件归位），因此失败的
提交只呈现“完整新集”或“原样旧集”，不会新旧混排。更短的覆盖还会 retire 旧集的残留分卷。
进程在 rename 序列中途被 kill 的恢复仍需磁盘 journal（见 `PLAN.md` 技术债）。
