# 不变量与地雷（改代码前先看）

> 最后核对：2026-09-30 @ `99d7b79`；实现细节以源码为准。

本文件是**长期规则**的单一来源：所有「改回去就是 bug」「看着像 bug 其实是设计」
「与官方有意不同」的结论都收在这里，**一条一行**。这里的每条都由源码或测试钉住
（括号里是契约）；过程、日期、实测表与被否方案的论证留在 git 历史，本文件不维护
CHANGELOG。**公开 API 的契约也在本文件**（末尾「公开 API
契约」段）：库与绑定是同一套设计约束的两个出口，分散到两个文件只会让它们漂移。

相关：[`../PLAN.md`](../PLAN.md)（下一步与开放议题）·
[`ARCHITECTURE.md`](ARCHITECTURE.md)（模块地图与设计不变量）·
[`CLI.md`](CLI.md)（命令行与退出码）· [`../CONTEXT.md`](../CONTEXT.md)（术语）。

## 字节对齐（与官方逐字节/逐字段对拍）

- **RAR5 尺寸字段最小 2 字节**：成员头与服务块（QO/RR/CMT）的
  `data_size`/`unpacked_size`/`comp_info` 一律 ≥2 字节；STM 服务块按
  `vint_size(unpacked_size << 12)` 预留（官方先写头后回填，故更宽）。契约
  `member_size_fields_use_a_two_byte_minimum` /
  `stream_block_size_fields_use_the_reserved_width`。
- **主头 locator 恒写**：每个归档的主头尾都有 locator（块 flags 含
  `SKIP_IF_UNKNOWN`），无 QO 记录时 QO 标志仍置、偏移写 0 占位；**有 QO
  与否只看偏移非零，不看标志位**（只看标志会凭空重建 QO
  记录、破坏前缀逐字节不变）。契约
  `locator_is_always_present_with_a_zero_qo_placeholder`。
- **RAR5 时间载体二选一**：头内 4 字节 mtime（`FILE_FLAG_TIME_UNIX`）**或**
  FILE_TIME extra 记录，绝不同时写；有记录就清头内标志（redirect
  与多卷分片头同规则）。契约
  `whole_second_mtime_has_no_file_time_record_on_unix`。
- **RAR5 元数据按宿主平台写**：Windows 上 `host_os`=0 + DOS 属性 + FILE_TIME
  记录的 Windows FILETIME（头里不写 4 字节 mtime），Unix 上 `host_os`=1 +
  `st_mode`；写错平台会让 WinRAR 读取器 NFC 归一化成员名并丢只读/隐藏/系统属性。
- **RAR4 头**：主头**不置** `LONG_BLOCK`；窗口位按归档级规则
  `clamp(ceil_log2(size)−16, min, 6)`（非 solid 用最大成员、solid 用整链总量；
  `unp_ver`<29 恒写 4）；`-m0` 成员写 `unp_ver` 20（加密成员仍 29——`-p` 布局是
  RAR29 的，写 20 会让读取器选错密码）。单卷与分卷 `-m0` 与官方 3.00–6.23
  逐字节相同。
- **RAR4 分卷创建用新式命名**：`base.partNN.rar`（按总卷数补零）+ 每卷主头置
  `MHD_NEWNUMBERING`、20 字节 ENDARC（`prefix_crc32` + `volume_index`；单卷仍是
  7 字节旧式），`.rev` 随之落 trailer
  布局。`-vn`（`old_numbering`）是**唯一**回到 `base.rar`/`base.rNN`
  的开关，且**只作用于 RAR 1.5–4.x**：RAR5/RAR13 恒忽略它，共用命名助手必须经
  `old_volume_numbering()`（否则 RAR5
  分卷创建会在安装阶段找不到暂存卷而整次失败）。契约
  `old_numbering_is_ignored_by_rar5_and_rar13`。
- **RAR4 落 DOS 属性位**：Windows 上直拷 `0x1|0x2|0x4|0x20`（目录含 `0x10`；
  `FILE_ATTRIBUTE_NORMAL` 不落，无 archive 位的文件落 `0x00`），非 Windows 保持
  `0x20`/`0x10`。
- **`NOT_CONTENT_INDEXED`（`0x2000`）要保留**：官方只额外保留这一位，offline /
  pinned / unpinned / no-scrub 一律丢弃，不进子集。写、抽取、`lt` 首列的 `I`
  三处都要处理。
- **RAR 1.5/2.x（`unp_ver` 15/20）不发 ext-time
  记录**：这两代读取器把普通文件头当定长，多发一条 `FHD_EXTTIME`
  会顶开数据偏移、官方 2.90 直接报 `the file header is corrupt`。只有 v29
  成员发（`build_member_ext_time`），四条发射路径都走它。
- **`-htb` 下 BLAKE2sp 记录取代 CRC32 字段**（不是并存）：序列化器顺带清
  `FILE_FLAG_CRC32`，否则每成员比官方多 4 字节。
- **`-rr` 三种形式**：裸 `-rr<N>` 是 legacy RAR4 的 parity **扇区数**，`-rr<N>%`
  是受保护前缀的百分比，裸 `-rr` 是官方默认 3%。RAR5
  记录只按百分比定尺，故裸数字在 RAR5 上按百分比解释。
- **`-rr` 配 `-v` 时每卷各带一份内联记录**，每卷只保护自己的前缀；卷预算按
  `recovery_volume_reserve(prefix_len)`
  的几何长度做**候选前缀二分**（直接按最大记录收窄每卷会白扔约 2 KB）。
- **`rc` 重建卷沿用数据卷自己的零填充宽度**（`.rev`
  的宽度是恢复卷号，两者位数可不同），没有现存数据卷时才用 `layout.width` 兜底。
- **加密块 padding 是 zero-fill，不是 PKCS7**（7-Zip 会校验 padding 区全零）。
- **卷大小必须精确**：新增块类型（如 QO）记得同步配额记账。

## 有意不追平官方

- **`-rr<N>%` 的取整口径**：官方那套取整没有闭式（斜率等于
  P%，偏差是扇区上的加性常数且随尺寸变），我们的规则是
  `max(2, ceil(prefix*P/(100*512)))`——只向上取整、绝不少于请求的百分比。裸
  `-rr<N>`（计数）精确对齐。
- **`rar rr` 只在 3% 上写记录是官方缺陷**：6.23/7.23 无视
  `-rr10`/`-rr20`/已有记录一律写 3%。我们**显式强度照样生效**（与 `a`
  同一套单位），裸 `-rr` 才 3%。
- **`-ts` 的「字母+数字」组合**：官方 `-tsc2`/`-tsa2`/`-ts3` 静默退化成仅 mtime
  （丢掉用户点名的种类）。我们按文档语义处理（`1` 对**已选**时间做秒截断），故
  `-tsc1`/`-tsa1` 与官方差一个 ns 位。
  同类原则：**静默丢弃用户请求＝缺陷，不照抄**。
- **`rar r` 没修动时不写
  `fixed.<name>`**：官方即使没修成功也拷一份与损坏输入逐字节相同的产物。
  我们按「不能进行的修复不留产物」不写，只提示并在询问后重建
  `rebuilt.<name>`（退出码 3 与询问行为已对齐）。
- **`rar r` 的退出码只由结果决定**：丢成员或没产出 → 3，全部救回 → 0。
  官方按容器分叉（RAR5 头损坏 3、legacy 头损坏 0、RAR13 不产物
  0），同一「丢数据」事实给出不同信号，属官方缺陷。
- **官方对载荷损坏不校验、原样拷坏成员**；我们丢坏成员并逐条打印。
- **WinRAR 的 RAR4
  修复对周期数据有缺陷**：恢复记录块落在其保护的最后部分扇区且成员数据短周期重复时，官方
  `rar r` 会修坏 RR 尾部（6.23 与 7.23 一致）。我们的 parity
  组与官方一致，差别只在**只写回落在前缀里的字节、记录自身永不改写**，故能逐字节修复同样损坏。互操作测试因此用伪随机成员数据。
- **无控制台且非静默时的覆盖询问**：官方先打印再读 stdin 失败并
  `Program aborted`；我们直接按跳过处理（不询问、继续、全跳则 exit 10）。同一
  TTY 场景与官方一致。
- **RAR4 成员注释（`cf`）**：v29+ 写侧在成员数据后发射**独立**
  `COMM_HEAD`（0x75），官方 6.23/7.23 `t`/`x` 均
  `All OK`；pre-RAR3（`unp_ver`<29）保持嵌套布局。多卷成员注释继续拒绝。
- **`-ed` / `-ed1` 语义**：`-ed` 是「完全不写目录记录」（属性丢失），`-ed1` 才是
  「只排除不含文件的目录」（子树有文件的目录保留）。
- **Windows 联接点的 redirect 目标串**：我们写原始路径（反斜杠），WinRAR 写 NT
  打印名（正斜杠 + `/??/`）。默认抽取对两者都拒绝绝对目标，差别只在 `-ola`
  信任档。
- **RAR4 错口令与损坏头不可区分**：RAR4 没有口令校验值，加密块的垃圾
  `head_size`、解析/CRC 失败**统一映射 `WrongPassword`（exit
  11）**，别试图"区分"。
- **`-idn` 仍不生效、`-idc` 恒等效**（我们不打印官方版权/Trial 横幅）、`-iver`
  文案与官方不同；`rar r` 对载荷损坏的处置见上。

## 提取与安全

- **不安全链接目标只跳过该链接、不中止整轮**（记进
  `ExtractionReport::refused`，与 `-o-` 的 `skipped` 分开，exit 1）；
  链接目标的校验在删除既有目标**之前**。
- **Windows 保留/歧义成员名改成可用名字**，不是拒绝：`:` → `_`，组件最后一字节是
  `.`/空格 → `_`，组件整体等于保留设备名 → 前加 `_`，带扩展名的
  `aux.txt`/`NUL.log`
  不动。`-oni`（`ExtractOptions::allow_incompatible_names`）跳过设备名前缀，但
  `:` 与尾部点/空格仍归一（我们选不丢名字的安全处理）。
  护栏不变：空名/绝对路径/`..`/NUL 仍拒。官方的
  `WARNING: Attempting to correct the invalid ... name` 走 stderr 且 `-idq`
  不抑制，由 `ExtractionReport::corrected` 携带。
- **目的地类型冲突两个方向都要成功**：文件压同名空目录 → 删空目录再装；
  目录压同名文件 → 删文件再建；**非空目录**才报 `Cannot create` + exit
  9（`ErrorCode::Create`）。安装失败一律清掉暂存文件。
- **先校验后建目录**：路径包含性校验必须在 `create_dir_all` 之前。
- **重定向成员重复抽取要先删既有非目录目标**（普通成员走原子替换，两者行为要一致），
  目录挡路则明确拒绝。
- **`-or` 优先于询问**：同设时不询问、直接编号改名，且编号只从**原文件名**取一次
  （否则会写出 `a(1)(2).txt`）。
- **有界内存**：成员从不整块进内存（分块大小与峰值见
  [`ARCHITECTURE.md`](ARCHITECTURE.md)「设计不变量」）；legacy 大成员（≥64 MiB）
  与压缩成员同样走 spill。
- **`-mcde+` 按 64 KiB 块二选一**，记录**不相交**（重叠记录会被官方 UnRAR 拒）；
  缓冲与流式两条路径都如此。

## 恢复与编辑

- **跨多条固态链的删除要收集全部受影响链**：只按最低被删序号取一条，会漏掉链外的被删成员，其后的幸存者逐字拷贝却引用已消失的窗口（产出损坏但
  `apply` 报成功）。每条链头都要重建共享窗口。
- **多卷重写失败不提交半成品**：暂存卷集只在写循环全部成功后摘除，失败要恢复
  path/volume 状态并清暂存文件（`ArchiveEditor` 与 `ArchiveWriter` 一样失败即
  abort）。
- **无 ENDARC 的老归档（RAR
  2.9）可编辑**：以最后一个块的结束为重建插入点，重写时补一条新 ENDARC。
- **`rar r` 的打捞扫描只覆盖 RAR5 与 RAR4 且仅非
  `-hp`**（加密流的块头无法廉价探测），RAR 1.3/1.4 无打捞。
- **legacy RR 定位有两版**：容忍版 `scan_protect_tolerant` **仅 repair
  入口**用，编辑路径仍走严格版（坏归档在那里就该报错）。
- **恢复记录要检测/修复它自己那个不完整尾扇区**（按前缀零填充算
  tag，只写回前缀部分，绝不改写记录自身字节）；修不动时逐条报
  `cannot recover data` 并询问是否重建。
- **RAR5 `-hp` 编辑**：重写头走 `write_block_header`
  重加密，多卷重写首卷补发明文 ENCR 头；注释块只返回头 frame、payload
  单独写（`-hp` 下加密 data area 是 bug）。`-hp`
  与内联恢复记录**可以共存**（记录重建，`.rev` 可选），`-k`（锁定档）仍拒绝。
- **RAR4 `-hp` 编辑**：改名/注释/锁定逐卷重写，`-hp`
  档在同样加密下编辑（需要口令）；已有的逐卷 NEWSUB
  记录在重写时**按原强度重建**（丢弃旧记录再在末块前重发，否则 `rar r` 会用错
  parity 改坏数据），并在装好新卷后从新卷重建 legacy `.rev`。
  **但显式请求新强度（`rar rr <set>` / `-rr`）在 RAR4
  分卷上是拒绝项**（见「一致拒绝」）。
- **PROTECT_HEAD（RAR 2.5 时代记录）不可就地编辑/追加**：只有 NEWSUB `Protect+`
  且落在 ENDARC 之前的记录能随前缀重写重建，其余形态明确报错、要求重建归档。
- **`reconstruct` 保留的是容器族，不是成员 codec**：legacy 源重建为 RAR4
  容器、成员是 STORE（`unp_ver` 20）。**别把成员改回 29**，那会破坏 `-m0`
  的逐字节对拍。

## 工程与架构

- **分层单向**：层次与逐条约束的**权威**在
  [`ARCHITECTURE.md`](ARCHITECTURE.md)「设计不变量」（`archive` → `format` →
  `engine` → `codec`/`crypto`/`fs`/`model`/`options`，根级词汇是叶子，`recovery`
  在 `format` 之上）。这里只记规则：**下层不得反向命名上层**，逐行由
  `tests/architecture_boundaries.rs` 与 `docs/dependency-matrix.txt`
  钉住；发现边像违规是**评审事件**，不是更新快照。
- **CLI 错误类别不得被字符串化吞掉**：transaction 闭包、`open_editor`、
  `apply_version_edits`、comment/recovery/move 路径用 `CliResult` +
  `CliError::context(..)`，否则 `u -pwrong` 会从 11 退化成 fatal 2。
- **选项结构可加性契约**：`ExtractOptions` 字段是
  `pub`，新加字段会打断穷举字面量的调用方——**用 `..Default::default()` 构造**；
  只有两个有意逐字段列出的映射点（CLI `ExtractRequest::options`、绑定
  `options.rs`）。完全体（私有字段 + builder）留待破坏性发布。
- **错误构造集中化**：`RarError` 的构造一律经 `error.rs`
  的构造函数（文案风格单一来源：小写、无句点、家族前缀
  `RAR4:`/`RAR 1.3:`/`RAR5:`/`RARVM:`），匹配分支仍直接用变体。
- **CLI「绝不静默丢弃开关」的唯一例外**：官方把已知开关挂在不适用命令上默默忽略，为兼容我们也过滤，但**故意拒绝的
  `-dr`/`-dw`/`-vd` 必须仍走到显式拒绝**，`-p` 的安全豁免不能被丢掉。
- **没有固定 MSRV**：`edition = "2024"`，CI 跑稳定版全矩阵。
- **Windows 上绑定 crate 必须用 MSVC 目标**：`napi-build` 在 windows+gnu
  下要求一个没有发行版会带的 `libnode.dll`。
- **版本号五处一致**（发布不变式）：三个 crate 的 `[package] version`、
  `crates/rar-napi/package.json`、根 `Cargo.toml` 的 workspace 依赖版本；发布
  tag `vX.Y.Z` 由 Release job 校验。改版本号要同步刷新 `Cargo.lock` 与
  `fuzz/Cargo.lock`。
- **测试不在主 CI 里跑**：CI 只跑确定性 smoke（`cargo test -p rar-rs` +
  `cargo test -p rar-cli --bins`），官方工具互操作与 JS/WASI
  绑定套件是本地闸门。**改行为后本地必须自己跑受影响套件**——0.12.0
  曾带着过期断言发布。
- **文档一致性靠人工**：面向实现的文档（`README`/`CONTEXT`/`ARCHITECTURE`/`CLI`/
  `testing`/`rar4-creation-spec`）在标题下标注
  `最后核对：YYYY-MM-DD @ <短 commit>`，改行为要同步刷新。
  项目**不加**强制校验脚本（已定）。
- **`MAX_CATALOG_ENTRIES` / `MAX_MEMBER_CHUNKS` 等上限对每一族都要设**：RAR13
  每条 21 字节头也会 push 条目，legacy 跨卷合并同样要查 chunk
  上限（`check_chunk_cap`）。
- **RAR5 打包读取要读满声明长度并防御缺卷**（`get(vol)` 而非索引），与 RAR4 的
  `read_exact` 口径一致。
- **RAR4 清空归档先查 victim 非文件**（与 RAR5 同款守卫），否则会把同名目录 park
  成隐藏备份并遗留。
- **标记编译门用 `cfg(unix)`/`cfg(windows)`，别用格式常量**：`OS_WINDOWS`
  是格式常量不是编译门；`wasm32-wasip1-threads` 既非 unix 也非 windows，漏门会让
  CI 红。

## 公开 API 契约（库与绑定）

- **错误类别必须一路传到消费者**：库的 `ErrorCode`（16
  类）是稳定分类，**不许**在某一层被压成"参数错/一般失败"两个桶——CLI 用它映射 11
  个退出码，绑定用它填 `RarError.rarCode`。napi-rs 只能经 `napi_create_error`
  抛错，而它唯一可配的字段 `code` 被锁死为 status 串，**Rust
  侧无法挂自定义属性**；因此类别随消息走（`[rar-rs:<code>] …`，`src/error.rs` 的
  `message_with_code`），由手写入口 `rar-rs.js` 解析成
  `RarError { code, rarCode }`。契约由
  `error::tests::every_library_category_keeps_its_code_and_status` 与 JS
  `every rejection is a RarError carrying a stable rarCode` 钉住。
- **无标记的错误按 status 判类，不许一律当 internal**：绑定自己做的标量校验
  （`level`/`threads`/`volumeSize`/条目的 `kind`·`path`·`data`）抛的是裸
  `InvalidArg`，那是调用方**能修**的参数错 → `invalid_option`；只有 `internal`
  才是绑定自身的 bug（panic 防火墙）。把两者混为一谈会让消费者以为要用例上报。
- **`ExtractErrorPolicy`
  的两种语义都要在文档里说清**：`Abort`（默认＝官方）在第一个失败成员处中止并返回该错误；`Collect`
  记录进 `ExtractionReport::failures` 后继续，
  **整轮仍算成功**（是否把"部分成功"当失败由调用方判断，本层不猜）。**取消永不收集**，始终中止。绑定侧即
  `ExtractArchiveOptions.collectErrors` + `ExtractionResult.failures`。
- **并行提取必须给逐成员语义让路**：开了进度回调、或策略不是 `Abort` 时，
  `extract_all_parallel` 一律返回 `None`
  退回串行——它先把所有成员解码完，"在第一个失败处停下"根本无法实现，与其猜不如让路。改这些条件前先想清楚顺序语义。
- **提取进度的粒度是逐成员，且不许倒退**：成员经临时兄弟文件**原子安装**，中途没有可发布的字节数；因此
  `on_progress` 的 `bytes_written`
  在一个成员落盘后才前进（单个巨成员会长时间只报很少）。终态事件必须重发**库给出的真实总量**，写
  `done: 1, total: 1` 会让消费者看到进度从 80000/80000 倒退到
  1/1；库从未报告过总量时 **不发**终态事件（否则 1/1 会被读成字节数）。
- **`ExtractionReport` / `ExtractOptions` 的派生是刻意的**：报告不能 `Clone`/
  `PartialEq`（`failures` 里的 `RarError` 含 I/O
  源，两者都不成立），要取出失败用 `into_failures()`；`ExtractOptions` 可
  `Clone`（内部的进度 sink 是 `Arc<Mutex<..>>`），但 `PartialEq`
  是手写的——**比较时忽略进度回调**，闭包不可比。
- **生成物与手写物必须分名，别和生成器抢文件名**：`napi build --platform --js
  binding.js --dts binding.d.ts`
  决定生成物叫 `binding.*`；**手写入口是 `rar-rs.js`（错误再水化）+
  `rar-rs.d.ts`（公开类型）+ `wasi-path-map.cjs`**，它们必须跟踪、不能出现在
  `crates/rar-napi/.gitignore` 里。历史教训：入口曾叫 `index.js`，于是
  `napi build`
  会把包入口整个删掉（生成器的"重写入口"步骤），每次手写包装都得靠事后打补丁。
- **WASI 路径映射表按名字维护，并让漂移失败于构建**：`patch-wasi-loader.mjs`
  的包装表必须以导出名作键，且 `assertEveryExportIsWrapped` 在生成 loader
  出现表里没有的导出时 **让构建失败**。按下标维护的表曾经漏掉
  `setMemberComment`，使 WASI 后端把 Windows 主机路径直接交给沙箱并
  ENOENT；`mapMemberCommentArgs`
  只翻归档路径，**成员名是归档内名，翻了就坏**。JS 侧由
  `__test__/wasi-path-map.test.mjs` 钉住。
- **仓库 URL 以远端为准**：根 `Cargo.toml` 的 `repository` 与
  `crates/rar-napi/package.json` 的 `repository`/`homepage`/`bugs`
  必须同源。曾长期写着一个不存在的 codeberg 路径，而真实远端是 GitHub（三个
  crate 的 metadata 一起错）。
- **`.d.ts` 必须被编译**：`npm run typecheck`（`tsc --noEmit` 加
  `__test__/types.test-d.ts` 的编译期断言）是它的闸门，CI 在 build 之后运行。
  挡住的是：公开面里声明了实现没有的东西。
