# CONTEXT — rar-rs

领域词汇。给架构审查和后续 skill 使用；新术语先查这里，模糊了就地改。

## 领域词汇

- **Archive（归档）** — 一个 RAR 归档：单卷文件或 `.partN.rar` 分卷集。RAR5 容器（8 字节签名）或 RAR4 老容器（7 字节签名）。
- **ArchiveVersion（归档版本）** — *家族 vs 版本*命名：`ArchiveVersion`（`version.rs`）取的是 RAR5 容器**家族**内的编解码**版本**，不是容器本身。现仅三变体 `Rar40`（v29 codec，LZSS+Huffman，7 字节签名）、`Rar50`（v50 codec，DC 64 码）与 `Rar70`（v70 codec，DCX 80 码），共用同一容器/签名/信封（Rar40 除外，它用独立容器）。`from_v70(bool)` 由成员头（读：`comp_version == 1`；写：字节级字典存在）映射；`uses_extra_dist()` 在 DC/DCX 表间选择。
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
- **Legacy family（老容器族）** — `Rar!\x1a\x07\x00` 7 字节签名的 RAR 1.5–4.x 容器（`rar40/`），与 RAR5 8 字节签名区分；固定宽度头 + 16 位头 CRC（ext-time 尾不在覆盖内）、`format_version: 4`。读路径：`rar40/mod.rs` 扫描/解析（`Rar4VolumeScan` 跨卷 split 合并：每卷一个 chunk；`-hp` 主头 MHD_PASSWORD 后每块按 `[8B salt][align16(head_size) 密文]` 解密再解析，data 起点为块起始+磁盘头长 `header_end`）→ `rar40/read.rs` 成员解码门面（跨卷按 chunk 读取拼装后解密/解码）。写路径：`rar40/write.rs` 独立写管线（固定宽度头序列化 + RAR29 编码器 + 成员级加密 + 成员边界多卷切分）。**写侧全能力（2026-09）**：LZSS m1–m5 + PPMd 编码（rars 编码半，非 solid 与 solid 链模型延续）+ 六大标准 VM 过滤器写侧（E8/E8E9/Delta/Audio 自动探测；RGB/Itanium 编码就绪）+ `-hp` 头加密写侧 + NEWSUB 0x7a RR 恢复记录写/修 + **多文件并行 batch**（非 solid 独立成员池并行、字节与顺序一致）+ solid 链 PPMd 模型（LZ levels 与 PPMd model 双链状态、赢者推进）。读侧：预 RAR3 代 solid 链按归档级 MHD_SOLID（`rar4_solid_archive`）+ 大成员流式提取（STORE 直拷 / 压缩解码器增量 flush + 窗口裁剪，`decode_member_bytes_to`）。多卷发现 `discover_volumes` 支持 `.partN.rar`（新命名）与 `.rar/.rNN`（老命名，r→z 每百卷升字母，任意卷入口）。
- **Rar29Decoder（`codec/rar29.rs`）** — RAR 3.x/4.x（unp_ver ≥ 29）成员解码器（rars 解码半移植）：自带 MSB 位读器 + 规范 Huffman 表；成员=块序列，块头（byte 对齐）= PPMd 标记（bit1 + init byte → `codec/ppmd.rs` 的 PpmdDecoder/RangeDecoder）或 LZ 头（keep-tables 位 + 可选新表），块间可混切模式。成员尾消费块控制符（`SameFileNewTable`/`NewFileKeepTables`/`NewFileNewTables`；PPMd 走 esc 结束符）——solid 链跨成员靠它续表/换表，每个成员自己的 packed 区从块边界起新位读器。窗口保留 ≤ 4 MiB（`MAX_HISTORY`）。**VM 过滤记录**（LZ symbol 257 / PPMd esc-3）解析为待应用过滤器（`VmFilter`/`VmProgram`），成员输出时经 `filtered_range` 逆变换；标准过滤器（E8/E8E9/Itanium/Delta/RGB/Audio）按指纹（XOR=0 + len/CRC32）识别并原生执行，非标准（通用）VM 程序显式 Unsupported。
- **Rar15Decoder（`codec/rar15.rs`）** — RAR 1.5（unp_ver 15）解码器（rars `Unpack15` 近逐字提取）：标志位驱动 LZ + 自适应 Huffman（`ch_set*`/`n_to_pl*` 表随解码自组织）+ st 运行模式，64 KiB 环窗（`window`/`unp_ptr`）；流以 `new_final` 结尾标记读取（尾部零填充）。解码器已带 `solid` 参数（保留窗口/表），但 rar4 链接线未做（每成员新解码器）。
- **Rar20Decoder（`codec/rar20.rs`）** — RAR 2.x（unp_ver 20/26）LZSS+Huffman 解码器（rars 解码半移植）：自含 MSB 位读器/规范 Huffman（与 rar29 同构，按 rars 惯例逐文件复制）；块头 16 位 peek——bit15=**音频块**（每通道 Huffman 表 + 自适应 delta 预测，`AudioState`），bit14=keep-tables，其余为 LZ 块（主 298 符号：256 重末匹配/257–260 旧偏移/261–268 短距/269 块尾/270–297 全长匹配）；level 长度 19×4bit 直读（无 RAR3 的 0xF 逃逸）。成员尾 `read_last_tables` 消费块尾标记以续链。solid RAR2.x 链暂不支持（每成员新解码器）。
- **PpmdDecoder（`codec/ppmd.rs`）** — PPMd 变体 H 解码器（rars 解码半移植，编码器不移植）：Suballocator（12 B 单元、双端 bump + 空闲桶 + glue）+ 上下文模型（contexts Vec 模拟 C 指针布局）；`decode_init` 由块头 init byte（reset/阶/字典 MB/esc 标记）重启模型，`decode_symbol` 出符号。错误走自带 `Error`（InvalidData/NeedMoreInput），rar29 侧 From 映射。
- **Legacy solid chain（老固态链）** — `extract.rs` 对 RAR4 用常驻 `rar4_decoder`（`ReadState`）+ `rar4_decoded_through` 索引镜像 RAR5 链解码；链内 STORE 成员断链（窗口重开）。solid 排序由 WinRAR 决定，链起点=归档首文件。

## 分层结构（镜像参考架构 rars）

- **格式层 `rar50/`**：容器常量 + 头类型/解析（mod.rs + headers/{parse,serialize,locator}.rs）、成员读/解码门面（payload.rs）、读路径（extract.rs）、写管线（write/{mod,engine,layout}.rs）、vint/blake2sp。低版本格式兄弟模块 `rar40/`（读：扫描/头解析 + 成员解码门面；写：固定宽度头序列化 + RAR29 编码器调度）。
- **编解码层 `codec/`**：一族一目录/一文件——`lzss_huff/`（RAR5 LZSS+Huffman 编码器+解码器）、`rar29.rs`（RAR3/4 成员解码器：LZSS+Huffman+PPMd 块）、`rar29_encoder.rs`（RAR3/4 编码器：从 rars 移植的 Unpack29Encoder）、`ppmd.rs`（PPMd 变体 H 解码器，rar29 引用）；bitstream/huffman/filters/match_finder/window 为共享原语（`rar29.rs`/`ppmd.rs` 自含位读器/错误，不动 RAR5 原语）。
- **加密层 `crypto/`**：一族一文件（crypto/rar50.rs）。
- **恢复层 `recovery/`**：rar50.rs（内联 RR）+ rev50.rs（.rev 卷）。
- **基础设施**：detect.rs（签名/SFX 扫描）、parallel.rs（Rayon 池）、io_util.rs（原子暂存/有界读）、version.rs/features.rs（薄词汇模块）、options.rs/error.rs/write_progress.rs。
- **CLI 层 `crates/rar-cli`**：rar/unrar 两二进制；common.rs（WinRAR 开关/配置兼容核心）+ input/password/output/time 模块。

## 项目事实

- Cargo workspace：库 crate `rar-rs`（读取 RAR 1.5–4.x/RAR5/RAR7，创建 RAR4/RAR5/RAR7）+ CLI crate `rar-cli`（`rar` 创建/修改/提取、`unrar` 提取/列表）+ `rar-rs-napi`（native/WASI binding）。
- 库热点已拆解：`archive/` 目录是 facade 层（`mod.rs` 结构体/构造器/生命周期，`create.rs` 写生命周期，`rewrite.rs` 外科重写，`entry.rs` 条目类型，`discover.rs` 分卷发现）；读写路径分别在 `rar50/extract.rs` 与 `rar50/write/`。
- 互操作测试：`crates/rar/tests/{rar50_roundtrip,format_assertions,rewrite_tests,official_interop,rar4_rejection,cancel_flag,quick_open_listing}.rs`（官方 rar/unrar 用 SA_OFFICIAL_RAR/UNRAR env 门控）、`crates/rar-cli/tests/cli_behavior.rs`（CARGO_BIN_EXE 需随二进制所在 crate）、`crates/rar-cli/tests/winrar_interop.rs`（Windows 本机 WinRAR 双向验证）。
- fuzz：`fuzz/` 独立 crate（不在 workspace），五目标 parse/crypto/recovery（读侧）+ write/rewrite（写侧），standalone 变异循环 + `cargo +nightly fuzz run <t> --features fuzzing` 双模式；语料嵌入真实 WinRAR fixture。
- 回归验证：根 `.github/workflows/CI.yml` 执行 workspace fmt、默认/无默认 feature check、全 target clippy `-D warnings`、测试、独立 fuzz workspace check，以及 native/WASI binding 构建与测试；官方二进制互操作仍由 `SA_OFFICIAL_RAR`/`SA_OFFICIAL_UNRAR` 手动门控。
- 迁移记录：仿 rars 架构重构的完整计划与决策（见 `PLAN.md` 与 git 历史）。
- 文档索引：`docs/README.md`（所有文档的导航入口）；格式细节见 `docs/FORMAT_RAR5_RAR7.html`（以本实现为准，冲突处对照 rars）。
- 计划：`PLAN.md`（已完成记录 + WinRAR 7.23 差距清单）。
