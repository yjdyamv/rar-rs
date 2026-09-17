# rar-rs 计划

> 最后核对：2026-09-17 @ `3c10f14`；实现细节以源码为准。

本文件只留**结论**与**下一步**。历次审计、逐批修复与加固的过程记录在 git 历史
（旧版详单：`git show d9201cf:PLAN.md`）；本文件不再维护 CHANGELOG。

相关文档：术语见 [`CONTEXT.md`](CONTEXT.md)，模块图见
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)，格式细节见
[`docs/FORMAT_RAR5_RAR7.html`](docs/FORMAT_RAR5_RAR7.html)，性能议题见
[`docs/issues/compression-perf/map.md`](docs/issues/compression-perf/map.md)。

## 下一步

优先级自上而下。完成一项勾掉一项：结论并入本文，过程留在 git 历史。

### P0 · 正确性

- [x] **legacy RAR4 `-hp` 错口令判定不确定**（已修 2026-09-16）：错口令时
      `read_encrypted_block` 只在解密后的垃圾 `head_size` 越界/过小时报
      `WrongPassword`；若垃圾长度恰好合理（落在界内），会继续解析并在头 CRC 上报
      `Crc`（实测 200 次复现 1
      次：`Crc { expected: 49511, actual: 64398,
      context: "RAR4 block type 0xca header" }`）。RAR4
      没有 MAC/口令校验值，错口令与损坏头本就不可区分：现把加密块的垃圾
      `head_size`、解析/CRC 失败统一映射为 `WrongPassword`（CLI → exit
      11），新增两个确定性单测；原 flaky 测试连续 500 次 0 失败。过程见 git
      历史。
- [x] **Linux 构建断裂**（已修
      2026-09-16）：`format/rar5/headers/serialize.rs::
      build_stream_block`、它在
      `format/rar5/headers/mod.rs` 的重导出、以及 `format/rar5/write/stream.rs`
      的 `vint` 导入都被 `#[cfg(windows)]` 门控；但
      跨平台的多卷重写（`transaction/multivolume.rs` 重发 "STM" 记录）与
      `write_stream_record` 会在所有平台调它们，导致
      `cargo check -p rar-rs --target x86_64-unknown-linux-gnu` 报 3 个错——**已
      推送的 `main` 在 Linux 上编不过（CI lint job 会红）**。修法：去掉这三处
      `#[cfg(windows)]`（`OS_WINDOWS` 只是格式常量，不是编译门）。已验证 Linux
      check / clippy `-D warnings` / rustdoc `-D warnings` 与 Windows
      全量测试均过。
- [x] **测试共享状态审计 + 修复**（2026-09-17）：
  - `cli_behavior` 的 `rarfiles.lst`：`cli_rarfiles_lst_orders_solid_members`
    把列表写到二进制旁（`target/debug/rarfiles.lst`）——**跨进程**共享位置，本地
    4 路并发实测 31/60 失败（且会把 `cli_se_preserves_input_order`
    拉挂）。已改为**二进制私有副本**（拷贝到 temp
    dir、列表放副本旁），并删掉那把只是**进程内**的
    `rarfiles_lst_lock`。**注意**：实测 `cargo test` 是**顺序**跑各测试二进制的
    （`Running`→`test result`→`Running`），所以这条**不是** CI `cli_behavior`
    失败的根因；它是 nextest / 并发 / 陈旧缓存下的隐患，已消除。顺带修了
    `input.rs` 一行被写坏的文档注释。
  - `name_policy` 单测用 `std::env::set_current_dir`（进程级
    CWD，与同二进制内并行的其他单测天然冲突）：已给 `collect` 加显式
    `base: Option<&Path>`（生产传 `None` = 仍按 CWD；测试传 temp
    dir），测试不再改 CWD。
  - 复查其余无同类问题：`winrar_interop` / `rar-rs` 测试都用 per-test
    `tempdir`； napi 的 `set_var` 测试由 `test_lock` 串行化；库的运行时 env 开关
    （`RAR_RS_FAR_BAND` 等）测试不设置。
- [x] **CI `cli_behavior` 的 Linux 失败**（已定位并修复 2026-09-17）：失败用例是
      `switches::cli_skip_links_does_not_report_skipped_links`（`#[cfg(unix)]`）。它
      在 `93ae9ce` 写成时断言“全部跳过 → exit 0”；`c2c43d4`
      加“全部选中成员都被跳过 → exit 10（No files to
      extract）”后它就陈旧了，而因为它只在 Linux 跑、Linux
      又被编译断裂挡住，一直没暴露。用**本机官方 UnRAR 7.23 对照**确认：
      “已存在文件 + `-ol-` 链接 + `-o-` 跳过”时官方也是 **exit 10**（且打印 "No
      files to extract"）——所以行为正确、测试期望过期；已把断言改成 exit 10，
      保留「链接不得出现在 Skipping 报告里」。定位手段正是本轮加的 CI「失败用例
      → check-run annotation」，它一次就给出了用例名。
- [x] **RAR4 batch 与 sequential 偶发不一致**（已修 2026-09-17）：**不是竞态**，
      是 `local_offset_secs()`（库 `format/shared/legacy_time.rs` + CLI
      `time.rs`）把 “本地时间”与 UTC **分两次采样**：Windows 的 `GetLocalTime`
      只按 ~15.6 ms 系统 tick 前进，在高精度 `SystemTime::now()`
      跨秒后仍可能停留在前一秒，于是 `local - utc` 偶发差 1 秒（探针实测：3 秒内
      37,717 次返回 28799 = 28800-1，约 1.9%）。RAR4 DOS 时间只有 2
      秒分辨率、由本地秒奇偶决定 ext-time 的 `ADD_SECOND` 位，所以 ±1
      秒会翻转该位 → 同一文件在不同时刻编码出不同字节（长度相同），seq 与 batch
      于是偶发不等。定位手段：给偶发失败时 dump 两个归档 → 解码发现 DOS
      时间相同、仅 ext-time 旗标字串节 0xF000 vs 0xB000。
      **修法**：把原始差值吸附到最近整分钟（真实时区都是整分钟）；两处都改，新增
      `snap_to_minute` 确定性单测 + 真实时钟对齐测试。修后 4 路并发 160 次 0
      失败（修前约 1/20）。**Linux 不受影响**：`localtime_r(&utc)` 用的是同一个
      UTC 采样，本身精确。

### P0 · 发布收口（硬门槛）

- [ ] **许可与 SPDX**：确定仓库级 SPDX 表达式。待裁定的两处：① rars workspace
      metadata（MIT OR Apache-2.0）与后来 COPYING（WTFPL）的冲突，即解码侧 WTFPL
      / 编码侧 MIT OR Apache-2.0 的来源；② `recovery/legacy.rs` 声明了 rars
      移植但缺许可行。逐文件出处清单见
      [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md)。
- [ ] **打包与发布顺序**：`rar-rs`（0.1.2）已能 `cargo package` 并通过校验；
      `rar-cli` 依赖 workspace 内的 `rar-rs`，需先发布 `rar-rs`。三个 crate 的
      `readme` / `keywords` / `documentation` / `categories` 元数据已补齐
      （2026-09-16），但 `rar-cli` / `rar-rs-napi` 仍因 `rar-rs`
      未发布而无法解析依赖。
- [x] **删除 `rar_rs::archive::RarArchive` 兼容路径**（2026-09-17）：`lib.rs` 的
      `pub mod archive` 改为 `pub(crate)`，角色门面经 crate 根重导出；
      `rar_rs::archive::RarArchive` 不再可达。29 处集成测试改用门面
      （`comment()` / `verify()` / `copy_entry_to`）；仅测试使用的 `RarArchive`
      方法（`read_with_options` / `test` / `set_password`）标
      `#[cfg(test)]`，真正无用的 `read_to_writer(_with_options)` /
      `extract_with_options` 删除；公开文档不再链接私有类型。破坏性变更，但
      crate 未发布故无下游影响。验证：全量测试 54 targets、 clippy
      `-D warnings`、`rustdoc -D warnings` 全绿。

### P1 · 工程加固（低成本）

- [x] **CI 开启 rustdoc `-D warnings`**（2026-09-16）：`CI.yml` 的 doc 步骤加
      `RUSTDOCFLAGS: -D warnings` 并更新过期注释；Windows 与 Linux
      两个目标均实测零警告。

### P1 · 功能缺口（按需，不阻塞发布）

- [x] **(A) 不可压缩预检 → STORE**（2026-09-17）：新增
      `format/shared/write_ops.rs::whole_member_is_incompressible`（复用 RAR5 的
      `sample_is_incompressible` 探针 + 远距重复逃生门），接进 RAR4
      缓冲路径、batch 准备与 RAR13 写侧。老编码器要先建 **O(input) 的 token
      向量**，随机数据（媒体/ 已压缩文件）此前会白白分配上百 MiB；现在直接
      STORE。输出与原来的「编码后按尺寸回退 STORE」逐字节一致。
- [x] **(B) legacy 大成员 STORE 流式**（2026-09-17）：`add_file_rar4` 对 ≥
      `STREAM_COMPRESS_THRESHOLD`（64 MiB）的 **v15/v20** 成员走既有 spill
      流式通道但 **强制 STORE**（RAR29
      仍走压缩流式），从而有界内存；代价是大成员不压缩（文档化
      取舍）。验证：本机官方 UnRAR 7.23 对 `-ma15`/`-ma2` × {66 MiB 可压、8 MiB
      随机} 的 `t`/`x` 全部字节一致；`-ma4` 大成员仍压缩（archive 55 KB / raw 66
      MiB）。**回归修复（2026-09-17）**：流式发射器当时写死 RAR30（v29）密码，
      于是加密的 v15/v20 大成员一度用错密码流（官方 UnRAR 报 CRC 错、exit
      3）；先以 `!password_encrypted` 守卫留在缓冲路径并加回归测试，随后由下面
      (C) Stage 2 的**分代流式密码**彻底取代该守卫（现在加密大成员也有界内存）。
- [x] **(C) Stage 1 · RAR20 窗口多块压缩流式**（2026-09-17，已完成）：
      `rar20_encoder.rs` 新增 `EncodeToken::EndOfBlock`（主表符号 269，**仅在
      用到时给码**，单块输出逐字节不变）、`BitSink` trait + `StreamingBitSink`
      （只驻留半个字节 + 64 KiB 刷缓冲）、`write_lz_block<S: BitSink>`（从
      `encode_member_with_tables` 拆出）与 `ParseState`（`old_offsets` +
      last-match **跨窗口续传**，因为解码端这两个寄存器本来就不随块边界重置，
      所以窗口 token
      不需要改写，压缩率不降）。`encode_member_windowed_streaming` 按 64 KiB
      窗口写块：每块 keep_tables=0 + 平坦表，非末块追加 269；块位连续，
      因此共用一个 sink。`add_rar4_file_streaming` 对 Rar20（非 solid）调用它；
      `add_file_rar4` 的 `stream_level` 用真实 level。大成员先跑
      `sample_is_incompressible_file` 探针，不可压就直接 STORE（66 MiB 随机：76
      s → 0.65 s，避免白跑一遍再回退）。验证：本机官方 UnRAR 7.23 对
      `-ma2 -m1..-m5`（66 MiB 可压，350–356 KB）、`-v100k` 4 卷、66 MiB
      随机（STORE 兜底）的 `t`/`x` 全部字节一致；库测试
      `create_rar4_legacy_large_members_stream_compressed`（单卷 + 100 KiB
      分卷，断言 method ≠ 0）。
- [x] **(C) Stage 2 · legacy 大成员流式加密**（2026-09-17，已完成）：`cbc.rs`
      泛化为 `Rar4RangeEmitter` trait +
      `Rar4BlockRangeEmitter<C: Rar4BlockCipher>`
      （`Rar30RangeEmitter`/`Rar20RangeEmitter` 别名，块密码按 16 字节对齐 +
      carry）与 `Rar15RangeEmitter`（流密码靠新增的 `Rar15Cipher::skip`
      前进）；`add_rar4_file_streaming` 按代选密码：v29 带 salt、v20 补 16 字节
      padding、v15 无 salt 无 padding。`add_file_rar4` 去掉
      `!password_encrypted` 守卫（只有无密码实现的版本才留缓冲路径报错）。
      验证：本机官方 UnRAR 7.23 对 `-ma2 -p -v100k`（压缩+加密+4 卷）、
      `-ma15 -p -v100k`（676 卷，STORE+加密）、`-ma4 -p` 的 `t`/`x`
      全部字节一致；库测试
      `create_rar4_legacy_large_encrypted_members_roundtrip` （v15/v20 × 单卷/8
      MiB 分卷，v20 断言 method ≠ 0）。
- [ ] **(C) Stage 3 · RAR15/RAR13 压缩流式（原「窗口化」方案不可行，待定）**：
      RAR 1.5 与 RAR13（后者复用一个 `Unpack15` 编码器）是**单一自适应流**：
      解码端 `init_huff` 只在成员/链开始时建初表，之后每符号经 `corr_huff`
      演化，`get_flags_buf` 读的是同一个自适应集合；**格式里既无块结束标记、
      也无重发表语法**，所以 Stage 1 那种「每窗口重发一张表」在这里**不存在**
      （与「PPMd 块级 MT」同类的结构性限制）。可行替代是把 `Unpack15Encoder`
      改成**增量（跨块续传状态）编码器**：
      状态已经可整体克隆（`clone_for_planning()` 列全了自适应字段），只差 (a)
      `long_lz_buckets` 从「先建全量索引」改成边插边查、 (b)
      `pos + 1 < input.len()` 这类向前看改成带 carry（≤ 最大匹配长度）、 (c)
      跨块保留 flag 组 / `straddle` / stmode 局部量。产物与整块编码
      **逐字节相同**，用「整块 vs 分块」对拍即可直接验证，无互操作风险。
      收益：>64 MiB 的 v15/RAR13 成员从 STORE 变成压缩（现已是有界内存）。
- [x] **RAR13 大成员 STORE 流式**（已实现 2026-09-17）：`add_file_rar13` 对 ≥
      `STREAM_COMPRESS_THRESHOLD` 的成员改走新的
      `add_rar13_file_streaming_store`：
      第一遍顺序扫描算全成员滚动校验（文件头需要它），第二遍按卷（或 1
      MiB）分块拷贝， `-p` 用同一条 RAR13
      流密码**逐个字节续加密**（分卷片段延续同一流）；分卷片段头
      维持原语义（中间片存累计 packed
      校验、末片存整成员校验），分卷边界与头部与缓冲
      路径一致。验证：库测试（单卷 66 MiB、加密 9 卷）、以及**本机官方 UnRAR
      7.23** 对 `-ma13` 单卷、`-ma14 -v8m` 9 卷、`-ma13 -ppw -v8m` 9 卷的
      `t`/`x` 全部字节一致；另加了 `#[ignore]` 的 winrar_interop 用例。顺带修了
      `format/rar13/write.rs`
      模块注释里「加密成员每片重置密码流」的陈旧说法（代码是整段加密后切片、连续流）。
- [ ] **RAR4 solid 链内过滤器**：写侧尚未在 solid 链内应用 VM 过滤器（窗口=
      变换后字节语义，LZ 收益有限）。
- [ ] **RAR4 solid 归档 MT**：legacy solid 链保持串行；成员级并行需跨成员共享
      窗口，属结构性代价（RAR5 的 chunk 级 MT 已兑现）。
- **有意不做（设计决定，2026-09-17）**：
  - **老编码器块级 MT（v15/v20 单个大成员）**：Stage 1 之后 v20 已具备「多窗口
    并行解析 + 顺序写位流」的骨架，但需要每窗口独立的 match finder
    状态与确定性的窗口边界才能保证字节一致，收益仅「老格式大成员的创建
    速度」（官方 7.23 已移除 `-ma4`，老格式创建只是我们的扩展），故仍不做； v15
    另受自适应表限制。
  - **PPMd 块级
    MT**：单自适应模型，切块会改变输出（结构上不可行）；成员级并行已有。
  - **RAR13 非 solid 成员级 batch**：成本低但价值最低（DOS
    时代格式）；确需再做。
- [x] **`-mcde+` 按块过滤器选择**（已实现 2026-09-17）：官方 `-mcde+` 按 64 KiB
      块发**不相交**的 filter 记录（重叠记录会被官方 UnRAR
      拒）。现在两条路径都按 64 KiB 块二选一（各候选块内试编，取小；持平取
      delta）：缓冲走 `lzss_huff::forced_combined_specs`，流式走
      `forced_combined_stream_window`（逐块变换 + 绝对偏移记录）。RAR4
      本来就在候选里二选一，不变。验证：库单测
      （记录铺满成员、不重叠、重放与变换一致）、CLI `-mcde+` 往返、**本机官方
      UnRAR 7.23 对本实现产物 `t`/`x` 字节一致**。流式路径的 >64 MiB
      跨官方互操作标了 `#[ignore]`（慢）。

### P2 · 性能（未关闭议题，已 park）

- [ ] **issue 09 — DLL 单线程解析速度**：真实 DLL 上 m3 `-mt1` 落后 WinRAR 约
      5.9x，瓶颈是 BT4 下降步数（结构锁定：HASH_BITS、dict-log、提交阈值、近/远
      带宽四个旋钮已验证弹回）。
- [ ] **issue 04 — MT 随机数据窗口级不可压缩跳过**：已 park（低价值）——成员级
      STORE 兜底已让随机数据领先 WinRAR 10–80x；窗口级跳过有把边界成员从压缩翻
      成 STORE 的比率风险，需先有边界语料量化。

### 暂缓（等决策，不自行推进）

- **退出码差异**（用户要求暂缓）：
  - 缺失归档：我们 exit 2，官方 exit 10。
  - 未知开关：我们经 clap 解析失败 exit 2，官方 exit 7（未单独映射）。
- **STORE 成员竞态**：单遍 STORE 先 `hash_file` 再重读同一路径，同尺寸改写真可能
  写出旧 CRC/BLAKE2；回填头需要 patching（`-hp` 还要重加密），已接受。
- **交互式覆盖询问**：默认按官方非交互语义跳过已存在（`-o+`/`-y` 覆盖，`-or`
  自动改名）；交互询问未实现。

## 现状

- **RAR5 / RAR7**：创建与读取全功能对齐 WinRAR 7.23——压缩（m0–m5、DP 最优解
  析）、`-hp` 头加密、分卷、solid、内联恢复记录、`.rev` 恢复卷、quick-open、
  NTFS ADS、三时间戳、owner、`-mt` 多线程、长距离匹配、v70 大字典。
- **老容器族读取（RAR 1.3–4.x）**：三代解码器（RAR29/20/15）+ PPMd + 五大标准 VM
  过滤器 + 通用 RARVM 解释器，solid 链、分卷、`-hp`、各代数据解密。
- **RAR4 创建全能力**：LZSS m1–m5 + PPMd + 六大标准 VM 过滤器 + `-hp` + 多卷 +
  solid（含 PPMd 模型延续）+ 并行 batch + 单大成员块级 MT（字节同等）+ NEWSUB
  恢复记录。
- **RAR 1.3 / 1.4 / 1.5 / 2.x 创建**：`-ma13` / `-ma14` / `-ma15` / `-ma2`，含
  solid、`-p` / `-hp`、旧命名分卷。
- **RAR4 编辑全补**（ADR 0005）：头/块级操作 + 非 solid 块拷贝 + solid 整档
  repack；`-hp` 与分卷（`rn`/`ch`/`k`/注释）均已支持。
- **命令面**：官方 `rar` 全部命令（含 `rv` 补恢复卷、`lb/lt/vb/vt` 列表变体）。
- **工程**：workspace 三 crate（`rar-rs` / `rar-cli` / `rar-rs-napi`）；CI
  fmt/clippy `-D warnings`/测试/fuzz smoke + 定时官方互操作 job；七目标
  fuzz；取消钩子；QO 快路径；流式修复；零填充分卷。

## 一致拒绝（别"修"）

- 分卷 + 内联恢复记录（`-rr`）：WinRAR 分卷只能用 `.rev`；官方 6.23 自己的
  `rar r` 在带内联 RR 的分卷集上挂死（产出 0 字节 fixed），因此分卷 RR
  编辑也保持拒绝。
- 分卷 append、分卷删除：官方 `rar` 同样拒绝（"Cannot modify volume"）。分卷的
  `rn` / `ch`、`k` 与归档注释**不是**拒绝项：官方支持，我们也已支持（逐卷重写 /
  注释插在首卷主头后）。
- **强制 delta + 强制
  x86（`-mcd+ -mce+`）**：**已实现**（2026-09-17，见「下一步」 P1）——按 64 KiB
  块二选一、记录不相交，官方 UnRAR 可读；不再是拒绝项。

## 已知小差异（记录，互操作无碍）

- **RAR4 成员注释（`cf`）**：v29+ 写侧在成员数据后发射**独立** `COMM_HEAD`
  （0x75）块（外层不带 `FHD_COMMENT`、外层 CRC 覆盖固定+名；0x75 自身 CRC 按
  unrar 的 11 字节体覆盖），官方 UnRAR 6.23/7.23 `t`/`x` 均 `All OK` 且解出字节
  一致，`-hp` 下只加密 35 B 头；pre-RAR3（unp_ver<29）保持嵌套布局——官方对
  1.5/2.x 注释的校验本身不可作基准，且官方无 `cf` 不能生成对照。多卷成员注释继续
  拒绝。
- solid 且无 `rarfiles.lst` 时：WinRAR 按扩展名/名字启发式排序，我们按参数顺序。
- 目录条目名带尾斜杠。
- **`lb` 分片成员**：官方 `Rar.exe lb` 对跨卷成员不打印该成员名，官方
  `UnRAR.exe lb` 与我们一致；我们随 UnRAR，不追 `Rar.exe` 的省略行为。
- **WinRAR 的 RAR4 修复对周期数据的缺陷**：当恢复记录块本身落入其保护的最后部分
  扇区且成员数据是短周期重复（如 64 B pattern）时，WinRAR 自己的 `rar r` 会把 RR
  尾部修坏（产物 `Unexpected end of archive`），6.23 与 7.23
  一致。我们的写侧把部分尾扇区排除出 parity
  组、读侧只重建完整扇区，故能正确修复同样损坏。这是 WinRAR
  侧缺陷，不追平；互操作测试因此用伪随机成员数据。

## 归属（谁记录什么，别再重新论证）

- **压缩与性能**的结论、负例与基线 →
  [`docs/issues/compression-perf/map.md`](docs/issues/compression-perf/map.md)：
  seq 与最优解析的字节契约不动；`-mt` 低步数搜索是**接受的取舍**；BT4
  字节级流水、 FAR_BAND、两制近存等方向已实测为负例。
- **模块、分层与设计不变量**（有界内存/spill、安全提取、solid 与 MT、多卷
  journaled 提交）→ [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
- **术语** → [`CONTEXT.md`](CONTEXT.md)；**字节格式** →
  [`docs/FORMAT_RAR5_RAR7.html`](docs/FORMAT_RAR5_RAR7.html)；**架构决策** →
  [`docs/adr/`](docs/adr/)；**测试与 CI** →
  [`docs/testing.md`](docs/testing.md)。

## 备注（改代码前必读）

- 卷大小必须精确：新增块类型（如 QO）记得同步配额记账。
- 加密块 padding 是 **zero-fill 不是 PKCS7**——7-Zip 会校验 padding 区全零。
- **Markdown 文档由 `dprint` 格式化，正文 80 列**；改完文档跑
  `npx dprint@0.50.2 fmt`（配置见 `dprint.json`，约定见
  [`docs/README.md`](docs/README.md)）。
