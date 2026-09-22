# rar-rs 计划

> 最后核对：2026-09-22 @ `8fd1f2e`（本轮：文档失效引用清理 + WinRAR 对齐路线图；
> P1 退出码、P2 交互式覆盖询问、P3 `rar r` 无记录重建（含 RAR5/RAR4 头损坏打捞、
> legacy RR 定位加固与「记录够不着损坏」的询问/退出码对齐）、 P4 RAR5 `-hp`
> 编辑均已落地；另对齐 `-htb` 语义、列目录 QO 快路径、RAR13 修复行为、静默模式按
> 二进制的覆盖语义， 并记录 RAR5 元数据的字节差异（对拍官方 Windows 与 Linux
> 两个构建））； 实现细节以源码为准。

本文件只留**结论**与**下一步**：过程与逐批验证记录在 git 历史
（旧版详单：`git show c2c43d4:PLAN.md`），本文件不维护 CHANGELOG。

相关文档：术语 [`CONTEXT.md`](CONTEXT.md) · 模块图
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) · 字节格式
[`docs/FORMAT_RAR5_RAR7.html`](docs/FORMAT_RAR5_RAR7.html) · 性能议题
[`docs/issues/compression-perf/`](docs/issues/compression-perf/) · 出处
[`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md)

## 待办

### 发布收口

- [ ] **打包与发布顺序**：`rar-rs`（0.10.0）已能 `cargo package` 并通过校验；
      `rar-cli` 依赖 workspace 内的 `rar-rs`，需先发布 `rar-rs`。三个 crate 的
      `readme` / `keywords` / `documentation` / `categories` 元数据已补齐，但
      `rar-cli` / `rar-rs-napi` 仍因 `rar-rs` 未发布而无法解析依赖。
- [ ] **逐文件与上游对拍**：`rars` 移植的出处目前是 in-tree claim（每个文件 头 +
      inventory 表），尚未与上游 diff。不影响已定的许可表达式。

### 功能缺口（按需，不阻塞发布）

- [ ] **RAR4 solid 归档 MT**：legacy solid 链保持串行；成员级并行需跨成员共享
      窗口，属结构性代价（RAR5 的 chunk 级 MT 已兑现）。

**有意不做（设计决定，别当缺口修）：**

- **老编码器块级 MT（v15/v20 单个大成员）**：v20 已有「多窗口并行解析 + 顺序写
  位流」的骨架，但需要每窗口独立的 match finder 状态与确定性窗口边界才能保证
  字节一致，收益仅「老格式大成员的创建速度」（官方 7.23 已移除 `-ma4`）； v15
  另受自适应表限制。
- **PPMd 块级
  MT**：单自适应模型，切块会改变输出（结构上不可行）；成员级并行已有。
- **RAR13 非 solid 成员级 batch**：成本低但价值最低（DOS 时代格式）。

### 性能（未关闭议题，已 park）

- [ ] **issue 09 — DLL 单线程解析速度**：真实 DLL 上 m3 `-mt1` 落后 WinRAR 约
      5.9x，瓶颈是 BT4 下降步数（结构锁定：HASH_BITS、dict-log、提交阈值、近/远
      带宽四个旋钮已验证弹回）。
- [ ] **issue 04 — MT 随机数据窗口级不可压缩跳过**：成员级 STORE 兜底已让随机
      数据领先 WinRAR 10–80x；窗口级跳过有把边界成员从压缩翻成 STORE 的比率
      风险，需先有边界语料量化。
- [ ] **issue 15 — 价格驱动解析提速**（2026-09-18 立项）：目标是**压缩速度**——
      给解析器补中间档，把每位置代价从更深的搜索换成更省的价格计算。默认档字节
      不动（比率是契约）。四个杠杆、上游事实与验收判据见
      [`docs/issues/compression-perf/15-fl2-zstd-parser-tiers.md`](docs/issues/compression-perf/15-fl2-zstd-parser-tiers.md)。

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
value-carry （−3.6%）、12 MT 近窗对齐（已被 13 取代）。

**剩余公开差距**：DLL 单线程解析约 6–8 s vs WinRAR 1.8 s（其中 mt8 7.5×，见
issue 09）；xml m2/m3 +1.5%（解析差距，非块开销）；text64 MT 片间分歧（6554 vs
seq 6058 B）。各日期、各口径的实测表（level ladder、与 WinRAR
的多轮头对头、寄存器 级 A/B）是**过程记录**，随 `map.md` 移出长期文档，需要时
`git log -- docs/issues/compression-perf/` 找回。

### 暂缓（等决策，不自行推进）

- **STORE 成员竞态**：单遍 STORE 先 `hash_file` 再重读同一路径，同尺寸改写真可能
  写出旧 CRC/BLAKE2；回填头需要 patching（`-hp` 还要重加密），已接受。

## 已修（结论与不变量）

一条一行，细节在 git 历史。**括号里的是契约，别改回去。**

**正确性**

- RAR4 `-hp` 错口令判定不确定 → 加密块的垃圾 `head_size` 与解析/CRC 失败**统一
  映射为 `WrongPassword`（CLI → exit 11）**。RAR4 没有口令校验值，错口令与损坏头
  本就不可区分，不要试图"区分"它们。
- Linux 构建断裂 → `serialize.rs::build_stream_block` 等三处多余的
  `#[cfg(windows)]` 去掉（`OS_WINDOWS` 是格式常量，不是编译门）。
- RAR4 batch 与 sequential 偶发不一致 → **不是竞态**：`local_offset_secs()` 把
  本地时间与 UTC 分两次采样，Windows `GetLocalTime` 按 ~15.6 ms tick 前进，差值
  偶发差 1 秒（约 1.9%），翻转 DOS ext-time 的 `ADD_SECOND` 位。两处（库 + CLI）
  都把原始差值**吸附到最近整分钟**。Linux 不受影响。
- CI `cli_behavior` Linux 失败 → 断言过期：官方在「全部成员被跳过」时也是 **exit
  10**，不是 0。
- 测试共享状态 → `rarfiles.lst` 从二进制旁改为**二进制私有副本**（跨进程共享位置
  在并发下 31/60 失败）；`name_policy` 单测不再改进程 CWD（`collect` 加显式
  `base`）。

**工程**

- 许可与 SPDX（2026-09-19 定）：顶层 `license` 字段**只声明本项目自有贡献**
  （BSD-2-Clause）；第三方移植不折进该字段，由 `NOTICE` +
  [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md) 逐文件记录。rars
  移植全部为 **MIT OR Apache-2.0**（上游从未发布 WTFPL：crates.io
  所有版本与移植依据的 `c08a17b` 都是该表达式；2026-07-13 短暂存在的 COPYING 非
  WTFPL 正文，作者已于 2026-09-06 统一为 Apache-2.0），`LICENSES/WTFPL.txt`
  已删。
- 删除 `rar_rs::archive::RarArchive` 兼容路径：`pub mod archive` 改
  `pub(crate)`，角色门面经 crate 根重导出。
- CI：rustdoc `-D warnings`；wasm 目标纳入 lint（`HostAttributes` /
  `resolve_redirect_target` / `snap_to_minute` 等按使用点用 `any(unix, windows)`
  精确门控）；`cargo deny` 依赖门禁（`deny.toml`）；四个特性组合 clippy 全绿。
- 写侧不可压缩预检 → 直接
  STORE（`whole_member_is_incompressible`）。老编码器要先 建 O(input) 的 token
  向量，随机数据此前会白分配上百 MiB。
- CLI 退出码对齐 WinRAR（2026-09-22 实测 7.23）：缺归档现在 exit **10**
  （`error::open_error` 把 I/O `NotFound` 与真 I/O 错误分开），未知开关与未知
  命令 exit **7**（`error::parse_args` 覆盖 clap 默认的 2；`rar`/`unrar`
  未知命令统一），无参数仍 exit 0。契约由
  `cli_bad_command_lines_match_winrar_exit_codes` 与两个缺归档断言钉住。
- 交互式覆盖询问（2026-09-22）：`ExtractOptions::prompt_overwrite` 配
  `ArchiveReader::set_overwrite_prompt`（回调挂在 `ReadState`，同 `motw`
  先例）； TTY 且未给 `-y`/`-o±`/`-or`/`-f`/`-u` 时 CLI 注入 WinRAR 式
  `Y/N/A/R/Q` 询问 （`output::prompt_overwrite`；库不读 stdin），非 TTY
  保持跳过，且询问时强制 串行抽取。契约由 `overwrite_prompt.rs` 与
  `cli_interactive_overwrite_prompt` 钉住。**静默语义按二进制区分** （2026-09-22
  官方 6.23/7.23 实测）：`Rar.exe x -idq` 对询问一律答
  Yes——同目录已存在目标时**不询问直接覆盖**（退出 0）；`UnRAR.exe x -idq`
  **仍会询问**（无 stdin 时报读错、目标不动）。故
  `ExtractRequest::quiet_answers_yes` 由 `rar` 置真、`unrar` 置假，后者保持
  非交互跳过（退出 10）。契约由 `cli_quiet_mode_overwrites_without_asking` 与
  `cli_extract_overwrite_defaults_to_skip` 钉住。
- RAR5 `-hp` 编辑补全（2026-09-22 官方 7.23 实测）：重写头走
  `write_block_header` 重加密（重命名给 `RewriteOp::CopyBlock` 加
  `rebuild_header` 标记；verbatim 拷贝仍写磁盘原字节），多卷重写首卷补发明文
  ENCR 头。**根因修正**：RAR5 `build_comment_block` 原把头 frame + data area
  一起返回，`-hp` 下把 data area 也加密了——现只返回头 frame、注释 payload
  单独写（与 RAR4 侧一致）； `get_comment` 改用 `read_main_header`
  重建加密状态（普通 open 不缓存它）。建前 `-z`+`-hp` 拒绝已移除，`-k`
  保留。契约由 `cli_header_encrypted_rar5_edits_work` 与
  `cli_header_encrypted_multivolume_delete_works` 钉住。
- `rar r` 无恢复记录时的重建（2026-09-22 官方 7.23 实测）：新增
  `reconstruct_archive_path`（放在 `archive` 层编排 reader/writer——`recovery`
  不得依赖 `archive`）：**解码并校验**每个成员、只保留通过的，写
  `rebuilt.<name>`（legacy 源重建为 RAR4、其余 RAR5；STORE；不保留时间/属性；
  一次驻留一个成员）。CLI `rar r` 在 `Unsupported`（无记录）时走它，打印官方的
  `Data recovery record not found` / `Reconstructing` / `Found  <name>` /
  `Done`。 **头损坏也可打捞**：严格扫描失败时改用「打捞扫描」（`ScanStrategy`
  之外的 crate 内部入口）——RAR5 块头 CRC
  失败即逐字节重同步（`resync_plain_block`， 带「vint 合理 +
  已知块类型」的廉价预筛，避免每字节读满 2 MiB），跳过坏块继续， 并在
  `ReadState.salvage_damaged` 记「曾丢成员」（这些成员根本没进目录，`dropped`
  说不出名字）。**RAR4 同样可打捞**（2026-09-22 对拍官方
  6.23/7.23）：`scan_volume` 加 `salvage`，坏块处用 `envelope::resync_block`
  逐字节重同步（廉价预筛：head type ∈ `0x72..=0x7b` 且 `head_size`
  装得下），并清掉待续分片；重同步丢下的成员 同样计入
  `salvage_damaged`。**退出码按容器对齐官方**（实测）：RAR5 头损坏 exit
  **3**，**legacy 头损坏 exit 0**；载荷损坏两边都 exit 0——官方对载荷损坏是**不
  校验、原样拷贝坏成员**（实测 `rebuilt` 里 f1 报 checksum error），我们改为丢坏
  成员并逐条打印，更安全，但退出码保持官方语义。**RAR 1.3/1.4**：官方打印同样的
  重建横幅后一行 `Cannot repair archive with old format`、**不产物、exit
  0**，我们 照抄（不再报 `Unsupported`）。**范围**：打捞扫描覆盖 RAR5 与
  RAR4，且仅**非 `-hp`**（加密流的块头无法廉价探测）；RAR 1.3/1.4 无打捞。契约由
  `reconstruct.rs` 五个测试与 CLI
  `cli_repair_without_a_recovery_record_reconstructs` /
  `cli_repair_salvages_past_a_corrupt_header` /
  `cli_repair_salvages_a_legacy_header` /
  `cli_repair_reports_rar13_as_unrepairable` 钉住。**legacy RR 定位加固**
  （2026-09-22）：`scan_protect_stream` 的走路靠头里的尺寸字段推进，一个坏
  `packed_size`/`head_size` 就把它带偏——此前直接报 `RAR4: truncated block`、连
  记录都找不到。现给容忍版 `scan_protect_tolerant`（**仅 repair
  入口**用；编辑路径 仍走严格版，坏归档在那里就该报错）接上
  `resync_block`：解析失败或尺寸越界时
  重同步到下一个合法块、继续找记录。**另外**：记录存在但够不到损坏时（小归档里
  RR 之前凑不出一个完整 512 字节扇区，`repairable_blocks = 0`）不再谎报
  `All OK`—— 改为提示 `The recovery record cannot repair this damage`，
  并按官方询问
  `Reconstruct archive structure ? [Y]es, [N]o`（`output::confirm`； 静默 `-idq`
  不询问、按 Yes 重建；`N`/读不到答案则只留报告）→ 归档走**退出码 3**（官方此路
  3，与无记录重建的 0 不同），对应官方的 `cannot recover data` →
  结构重建。契约由
  `cli_repair_reports_an_unreachable_legacy_record_and_rebuilds` /
  `cli_repair_asks_before_rebuilding_after_an_unusable_record` 钉住。
- `-htb` 语义对齐官方（2026-09-22 官方对拍）：BLAKE2sp 记录**取代** CRC32 字段
  （`MemberPlan::file_header` 在有 hash 时不再写 `crc32_val`，序列化器顺带清
  `FILE_FLAG_CRC32`）。此前是「CRC32 + BLAKE2sp 并存」，每成员比官方多 4 字节；
  现增量与官方一致（+31/member，实测 `lt` 不再显示 CRC32）。 `options.rs`
  那句「in addition … matching WinRAR」的错误注释一并修正。
- 列目录接 QO 快路径（2026-09-22）：CLI 列目录命令（`rar l/v/lt/lb/i`、unrar
  同） 改用 `ScanStrategy::PreferQuickOpen`（新增 `ops::open_reader_quick`），无
  QO 时 透明回退全扫；抽取/校验仍走全扫。此前 CLI 从不使用 QO，写 `-qo`
  等于白写。 契约由 `cli_listing_uses_the_quick_open_record` 钉住（`-qo`
  档真实文件头损坏仍能 列目录，无 `-qo` 的同档全扫失败）。

**流式与编码**

- legacy 大成员流式：≥ 64 MiB 走 spill 通道，内存有界。**v15/RAR13 现在压缩
  流式；v20 仍是 STORE 流式**（有意取舍）。流式发射器按代选密码：v29 带 salt、
  v20 补 16 字节 padding、v15 无 salt 无 padding。
- RAR20 窗口多块压缩流式：新增 `EncodeToken::EndOfBlock`（主表符号 269，**仅在
  用到时给码**，单块输出逐字节不变）；`ParseState` 跨窗口续传 `old_offsets` 与
  last-match——解码端这两个寄存器本来就不随块边界重置，所以窗口 token 无需改写，
  压缩率不降。
- RAR15/RAR13 增量编码器：这两种格式是**单一自适应流**（无块结束标记、无重发表
  语法），所以只能把 `Unpack15Encoder`
  改成跨块续传状态。产物与整成员编码**逐字节 相同**（用「整块 vs
  分块」对拍锁定）。
- RAR13 大成员 STORE 流式：第一遍算全成员滚动校验，第二遍分块拷贝；`-p` 用同一条
  流密码逐字节续加密（**分卷片段延续同一流**，不是每片重置）。
- RAR4 solid 链内过滤器：按「先测量后提交」——plain 与各 filter 候选都不提交，
  最小者再与续链 PPMd 试验竞争，胜者才移动链状态。过滤成员仍是普通链环（读者窗口
  持有的就是变换后字节）。
- RAR4 solid 链 PPMd 续模型：**「上一个已发射成员是 PPMd」⇒ 发 0x87 续模型**。
  注意官方 6.23 的 RAR4 写入器根本不产 PPMd，所以 RAR4 PPMd 链只有解码侧参考。
- `-mcde+` 按块过滤器：按 64 KiB 块**二选一**，记录**不相交**（重叠记录会被官方
  UnRAR 拒）；缓冲与流式两条路径都要如此。

## 现状

- **RAR5 / RAR7**：创建与读取全功能对齐 WinRAR 7.23——压缩（m0–m5、DP 最优
  解析）、`-hp` 头加密、分卷、solid、内联恢复记录、`.rev` 恢复卷、quick-open、
  NTFS ADS、三时间戳、owner、`-mt` 多线程、长距离匹配、v70 大字典。
- **老容器族读取（RAR 1.3–4.x）**：三代解码器（RAR29/20/15）+ PPMd + 五大标准 VM
  过滤器 + 通用 RARVM 解释器，solid 链、分卷、`-hp`、各代数据解密。
- **RAR4 创建全能力**：LZSS m1–m5 + PPMd + 六大标准 VM 过滤器 + `-hp` + 多卷 +
  solid（链内亦应用 VM 过滤器与 PPMd 模型延续）+ 并行 batch + 单大成员块级 MT
  （字节同等）+ NEWSUB 恢复记录。能力表见
  [`docs/rar4-creation-spec.md`](docs/rar4-creation-spec.md)。
- **RAR 1.3 / 1.4 / 1.5 / 2.x 创建**：`-ma13` / `-ma14` / `-ma15` / `-ma2`，含
  solid、`-p` / `-hp`、旧命名分卷。
- **RAR4 编辑全补**（ADR 0005）：头/块级操作 + 非 solid 块拷贝 + solid 整档
  repack；`-hp` 与分卷（`rn`/`ch`/`k`/注释）均已支持。
- **命令面**：官方 `rar` 全部命令（含 `rv` 补恢复卷、`lb/lt/vb/vt` 列表变体）。
- **工程**：workspace 三 crate；CI 只做 fmt / cargo check / clippy `-D warnings`
  / cargo deny / rustdoc /**测试不在 CI 里跑**（本地闸门，见
  [`docs/testing.md`](docs/testing.md)）；七目标 fuzz；取消钩子；QO
  快路径；流式修复； 零填充分卷。

## 一致拒绝（别"修"）

- **分卷 + 内联恢复记录（`-rr`）**：WinRAR 分卷只能用 `.rev`；官方 6.23 自己的
  `rar r` 在带内联 RR 的分卷集上挂死（产出 0 字节 fixed），所以分卷 RR 编辑也
  保持拒绝。
- **分卷 append / 分卷删除**：官方 `rar` 同样拒绝（"Cannot modify volume"）。
  分卷的 `rn` / `ch`、`k` 与归档注释**不是**拒绝项：官方支持，我们也支持（逐卷
  重写 / 注释插在首卷主头后）。
- **把容器族 / recovery 做成编译期 feature**（2026-09 审查后否决）：不把 legacy
  族（`codec/legacy` + `format/{rar13,rar4}`，21,000 行 ≈ 29%）或
  recovery（`.rev`/RR，7,037 行 ≈ 10%）做成可选 feature。它们是产品范围本身—— 对
  RAR 1.3–4.x 的读写、`r`/`rv`/`rc` 与依赖它们的 `-hp`/solid 路径都是对外
  承诺，默认必须开启，因此 feature 化对本仓库的 CI、本地构建与发布产物**零
  收益**；代价是 50–90 处新 `#[cfg]`（现有 115 处）、CI clippy 矩阵翻倍、以及
  此后每次改 legacy/recovery 都要照顾门控。真需要“只读 RAR5”的消费者应该用
  裁剪的 fork，而不是往主干加开关。（同类先例：ADR 0007 删掉 `raw` feature，
  因为那个开关的成本大于收益。）

## 已知小差异（记录，互操作无碍）

- **`rar r` 没修动时不写 `fixed.<name>` 拷贝**：恢复记录够不到损坏时，官方仍写出
  `fixed.<name>`——实测它与**损坏输入逐字节相同**（没修成功也照拷一份）；我们按
  「不能进 行的修复不留产物」的既有契约**不写**，只提示 + （询问后）重建
  `rebuilt.<name>`。退出码 3 与询问行为已对齐。
- **无控制台且非静默时的覆盖询问**：官方先打印询问、读 stdin 失败后
  `Program aborted`（Rar）或 `Read error in the file stdin`（UnRAR）并终止整轮；
  我们直接按跳过处理（不询问、继续、全跳则退出 10）。同一 TTY
  场景我们与官方一致。
- **RAR4 成员注释（`cf`）**：v29+ 写侧在成员数据后发射**独立** `COMM_HEAD`
  （0x75）块，官方 6.23/7.23 `t`/`x` 均 `All OK` 且解出字节一致；pre-RAR3
  （unp_ver<29）保持嵌套布局——官方对 1.5/2.x 注释的校验本身不可作基准，且官方无
  `cf` 不能生成对照。多卷成员注释继续拒绝。
- solid 且无 `rarfiles.lst` 时：WinRAR 按扩展名/名字启发式排序，我们按参数顺序。
- 目录条目名带尾斜杠。
- **`lb` 分片成员**：官方 `Rar.exe lb` 对跨卷成员不打印该成员名，官方
  `UnRAR.exe lb` 与我们一致；我们随 UnRAR。
- **WinRAR 的 RAR4 修复对周期数据的缺陷**：恢复记录块落入其保护的最后部分扇区且
  成员数据短周期重复时，WinRAR 自己的 `rar r` 会修坏 RR 尾部（6.23 与 7.23
  一致）。 我们把部分尾扇区排除出 parity
  组、只重建完整扇区，故能正确修复同样损坏。**这是 WinRAR
  侧缺陷，不追平**；互操作测试因此用伪随机成员数据。
- **RAR5 元数据：平台风格 + 两条 WinRAR 惯例（2026-09-22 字段级实测 7.23 的
  Windows 与 Linux 两个官方构建）**。**平台风格**：WinRAR 按宿主平台写元数据 ——
  Linux 上 `host_os`=1、Unix `st_mode` 属性（0o100644）、FILE_TIME 记录 flags
  0x13， **与我们完全一致（记录字节逐字节相同）**；Windows 上则是
  `host_os`=0、DOS 属性 （0x20，1 字节）、FILE_TIME 记 Windows FILETIME（flags
  0x02）。我们**一律写 Unix 风格**，因此只在 Windows 上比官方多 2
  字节/成员（属性宽度），Linux 无此差。
  **两条与平台无关的惯例我们没复刻**：①官方**总是**写主头 locator 记录（无 QO/RR
  时 flags=QO、offset=0 占位；其偏移是变长 vint，我们为可回填用定长 5 字节）⇒
  我们固定部分小 **7 字节**；②官方把 `data_size`/`unpacked_size`/`comp_info`
  填到**至少 2 字节**（11 写 `8b 00`，我们写 `0b`）⇒ 我们 −3
  字节/成员。**另外**：官方只在「秒精度」时置 `FILE_FLAG_TIME_UNIX` 并省略
  FILE_TIME 记录，有时间 ns 时反过来（清 TIME_UNIX、写 flags 0x13
  的记录）；我们**两者都写** ⇒ +4 字节/成员（ns=0 时还多一条 11 字节的
  记录）。**净差**（同输入、11 字节载荷、含 ns）：1 成员 我们 77 / 官方 Windows
  81 / 官方 Linux 83，3 成员 183 / 181 / 187 ⇒ **每成员 +1（vs Linux）/ +3（vs
  Windows）， 固定部分 −7**。**只影响字节外观**：双向读写一致，interop
  套件全绿；对齐需按平台写 元数据并复刻两条惯例，会让同一输入在 Windows/Linux
  产物不同，属独立决策，未做。

## 归属（谁记录什么，别再重新论证）

- **压缩与性能**的契约、已否决方向与剩余差距 → 本文「性能」段：seq 与最优解析的
  字节契约不动；`-mt` 低步数搜索是**接受的取舍**；按日期的实测过程在 git 历史。
- **模块、分层与设计不变量**（有界内存/spill、安全提取、solid 与 MT、多卷
  journaled 提交）→ [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
- **术语** → [`CONTEXT.md`](CONTEXT.md)；**字节格式** →
  [`docs/FORMAT_RAR5_RAR7.html`](docs/FORMAT_RAR5_RAR7.html)；**架构决策** →
  [`docs/adr/`](docs/adr/)；**测试与 CI** →
  [`docs/testing.md`](docs/testing.md)。

## 备注（改代码前必读）

- 卷大小必须精确：新增块类型（如 QO）记得同步配额记账。
- 加密块 padding 是 **zero-fill 不是 PKCS7**——7-Zip 会校验 padding 区全零。
- **Markdown 由 `dprint` 格式化、正文 80 列**：改完跑 `npx dprint@0.50.2 fmt`
  （配置 `dprint.json`，约定见 [`docs/README.md`](docs/README.md)）。
