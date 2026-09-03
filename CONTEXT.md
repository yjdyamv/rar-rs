# CONTEXT — rar-rs

领域词汇。给架构审查和后续 skill 使用；新术语先查这里，模糊了就地改。

## 领域词汇

- **Archive（归档）** — 一个 RAR5 归档：单卷文件或 `.partN.rar` 分卷集。
- **ArchiveVersion（归档版本）** — *家族 vs 版本*命名：`ArchiveVersion`（`version.rs`）取的是 RAR5 容器**家族**内的编解码**版本**，不是容器本身。现仅两变体 `Rar50`（v50 codec，DC 64 码）与 `Rar70`（v70 codec，DCX 80 码），共用同一容器/签名/信封。`from_v70(bool)` 由成员头（读：`comp_version == 1`；写：字节级字典存在）映射；`uses_extra_dist()` 在 DC/DCX 表间选择。
- **Member（成员）** — 归档中的一个条目（文件 / 目录 / 重定向），对应一个文件头 + 数据区。
- **Volume（分卷）** — 多卷归档的单个 `.partN.rar` 文件；成员数据按卷切成 Chunk。
- **Chunk（分块）** — 跨卷成员在某卷中的数据段。非末块头携带该块密文 CRC32；末块携带（hash-key MAC 过的）明文 CRC，并携带完整 extra 记录。
- **Solid chain（固态链）** — 连续压缩成员共享一个 LZ 窗口；EncoderState/DecoderState 跨成员保持。单卷与分卷均已支持。
- **EncoderState / DecoderState** — 跨块/跨成员保持的编解码状态（lookbehind tail、dist cache、last length、Huffman 表），定义在 `codec/lzss_huff/encoder.rs` 与 `decoder.rs`。
- **Emitted block（发射块）/ parse block（解析块）** — 压缩流两种块：写侧把 LZSS 符号流切成**发射块**（≤ 4 MiB，局部字面量/距离/长度分布漂移时提前闭合，每块独立 Huffman 表）；解析/预算侧分块上限仍 128 KiB（`MAX_BLOCK_SIZE`）。发射块大小与解析块解耦（自适应发射块，2026-09）。
- **MemberDecoder** — `rar50/payload.rs`：统一成员读/解码门面（`ChunkReader` trait + `read_packed` + `decode_member`），STORE 直通与压缩解码共用。
- **Spill file（溢出文件）** — 大文件（≥ `STREAM_COMPRESS_THRESHOLD`，64 MiB）压缩路径的临时落盘文件：压缩流先溢出，头写出后再流式进归档，保证内存有界。
- **Streaming payload（流式负载）** — `write_streamed_payload`（`rar50/write/mod.rs`）：统一流式写路径（单卷/分卷 + 可选流式 AES-256-CBC），`write_stored_file` 是其 STORE 特例。
- **CbcRangeEmitter** — `rar50/write/engine.rs` 中连续 CBC 密文按任意字节区间发出的机制（read-ahead 到块边界 + ≤15B carry），使加密分块边界任意、卷大小仍精确（与 WinRAR 字节级一致）。
- **Header encryption（-hp）** — 归档级加密头（每卷开头明文），其后所有块为 `[IV][AES-256-CBC 加密头]`。
- **Recovery record（恢复记录）** — 单卷内联 "RR" 服务块，奇偶校验保护归档前缀（GF(2^16) Cauchy 矩阵，见 `recovery/rar50.rs`）。
- **Recovery volumes（.rev 恢复卷）** — 分卷集的 Reed-Solomon 奇偶校验卷，可重建缺失卷（`recovery/rev50.rs`）。
- **Quick-open（QO）** — 主头 locator + 末尾 "QO" 服务块，缓存文件头副本加速列表。
- **BLAKE2sp / hash-key MAC** — 成员哈希记录（`-htb`）；加密成员的校验和用 hash key MAC 保护（`rar50/blake2sp.rs`、`crypto/rar50.rs`）。
- **Redirect（重定向）** — symlink / hardlink / file-copy 成员（无数据区，仅 extra 记录）。
- **SFX** — 归档前带 stub 的自解压文件；`detect::sfx_offset_of` 定位归档起点。
- **Locator（定位器）** — 主头中的 QO/RR 偏移记录，close 时回填。
- **Quick-open fast path（QO 快路径）** — `RarArchive::open_quick`：只读主头 locator + QO 记录即得成员列表（O(QO) 而非 O(归档)）；无 QO 时透明回退全扫。
- **Streaming repair（流式修复）** — `repair_archive_path(src, dst)`：文件版 `{RB}` 扫描 + shard 级按需读取，只驻留恢复数据与损坏分片；完好不写输出、失败不残留。
- **Cancel flag（取消钩子）** — `set_cancel_flag(Arc<AtomicBool>)`：长操作在逐成员/逐块检查点返回 `RarError::Cancelled`；binding 映射 AbortSignal。
- **Zero-padded volumes（零填充卷）** — WinRAR 把卷号填充到总卷数位数（`part01..part15`）；发现/重建/.rev 命名均识别。

## 分层结构（镜像参考架构 rars）

- **格式层 `rar50/`**：容器常量 + 头类型/解析（mod.rs + headers/{parse,serialize,locator}.rs）、成员读/解码门面（payload.rs）、读路径（extract.rs）、写管线（write/{mod,engine,layout}.rs）、vint/blake2sp。将来低版本格式加 `rar40/` 等兄弟模块。
- **编解码层 `codec/`**：一族一目录（codec/lzss_huff/{mod,encoder,decoder}.rs = LZSS+Huffman 编码器 + 解码器 + encode/decode 分发）；bitstream/huffman/filters/match_finder/window 为共享原语。
- **加密层 `crypto/`**：一族一文件（crypto/rar50.rs）。
- **恢复层 `recovery/`**：rar50.rs（内联 RR）+ rev50.rs（.rev 卷）。
- **基础设施**：detect.rs（签名/SFX 扫描）、parallel.rs（Rayon 池）、io_util.rs（原子暂存/有界读）、version.rs/features.rs（薄词汇模块）、options.rs/error.rs/write_progress.rs。
- **CLI 层 `crates/rar-cli`**：rar/unrar 两二进制；common.rs（WinRAR 开关/配置兼容核心）+ input/password/output/time 模块。

## 项目事实

- Cargo workspace：库 crate `rar5`（RAR5-only 创建/读取，明确拒绝 RAR4）+ CLI crate `rar-cli`（`rar` 创建/修改/提取、`unrar` 提取/列表）。
- 库热点已拆解：`archive/` 目录是 facade 层（`mod.rs` 结构体/构造器/生命周期，`create.rs` 写生命周期，`rewrite.rs` 外科重写，`entry.rs` 条目类型，`discover.rs` 分卷发现）；读写路径分别在 `rar50/extract.rs` 与 `rar50/write/`。
- 互操作测试：`crates/rar/tests/{rar50_roundtrip,format_assertions,rewrite_tests,official_interop,rar4_rejection,cancel_flag,quick_open_listing}.rs`（官方 rar/unrar 用 SA_OFFICIAL_RAR/UNRAR env 门控）、`crates/rar-cli/tests/cli_behavior.rs`（CARGO_BIN_EXE 需随二进制所在 crate）、`crates/rar-cli/tests/winrar_interop.rs`（Windows 本机 WinRAR 双向验证）。
- fuzz：`fuzz/` 独立 crate（不在 workspace），五目标 parse/crypto/recovery（读侧）+ write/rewrite（写侧），standalone 变异循环 + `cargo +nightly fuzz run <t> --features fuzzing` 双模式；语料嵌入真实 WinRAR fixture。
- 回归验证：fmt/clippy `-D warnings` 双门（本地手动跑）+ fuzz（`fuzz/` 五目标，standalone 变异循环 + libFuzzer 双模式）。
- 迁移记录：仿 rars 架构重构的完整计划与决策（见 `PLAN.md` 与 git 历史）。
- 文档索引：`docs/README.md`（所有文档的导航入口）；格式细节见 `docs/FORMAT_RAR5_RAR7.html`（以本实现为准，冲突处对照 rars）。
- 计划：`PLAN.md`（已完成记录 + WinRAR 7.23 差距清单）。
