# rar-rs 计划

完成一项勾掉一项。本文件只留**结论**与**警告**；验证过程和工程细节见 git 历史（旧版详单：`git show d9201cf:PLAN.md`）。

## 现状

- RAR5 创建/读取全功能对齐 WinRAR 7.23：压缩、`-hp` 头加密、分卷、solid、内联恢复记录、`.rev` 恢复卷、quick-open、NTFS ADS、三时间戳、owner；另含 RAR7 (v70) 读写、`-mt` 多线程压缩、长距离匹配。
- 命令面：官方 rar 全部命令（含 `rv` 补恢复卷、`lb/lt/vb/vt` 列表变体）；开关矩阵见 `docs/SWITCH_MATRIX.md`（对照本机 RAR 7.23 实测）。
- 工程：fmt/clippy `-D warnings` 双门（本地）、五目标 fuzz（`fuzz/`：parse/crypto/recovery/write/rewrite）、取消钩子、QO 快路径 `open_quick`、流式修复 `repair_archive_path`、零填充分卷集支持。
- 架构：workspace `crates/rar`（库 crate `rar5`）+ `crates/rar-cli`（rar/unrar），按 rars 分层——词汇见 `CONTEXT.md`，迁移决策见 `docs/REFACTOR_MIRROR_RARS.md`，格式细节见 `docs/FORMAT_RAR5_RAR7.md`。

## 待办

- （无）

## 已取消 / 不做

- **RAR4 全部**（创建 `-ma4`、PPMd、加密解压）：定位 RAR5-only，遇 RAR4 明确报 unsupported（有测试锁定）
- unrar `s`（转 SFX）：官方 UnRAR 7.23 无此命令，非差距

## 已完成（要点）

- [x] 过滤器只实现 0–3（Delta/E8/E8E9/ARM），未知类型显式报错——类型 4–7 现实归档中不存在
- [x] 大字典内存防护：读侧上限改为可配置 `ExtractOptions::max_dict_size`（默认 4 GiB，`-mdx` 语义；`None` = 不限），RAR7 v70 >4 GiB 字典按上限拒绝
- [x] 字典：`-md` 全量语义（非法值报 Unknown option 与 WinRAR 一致）；默认 32 MiB；按 `min(-md, 2×floor_pow2(文件大小))` 裁剪；读侧上限 4 GiB（RAR5 格式上限）
- [x] RAR7 (v70) 读+写：>4 GiB 字典（5 位+1/32 非幂编码）、DCX=80 扩展距离表、u64 距离；裁剪落回 ≤4 GiB 自动降级 v50
- [x] 长距离匹配（`-mcl` 语义）：采样哈希表覆盖 ≤128 MiB 历史；128 MiB 副本实测压缩率差 0.3%、速度持平
- [x] solid + 分卷创建（encoder state 跨成员/跨卷保持，双向字节级验证）
- [x] >4 GiB 单文件创建（spill 流式管线：分块压缩 + 链式 CBC + 精确切卷，内存与文件大小无关）
- [x] **单文件多线程压缩（`-mt` 全速生效）**（2026-08）：流式路径按窗口缓冲，`codec/rar50.rs::encode_chunked_mt`
      将窗口切成 4 MiB 细粒度切片并行编码——每片带前文 tail 上下文 + 共享长距离表
      （`LongRange::find_from` 绝对锚点查询），重复距离缓存按片重置。实测 308.8 MiB 混合文件：
      `-mt8` 4.11s → **1.45–1.51s（≈2.7×）**，已超 WinRAR 7.23 同参 1.80s；压缩率 16.2%→16.7%
      （+3%，缓存重置代价）；`-mt1` 回退旧路径字节级一致；字典 128K/1M/8M/v70 标志全回环验证；
      solid 归档保持串行

### 命令

- [x] `ch`（大小写转换）、`p`（打印）、SFX 转换 `s`/`s-`、列表变体 `lt/lb/vt/vb`（`rar` 与 `unrar` 均已支持）
- [x] `rv[N]` 补恢复卷：对**已存在**分卷集生成 .rev；计数/百分比/默认 10%、封顶 10×ND，官方语义逐项实测；`.rev` 命名跟随卷集零填充
- [x] `r` 修复改流式（`repair_archive_path`）：只驻留恢复数据，超大归档可修复；完好不写输出

### 开关（批次 1–3 完成，均与 WinRAR 实测对照）

| 类别 | 开关 |
|---|---|
| 路径/掩码 | `-r/-r0` `-ep1..4` `-x@/-n@` `-ed` `-as` `-ad/-am` `-ver[n]` `-ms` |
| 时间 | `-ta/-tb/-tl/-tk` `-tn/-to` 过滤；`-ts[m,c,a][+,-,1]` 三时间戳存取；`-tsp` |
| 文件系统 | `-ol/-oh` 链接 redirect；`-os` NTFS ADS 双向；`-ow` owner |
| 归档组织 | `-z` 注释 `-ag` 命名 `-sl/-sm` 大小过滤 `-df` `-t` `-kb` `-op` `-or` |
| 配置体系 | rar.ini / .rarrc / RARINISWITCHES；rarfiles.lst solid 排序（子集规则） |
| 交互/消息 | `-y -o± -idq -ierr -ilog -iver -cfg-` 等；系统动作类（-ieml/-ioff/-isnd）只接受不执行 |

### 工程里程碑

- [x] fmt/clippy 双门：workspace 全量 `cargo fmt --check` + `cargo clippy --all-features -- -D warnings`（codec 热路径的 `too_many_arguments` 用针对性 allow，不做风险重构）
- [x] fuzz：`fuzz/` 独立 crate 五目标（parse/crypto/recovery 读侧 + write/rewrite 写侧），standalone 变异循环 + libFuzzer 双模式；种子语料嵌入真实 WinRAR fixture
- [x] CI 移除（`.woodpecker/ci.yml` 删除，2026-08）：回归验证回归本地 fmt/clippy + fuzz，官方互操作测试继续由 `SA_OFFICIAL_*`/本机 WinRAR 门控手动跑
- [x] 取消钩子 `RarArchive::set_cancel_flag(Arc<AtomicBool>)`：创建/提取/重写/分卷全检查点，`RarError::Cancelled`；binding 的 AbortSignal 接上
- [x] QO 快路径 `RarArchive::open_quick`：只读主头 + QO 记录即可列出（无 QO 回退全扫）；binding `listEntriesQuick`
- [x] 零填充分卷集：写侧对齐——≥10 卷时 writer 直接输出 `part01..partNN`（与 WinRAR 一致，close 收尾 rename）；`discover_volumes` 增加从基名探测 padded 首卷；`.rev` 命名跟随卷集填充；`rec_count > data_count` 误拒已修

- [x] atomic create/append：temp sibling 暂存，close 原子提交
- [x] 加密分卷每块加密记录（flags=1/3）+ `-hp` 分卷读取 + ENDARC flags 修复
- [x] deep-module 九项重构 → 仿 rars workspace 架构迁移（15636d8，计划见 `docs/REFACTOR_MIRROR_RARS.md`）

## 一致拒绝（别"修"）

- 分卷 + 内联恢复记录（`-rr`）——WinRAR 分卷只能用 `.rev`
- 分卷 append；分卷 lock / 注释——官方 rar 同样拒绝或受限

## 已知小差异（记录，互操作无碍）

- solid 且无 rarfiles.lst 时：WinRAR 按扩展名/名字启发式排序，我们按参数顺序
- 目录条目名带尾斜杠

## 备注（改代码前必读）

- 卷大小必须精确：新增块类型（如 QO）记得同步配额记账
- 加密块 padding 是 **zero-fill 不是 PKCS7**——7-Zip 会校验 padding 区全零
