# rar-rs 计划

> 最后核对：2026-09-16 @ `c2c43d4`；实现细节以源码为准。

本文件只留**结论**与**下一步**。历次审计、逐批修复与加固的过程记录在 git 历史
（旧版详单：`git show d9201cf:PLAN.md`）；本文件不再维护 CHANGELOG。

相关文档：术语见 [`CONTEXT.md`](CONTEXT.md)，模块图见
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)，格式细节见
[`docs/FORMAT_RAR5_RAR7.html`](docs/FORMAT_RAR5_RAR7.html)，性能议题见
[`docs/issues/compression-perf/map.md`](docs/issues/compression-perf/map.md)。

## 下一步

优先级自上而下。完成一项勾掉一项：结论并入本文，过程留在 git 历史。

### P0 · 发布收口（唯一硬门槛）

- [ ] **许可与 SPDX**：确定仓库级 SPDX 表达式。待裁定的两处：① rars workspace
      metadata（MIT OR Apache-2.0）与后来 COPYING（WTFPL）的冲突，即解码侧 WTFPL
      / 编码侧 MIT OR Apache-2.0 的来源；② `recovery/legacy.rs` 声明了 rars
      移植但缺许可行。逐文件出处清单见
      [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md)。
- [ ] **打包与发布顺序**：`rar-rs`（0.1.2）已能 `cargo package` 并通过校验；
      `rar-cli` 依赖 workspace 内的 `rar-rs`，需先发布 `rar-rs`。补齐三个 crate
      的 `readme` / `keywords` / `documentation` / `categories` 元数据。
- [ ] **下一个破坏性版本**：删除 `rar_rs::archive::RarArchive` 兼容路径，公开面
      只留 `ArchiveReader` / `ArchiveWriter` / `ArchiveEditor`（ADR 0006）。

### P1 · 功能缺口（按需，不阻塞发布）

- [ ] **RAR4 solid 链内过滤器**：写侧尚未在 solid 链内应用 VM 过滤器（窗口=
      变换后字节语义，LZ 收益有限）。
- [ ] **RAR4 solid 归档 MT**：legacy solid 链保持串行；成员级并行需跨成员共享
      窗口，属结构性代价（RAR5 的 chunk 级 MT 已兑现）。
- [ ] **`-mcde+` 按块过滤器选择**：官方 `-mcde+` 产出按 64 KiB 块*不相交*的过滤
      器序列，我们无法产等价的重叠记录，当前显式 `InvalidOption`；真对齐需实现
      按块过滤器选择（新立项，见「一致拒绝」）。

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
- **强制 delta + 强制 x86（`-mcd+ -mce+`）**：官方 `-mcde+` 产出按 64 KiB 块
  *不相交*的过滤器记录；重叠记录无法被官方 UnRAR 反转（实测两种记录顺序都被判
  checksum error），因此保持 `InvalidOption`。真对齐需实现按块过滤器选择。

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
