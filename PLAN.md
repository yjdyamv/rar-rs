# rar-rs 计划

> 最后核对：2026-09-24（本轮：**RAR4 分卷创建对齐官方新式命名 + `.rev` trailer
> 布局**——创建默认改 `base.partNN.rar`（零填充）+ 每卷主头 `MHD_NEWNUMBERING`
>
> - 官方 20 字节 `ENDARC`（其后 7 零字节），`.rev` 随之自动落 trailer 布局、
>   名字自动成 `base.partNN.rev`；新增 `-vn`（`old_numbering`）回旧式
>   `.rar`/`.rNN` 命名。官方 6.23 `t` 读我们产的新式集与 `.rev` 报
>   `All OK`、`rc` 重建缺失卷 **逐字节还原**，`.rev` 与官方逐字节相同；RAR13
>   仍旧式命名。**已知残余**：RAR4 成员头的三个字节字段（主头
>   `LONG_BLOCK`、窗口位、store 的 `unp_ver`）仍未对齐
>   官方，属既有纯字节外观差异，见「已知小差异」。 前轮：**RAR5
>   头字节对齐官方**——成员头与 QO/RR/CMT 服务块的
>   `data_size`/`unpacked_size`/`comp_info` 改写到官方的最小 2 字节、STM
>   服务块按官方的 `vint_size(unpacked_size << 12)` 预留（此前三者全是
>   最小编码，−3 字节/成员）；**Unix 时间载体去重**，头内 mtime 与 FILE_TIME
>   记录 二选一（此前两者都写））。另记 Windows 属性位子集、`-ts`
>   数字组合两处差异。 前轮：**主头 locator 恒写**（含 QO 0 占位、主头块 flags
>   0x5），并顺带修 `split_main_extra` 只看标志位 就重建 QO 记录的真
>   bug；locator 的偏移**宽度**仍是我们定长 5 字节 vs 官方按预计 大小 3–6
>   字节）。更前轮：**元数据改为按宿主平台写** ——新增 `platform.rs` 统一
>   `host_os`/属性/时间载体，Windows 上写 `host_os`=0 + DOS 属性 + FILE_TIME
>   记录的 Windows FILETIME，WinRAR/我们都恢复只读/隐藏/系统位、也不再被 NFC
>   归一化成员名；RAR4 顺带落 DOS 属性位（直拷，含 `0x00` 那个官方用例）。另修
>   抽取侧「带 DIRECTORY 位的重定向被当目录」与 `-si`/`add_bytes` 在 Windows 丢
>   mtime 两处缺陷）；更前轮：**真实用户语料 × 多文件类型的双向对拍** ——新增
>   `winrar_interop::scenarios`（空文件/无扩展名/点文件/含空格引号井号的名字/
>   CJK 与 emoji 名/200
>   字节长名/多级目录/空目录/大量小文件/文本与随机与结构化数据，
>   默认/m0/m5/solid/分卷 五个开关档 × 两种创建 × 两种读取，另加 unicode
>   归档注释与 NFD 名往返）；并把「Windows 上写 Unix
>   元数据」的两处**可见**后果（成员名被 WinRAR 读取器 NFC
>   归一化、只读/隐藏/系统属性丢失）测清后记入「已知小差异」）。
>   前轮：文档失效引用清理 + WinRAR 对齐路线图； P1 退出码、P2
>   交互式覆盖询问、P3 `rar r` 无记录重建（含 RAR5/RAR4 头损坏打捞、 legacy RR
>   定位加固、全扇区检测/修复 + 逐扇区报告 + 询问）、 P4 RAR5 `-hp`
>   编辑均已落地；另对齐 `-htb` 语义、`-rr`/`rr`
>   的强度口径（百分比只向上取整）、
>   开关解析宽松度（官方缺陷处按「丢数据即非零」报退出码）、抽取/写入两侧的线程与
>   MOTW 设置对称、`missing_docs` 文档闸门、绑定 crate 的 MSVC 构建要求、列目录
>   QO 快路径、RAR13 修复行为、静默模式按 二进制的覆盖语义， 并记录 RAR5
>   元数据的字节 差异（对拍官方 Windows 与 Linux 两个 构建）；
>   实现细节以源码为准。

本文件只留**结论**与**下一步**：过程与逐批验证记录在 git 历史
（旧版详单：`git show c2c43d4:PLAN.md`），本文件不维护 CHANGELOG。

相关文档：术语 [`CONTEXT.md`](CONTEXT.md) · 模块图
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) · 字节格式
[`docs/FORMAT_RAR5_RAR7.html`](docs/FORMAT_RAR5_RAR7.html) · 性能议题
[`docs/issues/compression-perf/`](docs/issues/compression-perf/) · 出处
[`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md)

## 待办

### 发布收口

- [ ] **打包与发布顺序**：`rar-rs`（0.11.0）已能 `cargo package` 并通过校验；
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

- **逐卷内联恢复记录**（2026-09-24 对拍官方 7.23 + 6.23）：`-rr` 配 `-v`
  时**每卷 各带一份**内联记录（官方 `rar l` 逐卷显示
  `recovery record`），每卷只保护该卷 自己的前缀（`-rr<N>%` =
  该卷前缀的百分比；题外：RAR5 记录只按百分比定尺、 legacy 裸 `-rr<N>` 是该卷的
  parity 扇区数），`-rr` 与 `-rv` 可共用。创建与分卷 重写**共用**卷收尾
  `create.rs::finish_volume(next_volume)`（RAR4 为 `finish_volume_rar4`）：先
  patch 该卷主头 locator 的 RR 偏移、再写记录、最后
  ENDARC；`write_archive_header_vol` 置 `ARCHIVE_FLAG_RECOVERY` 并预留 RR 偏移
  字段。**卷预算**：`recovery_volume_reserve(prefix_len)` 给纯几何的记录长度，
  分卷预算按**候选前缀二分**求最大 chunk（记录大小随前缀变；先「收窄到刚放得下」
  每卷会白扔约 2 KB）——卷不再超过 `volume_size`。**编辑**：分卷 `d`/`rn`/`ch`/
  `k`/`c` 从原集合 carry 强度并重建逐卷记录（显式 `-rr` 覆盖），`rar rr`
  亦可作用 于已有卷集；RAR4 分卷改名/注释路径丢弃旧记录后在 ENDARC
  前按原强度重建（否则 改名让旧记录失配，`rar r` 会用错 parity
  改坏数据），并在装好新卷后**从新卷重建 legacy `.rev`**（`.rev` 是卷字节的 XOR
  parity，改名后旧 parity 必然失配；旧名形状/计数变了的一并
  retire，重建失败时删掉 旧 parity，而不是留下一个会修出坏卷的
  `.rev`）。**已知残余**：官方 `-qo-` 下无 RR 时不写 locator、只有 RR 时 locator
  flags 仅 0x02（无 QO 字段），我们按 官方**默认**模式的形状恒写 QO 占位，故逐卷
  RR 的字节在 QO 偏移那几字节上仍不同 （`CONTEXT.md` 的 Locator
  词条同此）。**不追平**：官方对自己的卷集一律
  `Cannot modify volume`，`rar rr <set>` 只改被点名的卷并把该卷撑过
  `volume_size` （102400→111902）。

- **RAR4 分卷创建对齐官方新式命名 + `.rev` trailer 布局**（2026-09-24 逐字节对拍
  官方 5.91/6.23）。口径「**创建用新式，修复同时支持老式与新式**」。① **命名**：
  创建 RAR4 分卷改用 WinRAR 默认的零填充 `base.partNN.rar`（宽度 =
  总卷数位数）， 并给**每一卷**主头置 `MHD_NEWNUMBERING`；RAR13 保持
  `base.rar`/`base.rNN`。 ② **ENDARC**：多卷每卷收尾改官方 20
  字节形式（`HEAD_CRC(2) 0x7b(1)
  HEAD_FLAGS(2)=0x400e|0x0001(非末卷) HEAD_SIZE(2)=0x0014 prefix_crc32(4)
  volume_index(2) 0×7`）：`prefix_crc32`
  是 ENDARC 之前**整个卷字节**的 CRC-32、 `volume_index` 从 0 起；**单卷归档仍用
  7 字节旧形式**（flags `0x4000`）。官方 没有「其后补零填卷」（实测每卷都正好
  ENDARC 收尾，末卷也短），故只改形状不改 补零；卷预算按 20 字节（`-hp` 为
  40）预留。③ **`.rev` 自动落 trailer**：tail7 变 0 后 `use_trailer_format`
  自动选 Trailer，名字自动成 `base.partNN.rev`
  （新式）/`baseN.rev`（`-vn`）——官方 `t` 现在读我们的 `.rev` 报 `All OK` （此前
  `Unknown method`），`rc` 重建缺失卷**逐字节还原**，`.rev` 与官方
  **逐字节相同**。④ **`-vn`**：新增 `WriterOptions::old_numbering`（CLI `-vn`）
  回到旧式命名且不置 `MHD_NEWNUMBERING`。⑤ **寻址**：新式集（`rv`/`rc`/编辑）
  用**首卷** `base.part1.rar` 寻址，官方亦然（官方 `rc base.rar` 报
  `Cannot open`）。⑥ `stale_volume_paths` 的 RAR4 分支现在同时认两族命名与
  `.rev`，覆盖切换命名的重写。`canonical_recovery_names` 的歧义候选 bug 因
  trailer 名不含计数而**失效**（无需再按数据卷评分）。契约由
  `rar4_create::rar4_multivolume_uses_new_numbering_and_the_volume_endarc`、
  `rar4_rev3::{legacy_build_and_rebuild_each_missing_volume,
  old_numbering_layout_builds_and_rebuilds}`
  与
  `winrar_interop::recovery::rar4_recovery_volumes_match_winrar_byte_for_byte`
  钉住。

- **RAR5 成员/服务头的尺寸字段补到官方宽度**（2026-09-23 对拍官方 7.23）：官方把
  `data_size`（块信封的 Data Size）、`unpacked_size`、`comp_info` 三个 vint 一律
  写到**至少 2 字节**（值 11 写作 `8b 00`，我们此前写 `0b`），`-m0` 档实测每个
  成员因此比我们多 3 字节；其余字段（`attributes`/`host_os`/名字长度/extra 区
  大小/块 flags）官方仍用最小编码。现同口径补齐——文件头与 QO/RR/CMT 服务块
  一律最小 2 字节；**STM 服务块**按官方口径预留 `vint_size(unpacked_size << 12)`
  （≥2 字节，实测 4 字节流→3、512→4、64 KiB→5、8 MiB→6），官方对 STM 是「先
  写头、后回填」，故比其它块宽。契约由 `format::rar5::headers::serialize` 的
  `member_size_fields_use_a_two_byte_minimum` /
  `service_block_size_fields_use_a_two_byte_minimum` /
  `stream_block_size_fields_use_the_reserved_width` /
  `stream_size_field_width_matches_winrar` 钉住。
- **RAR5 时间载体去重（Unix）**（2026-09-23 对拍官方 7.23 Linux 构建）：官方成员
  的时间只占**一处**——头内 4 字节 mtime（置 `FILE_FLAG_TIME_UNIX`）**或**
  FILE_TIME extra 记录，绝不同时出现（实测：秒精度仅 mtime ⇒ `ff=6`、无记录； 有
  ns 或带 ctime/atime ⇒ `ff=4`、记录带全部出现的时间与 ns 位；多卷每个分片头
  同规则，中间卷也只带 FILE_TIME 一条）。我们此前两者都写（Unix 多写入记录、ns
  档还多出头内 4 字节）。现按「有记录即清标志」判定
  （`rar5_time_fields(.., has_time_record)`，记录由 `time_extra_cfg` 只在
  Windows 或「有 ctime/atime/亚秒」时生成），redirect 与多卷分片头同规则。契约
  由 `format_assertions::whole_second_mtime_has_no_file_time_record_on_unix` 与
  `nanosecond_mtime_roundtrip`（新增「有记录 ⇒ 头内字段不置位」断言）钉住。
- **RAR5 元数据按宿主平台写**（2026-09-23）：此前一律写 Unix 风格（`host_os`=1 +
  `st_mode` + 头内 mtime），Windows 上因此丢只读/隐藏/系统属性，WinRAR 还会把
  NFD 成员名 NFC 归一化（根因由实验钉住：只把成员头 Host OS 字节 1→0
  即消失）。现按平台 写：Windows 上 `host_os`=0 + DOS 属性（普通文件
  `0x20`+R/H/S、目录 `0x10`、symlink `0x420`、junction `0x410`、hardlink/copy
  `0x20`）+ FILE_TIME 记录的 Windows FILETIME （清
  `FILE_FLAG_TIME_UNIX`、头里不写 4 字节 mtime；`-ts1` 仍用 unix 秒）；Unix 侧
  不变（与官方 Linux 构建逐字节相同）。目录/联接点的 DIRECTORY
  位也与官方一致，抽取侧
  相应改为**先认重定向再当目录**（`is_directory_entry`），否则会把带 DIRECTORY
  位的 联接点建成空目录（`-oh`/`-oi` 重定向同理）。`-si`/`add_bytes` 成员的
  mtime 在 Windows 上改由 FILE_TIME 记录承载（此前会整条丢失）。契约由
  `winrar_interop::scenarios::windows_metadata_round_trips_through_winrar`、
  `format_assertions::nanosecond_mtime_roundtrip` 与
  `cli_behavior::parity2::cli_extracts_a_junction_as_a_real_mount_point` 钉住。
- **RAR4 落 DOS 属性位**（2026-09-23 对拍官方 6.23）：RAR4 的属性字段是 DOS 位，
  写侧此前只落 `0x20`/`0x10`，只读/隐藏/系统位丢失（只读文件解出仍是读写）。现按
  宿主平台取——Windows 上直拷文件属性（`0x1|0x2|0x4|0x20`，目录含 `0x10`；
  `FILE_ATTRIBUTE_NORMAL` 不落，故无 archive 位的文件落 `0x00`，与官方一致）、非
  Windows 保持 `0x20`/`0x10`（Unix 产物不变）。属性同时写进**模型条目**
  （`push_rar4_entry` 新收 `attr`），使 `lt` 与后续重打包（`attributes & 0xFF`）
  与磁盘一致。契约由
  `winrar_interop::rar4_create::rar4_stores_and_restores_dos_attributes` 钉住。
- **主头 locator 改为恒写**（2026-09-23 对拍官方
  7.23）：官方**每个**归档都在主头 尾写 locator 记录（主头块 flags 含
  `SKIP_IF_UNKNOWN`，即 0x5），QO 字段恒在—— 无 QO 记录时 QO 标志仍置、偏移写 0
  占位，RR 字段只在有恢复记录时才出现；我们 此前只在有 QO/RR
  时写。现同样恒写（`build_locator_body` 恒发 QO 字段）并置 0x4
  块标志，主头定长部分不再比官方小 7 字节。**顺带修一个真 bug**：
  `split_main_extra` 只看 QO **标志位**就认定「有 QO 记录」，于是追加/重写一个
  官方归档（或我们现在写的任何归档）时**凭空重建出一条 QO 记录**（实测 509→615
  字节、前缀不再逐字节不变）——现按官方的语义只认**非零偏移**，`had_qo`/`had_rr`
  同此。契约由
  `locator::tests::locator_is_always_present_with_a_zero_qo_placeholder` 与
  `rewrite_tests::delete_from_solid_archive_recompresses_chain` 钉住。

- **不安全链接目标不再中止整轮抽取**（2026-09-23 对拍官方 7.23）：目标逃出目的
  目录的 symlink/junction 此前让 `extract_all` 直接返回 `Security`
  错误并**中止整轮** （`extract_redirection` 的 `?`）——既违背 `-ola`
  的文档口径（应「拒绝该链接」），也让 我们**自己** `-ol`
  建的联接点归档解不动（`rar a -ol` 后 `rar x` 报错、其余成员也不
  落盘）。官方是**只跳过该链接**（`Skipping the potentially unsafe X -> Y link`）并
  **继续**，退出 **1**。现：拒绝该链接但继续，记进新的
  `ExtractionReport::refused`（与 `-o-` 的 `skipped` 分开），CLI 打印同样的
  `Skipping the potentially unsafe ... link` 并 exit **1**（`-o-` 的跳过仍是
  0）。安全
  策略本身不变（仍不物化逃逸链接，且校验移到删除既有目标**之前**，拒绝时不动原文件）。
  契约由 `rewrite_tests::symlink_targets_escaping_the_destination_are_skipped`
  钉住。
- **`-tsc` / `-tsa` 不再丢掉修改时间**（2026-09-23 对拍官方 7.23）：
  `parse_ts_specs` 的 `save` 从全 `false` 起步、只置位被点名的种类，于是单独
  `-tsc` / `-tsa` 把 `save_mtime` 也变成 `false` —— 归档里**整个修改时间消失**
  （`lt` 没有 `Modified:` 行，官方是 `Modified:` +
  `Created:`/`Accessed:`）。现从 `[true,false,false]` 起步（mtime
  默认开，点名只是**追加**，只有显式 `-tsm-` 才去掉 mtime）。契约由
  `time::tests::ts_specs_add_times_without_dropping_mtime` 钉住。
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
- **跨多条固态链的删除**（2026-09-23）：`edit_plan`
  原先只按**最低被删序号**取一条
  链范围，链外的被删成员被静默跳过，其后的固态幸存者逐字拷贝却引用着已消失的窗口
  （产出损坏、`apply` 仍报成功）。现收集**全部**受影响链（逐条
  `chain_range_around`、去重排序），planner 以游标进入每条链，并给
  `RewriteOp::Recompress` 加 `chain_head` ——
  执行器在**每条**链头重建共享窗口（此前
  只建一次，第二条链会引用第一条链的窗口）。契约由
  `delete_across_two_solid_chains_recompresses_both` 钉住。
- **多卷重写失败不再提交半成品**（2026-09-23）：`rewrite_multivolume` 先把
  `pending` （暂存卷集）挂上、只在写循环全部成功后摘除；循环内任一 `?`
  早退都会让 `Drop` 的 `close()` 把**截断的首卷**装上并 retire
  其余卷（静默丢数据）。现把挂载段拆成
  `write_staged_volume_set`，失败时在其外恢复 path/volume 状态、摘除 `pending`
  并清除 暂存文件（此前 `ArchiveEditor` 缺 `ArchiveWriter` 那样的失败即
  abort）。契约由 `multivolume_edit_recovers_state_after_an_abort` 钉住。
- **`-or` / 交互 Rename 的编号不嵌套**（2026-09-23）：`next_free_name`
  从**已被改名** 的候选上重取 stem/ext，两次冲突就写成
  `a(1)(2).txt`（WinRAR/UnRAR 是 `a(2).txt`）。 现只从原文件名取一次。契约由
  `auto_rename_numbers_without_nesting_the_suffix` 钉住。
- **重定向成员可被覆盖**（2026-09-23）：symlink/hardlink/junction
  此前直接创建、不删 既有目标，重复抽取在 `EEXIST`
  上中止（普通成员走原子替换，行为不一致）。现先删既有
  非目录目标，目录挡路则明确拒绝。契约由
  `redirect_members_are_replaced_on_reextract` 钉住。
- **`-or` 对询问的优先级**（2026-09-23）：`prompt_overwrite` 与 `auto_rename`
  同设时，代码先走询问块再落到改名，与 `ExtractOptions::prompt_overwrite`
  文档「`-or` 优先」不符（`Skip`
  会连改名一起吞掉）。现同设时**不询问**、直接编号 改名。契约由
  `auto_rename_takes_precedence_over_the_prompt` 钉住。
- **RAR4 加密 STORE 成员的错口令**（2026-09-23）：成员级 CRC 校验不经过
  `map_codec_error`，错误口令报 `Crc`（exit 3）而压缩成员报
  `WrongPassword`（exit 11）。现加密成员的 CRC 失配统一映射为
  `WrongPassword`（RAR4 无口令校验值，与损坏 不可区分）。契约由
  `rar4_wrong_password_on_a_stored_member_is_wrong_password` 钉住。
- **RAR5 打包读取的截断与缺卷**（2026-09-23）：`read_chunk` 用
  `take(len).read_to_end`（短读静默）并按 `volume_paths[vol]`
  直接索引。现校验读满 声明长度（否则报 `Format` 截断）并用 `get(vol)`
  防御缺卷，与 RAR4 的 `read_exact` 口径一致。
- **RAR4 清空归档的目录挡路**（2026-09-23）：`erase_rar4_archive` 缺 RAR5 同款
  「victim 非文件」守卫，会把同名目录 park 成隐藏备份、删失败后遗留。现与 RAR5
  一致 地拒绝。
- **RAR13 目录无上限**（2026-09-23）：`rar13::parse_volume` 每条 21 字节头就
  push 一个条目且不查上限（RAR4/RAR5 都走
  `check_entry_cap`），手工构造的文件可无界膨胀。 现同样按 `MAX_CATALOG_ENTRIES`
  设限。

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
  `ArchiveReader::set_overwrite_prompt`（回调挂在 `ReadState`；此后 MOTW 已改为
  `ExtractOptions::mark_web`，见下条）； TTY 且未给 `-y`/`-o±`/`-or`/`-f`/`-u`
  时 CLI 注入 WinRAR 式 `Y/N/A/R/Q` 询问 （`output::prompt_overwrite`；库不读
  stdin），非 TTY 保持跳过，且询问时强制 串行抽取。契约由 `overwrite_prompt.rs`
  与 `cli_interactive_overwrite_prompt` 钉住。**静默语义按二进制区分**
  （2026-09-22 官方 6.23/7.23 实测）：`Rar.exe x -idq` 对询问一律答
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
  `salvage_damaged`。**退出码只由结果决定（不照抄官方的按容器分叉）**：丢了成员
  （`dropped` 或 `skipped_damage`）或修复没产出任何东西 → exit **3**；全部救回 →
  exit **0**。官方实测这里是按容器分的——RAR5 头损坏 3、**legacy 头损坏
  0**、载荷损坏两边 0、RAR 1.3/1.4 不产物也
  0——同一「丢数据」事实给出不同信号，属官方缺陷，**不追平** （2026-09-22
  定案）。官方对载荷损坏是**不校验、原样拷贝坏成员**（实测 `rebuilt` 里 f1 报
  checksum error），我们改为丢坏成员并逐条打印。**RAR 1.3/1.4**：官方打印
  同样的横幅后一行 `Cannot repair archive with old format`、不产物、exit 0；我们
  打印同样的行，但按「没产出＝失败」报 exit 3。契约由
  `cli_repair_salvages_a_legacy_header`（legacy 丢成员 → 3）与
  `cli_repair_reports_rar13_as_unrepairable`（RAR13 不产物 →
  3）钉住。**范围**：打捞扫描覆盖 RAR5 与 RAR4，且仅**非
  `-hp`**（加密流的块头无法廉价探测）；RAR 1.3/1.4 无打捞。契约由
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
  重同步到下一个合法块、继续找记录。**检测/修复覆盖全扇区**（2026-09-22 官方
  6.23 逐字节实测）：此前只比对「完整扇区」（`repairable_blocks`），记录**自己
  那个不完整尾扇区**的 tag 从不比对——坏在那里时我们**谎报 `All OK`**（同一份文件
  官方 `rar r` 能修好、`unrar t` 也报
  `p2.txt - checksum error`，我们却说健康）。
  现比对记录的**每一个**声明扇区：不完整尾部按**前缀零填充**语义算 tag（实测官方
  tag 与 parity 都是这个语义，`parity slot 7: xor-of-group == parity-on-disk` 为
  真），重建时**只写回前缀部分**、绝不改写记录自身字节。我们的 writer
  同步修正：parity 组此前 `if block < full_sectors` 排除了那个尾部扇区（比官方
  弱），现已与官方一致。
  **逐扇区报告**也照官方：`Sector N (offsets {起始:X}...{结束:X}) damaged -
  data recovered|cannot recover data`（十六进制；实测对齐
  `Sector 5 (offsets
  A00...C00)` 与
  `Sector 117 (offsets EA00...EC00)`，两个损坏点重建出的 `fixed`
  与官方同样逐字节还原）。修不动（同一 parity 组内两个坏扇区）时不再
  中止，而是逐条报 `cannot recover data`，再按官方询问
  `Reconstruct archive structure ? [Y]es, [N]o`（`output::confirm`； 静默 `-idq`
  不询问、按 Yes 重建；`N`/读不到答案则只留报告）→ 归档走**退出码 3**（官方此路
  3，与无记录重建的 0 不同）。库侧 API 由 `bool` 改为
  `LegacyRepair`/`LegacyDamagedSector` 报告。契约由
  `cli_repair_recovers_damage_in_the_records_final_sector` /
  `cli_repair_asks_before_rebuilding_after_an_unusable_record` 与 legacy 单测
  （尾部扇区检测+修复、同组双坏扇区报 unrecovered）钉住。
- `-rr` 三种形式的语义对齐官方（2026-09-22 官方 6.23 实测）：**裸 `-rr<N>` 是
  legacy RAR4 的 parity 扇区数（计数）**——`-rr10` 在任何尺寸下都写出恰好 10 个
  扇区；`-rr<N>%` 是受保护前缀的百分比；**裸 `-rr` 是官方默认 3%**（不是我们此前
  假设的 10%）。此前 CLI 把三种形式全映射成百分比，于是 `-rr10` 在 400 KB 上变成
  78 个扇区（官方的 7.8 倍）。RAR5 的记录只按百分比定尺（实测官方 `-rr10` 与
  `-rr10%` 产出的记录**同尺寸**），因此裸数字在 RAR5 上按百分比解释。库侧新增
  `WriterOptions::recovery_sectors(u32)`（与 `recovery_percent` 互斥；RAR5 与
  RAR13 拒绝计数而不是静默丢弃），CLI 新增 `--recovery-sectors`。契约由
  `legacy_rar4_recovery_record_follows_the_rr_forms`（CLI，含 `rec_sectors`
  解析）与
  `recovery_sector_count_is_exact_and_legacy_only`（库，含两种非法组合） 钉住。
- **CLI 开关面审计（2026-09-22，对拍官方 7.23）**：用「官方开关表 × `l`/`t`/`a`
  三种命令」做 240 例差分扫描，并在源码里找「解析了但从不读取」的字段（169 个
  clap 字段中 14 个）。结论：**开关面本身齐全**（那 3 个"缺失"是
  `-ht`/`-id`/`-o` 的变体 造成的假象；14 个未读取字段里绝大多数是 CLI.md
  已记录的"接受但无操作"），差的是一类
  **解析宽松度**——官方把任何已知开关挂任何命令都接受、不适用的**默默忽略**（实测
  `l -m5`/`l -o+`/`l -kb`/`l -c-`/`t -rr10` 全 exit 0；只有真未知开关才 exit
  7）， 而我们按命令声明、不适用的直接报 exit 7（扫描里读侧约 45
  例）。现按官方行为修正： `switches_after_command`
  在重排之后**无条件**过滤——先把 WinRAR 写法翻译成 `--long`，
  再丢掉目标子命令未声明的开关（允许集 = 该子命令自身参数 + 根级 `global`
  开关）。这是 **本
  CLI「绝不静默丢弃开关」规则的唯一例外**，否则就无法与官方兼容；故意拒绝的
  `-dr`/`-dw`/`-vd` 仍声明在会生效的命令上，因此仍走到显式拒绝；clap
  之前消费的内部 标记（裸 `-p` → `--password-prompt`，`reject_bare_password`
  靠它拒绝明文归档）显式 豁免，否则那条安全检查会被丢掉。同时修
  **`-v-`**：此前被当成 `--volume-size=-` 报错， 现映射为 `--no-volumes`（与
  `--volume-size` 互为 `overrides_with`，实现「后写者胜」）。 实测遗留（已写进
  `docs/CLI.md`）：`-idn`（官方列表不打印成员名）仍不生效、`-idc` 恒
  等效（我们不打印官方版权/Trial 横幅）、`-iver` 文案与官方不同。契约由
  `cli_irrelevant_official_switches_are_ignored_like_winrar`（含「未知开关仍
  exit 7」） 与 `cli_v_minus_cancels_volume_creation` 钉住。
- **库 API 对称性 +
  可运行示例（2026-09-22）**：审计发现唯一的真设计缺口是**线程配置
  不对称**——`WriterOptions::threads` 是 per-archive，而抽取端只能在**进程全局**
  `set_extraction_threads` 上设（`ExtractOptions`
  根本没有该字段），同一进程两个消费者
  会互相覆盖。现补齐对称：`ExtractOptions::threads`（`None` → 全局 →
  自动；`Some(0)`
  跳过全局、直接自动），抽取池**按线程数缓存**（原先是一个池、按需重建，会在两个不同
  线程数的并发抽取间抖动），解析规则与压缩侧同构并有单测
  （`extraction_run_threads_win_over_the_global_override`）；CLI 的 `-mt`
  改为逐次下发
  （`ExtractRequest::options()`），不再改全局。另补两个**可运行示例**
  （`examples/create_and_extract.rs`、`examples/edit_and_repair.rs`）——此前
  `examples/` 只有 bench/probe，新用户没有普通用法的样板；README 现在指向它们。
  契约由 `extract_options_threads_drive_the_parallel_path`（≥4 成员、≥64 MiB
  解开量 走批量路径，`threads=Some(2)` 与 `Some(0)` 都逐字节校验）钉住。
- **绑定（rar-napi）对齐 WinRAR + API
  补全（2026-09-23）**：`ExtractArchiveOptions` 此前无逐次线程、无
  freshen/update、无大小上限（且注释谎称有「绑定自己的线程池」——
  实际只有全局默认）。现补 `threads`（逐次 `-mt`，与写侧 `threads` 命名对称）、
  `freshen`/`update`（`-f`/`-u`）、`maxUnpackedBytes`/`maxTotalUnpackedBytes`（未设或
  0 = 不限，磁盘抽取仍是流式）；`CreateArchiveOptions` 补
  `recoverySectors`（legacy RAR4 的 `-rr<N>` 精确扇区数，RAR5
  记录只按百分比定尺，故被拒）。契约由
  `extractArchive honors freshen and update (-f/-u)`、
  `extractArchive enforces the size limits and threads option` 与
  `createArchive recoverySectors is the legacy RAR4 sector count` 三个 JS
  用例钉住。
- **库 API：抽取设置归位 + 文档闸门（2026-09-22）**：审计发现两处可改。①
  `ArchiveReader` 上的 `set_*` 里，**MOTW 本来就是逐次抽取的策略数据**（CLI
  也只是把它 放进请求再推给 reader），现已移入
  `ExtractOptions::mark_web`，`set_mark_of_the_web` 与 `ReadState.motw`
  一并删除——抽取入口本来就把 `opts` 存进 `read_ctx.extract_options`，
  传播处直接读它，连参数都不用加。代价是 **`ExtractOptions` 放弃
  `Copy`**（`MarkOfTheWeb` 带 `Vec<String>` 扩展名过滤），这也正好与
  `WriterOptions`（Clone 不 Copy）对齐；受影响的 调用点改 `.clone()`（库内 5
  处 + 测试 5 处），语义不变。② 加 **`#![warn(missing_docs)]`**： 一次性补齐 74
  处公开项文档（`ErrorCode` 16 个变体、`RarError`/`Result` 别名、`FileHeader`
  /`DataChunk` 字段、codec 常量、`EncryptionParams`、`FeatureSet`、恢复记录 CRC
  辅助函数、 parallel 门控下的
  `encode_chunked_mt`/`encode_with_filters_mt`），默认与 `parallel` 两种 feature
  配置都零缺口，并由 clippy `-D warnings` 长期强制执行。契约：`CONTEXT.md` 的
  MOTW 词条与 `docs/CLI.md` 已同步（`-om` 现在是 `ExtractOptions::mark_web`）。
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

- **分卷 + 内联恢复记录（`-rr`）**：**已实现**（2026-09-24，创建 + `rar rr` +
  分卷编辑，RAR5 与 legacy RAR4 同形），结论见「已修」；此前的拒绝与 依据（6.23
  的挂死）已作废。
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

- **RAR4 成员头的三个字节字段未对齐官方**（2026-09-24 对拍 6.23）：`-ma4 -m0`
  单卷/多卷逐字节对拍后，除下列 6 字节外**全部相同**（卷尺寸、载荷、20 字节
  `ENDARC`、`.rev` 均一致）：① 主头 `HEAD_FLAGS` 我们多置
  `LONG_BLOCK`（`0x8000`）， 官方不置（`-ma4` 单卷 `0x0000`、分卷 `0x0111`）；②
  FILE_HEAD 窗口位 （flags 位 5–7）我们恒写 6（4 MiB），官方按成员定尺——store 恒
  1（128 KiB）、 压缩为 `min(4MiB, max(128KiB, next_pow2(size)))`（实测 5 K/50
  K→1、500 K→3、 5 M/50 M→6）；③ FILE_HEAD `unp_ver` 我们按容器写 29，官方对
  `-m0`（请求 level 0）写 20（即便成员因不可压而 store，只要请求 level ≥3 仍是
  29）。三处是 **纯字节外观**差异：双向读写正常、官方 `t`/`rc`
  无碍，且是**既有**行为（与分卷 无关，单卷同样如此，本轮未改动）。对齐 ②
  需要改成员发射器的字典口径（solid
  链尤需谨慎，声明过小会损坏解码），属独立一轮，故按「已知小差异」记录，不改
  代码。

- **`-rr<N>%` 的百分比取整**：官方 6.23 的百分比形式不是干净的 P%。已量清的结构
  （细扫 20–100 KB、步进 4096 字节）：**斜率精确等于 P%**（边界严格相隔 5120
  字节 @P=10），偏差是**扇区上的加性常数** 且随尺寸缓慢变化——+1（20–150 KB）→
  0（200–300 KB）→ −1（400 KB）→ −2 （500 KB）；同一 500062 前缀跑
  P=1/3/5/10/20/30/50，`floor − rec` **恒为 2**
  （证明是加性而非乘性）。**没有单一闭式**：`floor/ceil/round × (prefix + K)` 全
  K 扫描无解；刚性格点要求唯一边界相位，但 32832 与 53312 同余 mod 5120
  却给出不同
  偏移；不动点（把记录自身大小算进基准）会发散。基准已确认是「记录之前的前缀」。
  **我们的规则已定稿：`max(2, ceil(prefix*P/(100*512)))`——只向上取整，绝不向下**
  （记录保护的字节数不得少于请求的百分比），不照抄官方那套无闭式的取整。同一实测表
  （前缀→官方 parity 扇区，P=10）：20544→5、32832→8、50067→11、100068→20、
  150068→30、200068→39、300068→58、500069→95；**我们给 5/7/10/20/30/40/59/98**：
  中档（100–150 KB）与官方**完全一致**（`-rr10%` 与裸 `-rr` 在 150 KB 上都是
  30/9， 逐值相等），小档差 ≤1（我们更多），大档我们高于官方（400 KB +2、500 KB
  +3）——符合「宁可多给不可少给」。契约由 `percent_recovery_count_rounds_up`
  钉住。裸 `-rr<N>`（计数）不受影响，精确对齐。
- **`rar rr` 命令只在 3% 上写记录**：官方 6.23/7.23 实测（22 组）——该命令对
  `-rr10` / `-rr10%` / `-rr20` / 裸 `-rr`、尾随位置参数、以及**归档里已有的 20%
  记录**一律无视，总是写出 **3%** 记录（开关放命令前后都一样）。我们按「静默丢弃
  用户请求」的缺陷处理：**显式强度照样生效**，而且与 `a` 同一套口径—— `-rr20%` =
  20%、`-rr20` = legacy RAR4 的 **20 个 parity 扇区**（RAR5 同样读作
  20%，因为它的记录只按百分比定尺）、裸 `-rr` 或什么都不给 = 官方默认 **3%**；
  另有我们的扩展形式：尾随参数（`rar rr <archive> 20` / `20%`）恒为百分比。
  **默认值已从 10% 对齐到 3%**。契约由
  `legacy_rr_command_honors_the_requested_strength` 钉住。
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
  一致，且与记录是谁写的无关）。我们的 parity 组与官方一致（含那个尾部扇区），
  差别在**写回范围**：只写回落在前缀里的那部分字节，记录自身字节永不改写，故能
  逐字节修复同样损坏。**这是 WinRAR 侧缺陷，不追平**；互操作测试因此用伪随机成员
  数据。
- **RAR5 元数据：两台平台的风格都已对齐（2026-09-23）**。WinRAR
  按宿主平台写元数据 —— Linux 上 `host_os`=1、Unix `st_mode`、FILE_TIME 记录
  flags 0x13；Windows 上 `host_os`=0、DOS 属性（1 字节）、FILE_TIME 记 Windows
  FILETIME（flags 0x02，头里 不写 4 字节
  mtime）。**我们也按平台写了**（见「已修」），故不再有属性丢失/名字归一化
  的差异；成员头与 QO/RR/CMT 服务块的 `data_size`/`unpacked_size`/`comp_info`
  也已补到官方的最小 2 字节、STM
  的预留宽度也对齐（见「已修」）。**仍差的两处**：①主头 locator
  的**偏移字段宽度**——官方按「写主头时对最终大小的估计」预留（实测：3/4/5/6
  字节，阈值 2^9 / 2^16 / 2^23；更大的归档继续变宽，1 GiB 档实测 9 字节；QO 与
  RR 同样宽、无记录时 QO 写 0）。现提供 `WriterOptions::estimated_size(bytes)`
  （及 `CreateOptions::estimated_size`）：给出预计大小即按官方分档预留，**`-m0`
  小归档与官方逐字节相同**；不给则沿用历史定长 5 字节（35 位，超 32 GiB 写哨兵
  0）⇒ 默认仍比官方 **+2 字节**（最小档）到 **−1 字节**（≥256 MiB 档）。**CLI
  未接线**：官方的估计是内部经验式（按成员累加，含成员名字长度、且带 16 字节量级
  的取整与饱和），无法由我们自身的头字节推出，故 CLI 不自动填。locator 本身（恒
  写、QO 占位、主头块 flags 0x5）已与官方一致。②**官方默认就写 QO 记录**
  （2026-09-23 实测 7.23 的 console `a`）： 归档越大越会写——实测 ~4 KB
  输入不写（QO 偏移 0）、8 KB 起写（QO 偏移 8056）， 我们只按 `-qo` 写（CLI
  默认不写，`docs/CLI.md` 的“console 默认不写 QO”只对
  小归档成立）。只影响字节（官方多一条 QO 服务块），不影响读取（无 QO 时双方都
  回退全扫）。**只影响字节 外观**：双向读写一致，interop 与语料对拍全绿。契约由
  `winrar_interop::scenarios`（`windows_metadata_round_trips_through_winrar` /
  `varied_corpus_round_trips_through_both_tools`）钉住。
- **`-ts` 的「字母+数字」组合**（2026-09-23 对拍官方 7.23 Linux 构建）：官方只认
  `-ts`/`-tsm`/`-tsc`/`-tsa`/`-ts1`/`-tsm1` 等少量形式——实测
  `-tsc2`/`-tsc3`/`-tsa2`/`-tsa3`/`-ts2`/`-ts3` 一律**静默退化成仅 mtime**（用户
  点名的 ctime/atime 被丢掉），而 `-tsc1`/`-tsa1` 又不做秒截断（保留
  ns）。我们按 文档语义统一处理（`<种类><精度>`，`1`
  对**已选**时间做秒截断），故 `-tsc1`/`-tsa1` 与官方差一个 ns
  位，`-tsc2`/`-ts3` 之类我们仍按请求存
  ctime/atime。按「静默丢弃用户请求＝缺陷，不照抄」的既有口径处理，未追平。
- **Windows 属性的位子集**（2026-09-23 对拍官方 7.23）：`platform.rs` 的
  `STORED_DOS_ATTRIBUTES` 只映射只读/隐藏/系统/目录/归档/重解析点六位，于是从
  目录继承了「内容未索引」（`FILE_ATTRIBUTE_NOT_CONTENT_INDEXED`，`0x2000`）的
  文件，我们属性字段写 `0x20`、官方写 `0x2020`（`A0 40`），解出后我们丢该位、
  官方保留。官方显然照抄了更多 Windows 位；逐位对齐需先测清它的取舍。属有意
  子集，未对齐。
- **Windows 联接点的 redirect 目标字符串**（2026-09-23 对拍官方 7.23）：`-ol` 存
  junction 时我们写其原始路径（`C:\dir\target`，反斜杠），WinRAR 写 NT 打印名、
  正斜杠、带 `/??/`
  前缀（`/??/C:/dir/target`）。两者的默认抽取行为一致——绝对目标
  都被安全策略**拒绝**（见上条 exit 1）；差别只在
  `-ola`（信任档）下：我们的形式能被 我们重建回真 junction，官方那套 `/??/`
  形式在 `\??\` 之外未必成立。属字节与信任档 下的边角差异，未对齐。

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
- **Windows 上绑定 crate（`crates/rar-napi`）必须用 MSVC 目标构建**：Node 是
  MSVC 构建的，`napi-build` 在 windows+msvc 下什么都不做，而 windows+gnu
  那条路要求 `LIBNODE_PATH`/`LIBPATH`/`PATH` 里存在一个**没有发行版会带**的
  `libnode.dll` （缺了就 `libnode.dll not found in any search path`
  panic）。这台机 rustup 默认 已是 `stable-x86_64-pc-windows-msvc`，所以裸
  `cargo build` 覆盖全 workspace ✓； 默认若是 GNU，则须
  `--target x86_64-pc-windows-msvc`（或 `rustup default` 切换）。 Linux/macOS
  无需任何设置（故 CI 的 `--workspace` 能过）；wasm 目标另需 `napi build` 注入的
  `EMNAPI_LINK_DIR`。详见 Cargo.toml 注释与
  [`docs/testing.md`](docs/testing.md)。
- **Markdown 由 `dprint` 格式化、正文 80 列**：改完跑 `npx dprint@0.50.2 fmt`
  （配置 `dprint.json`，约定见 [`docs/README.md`](docs/README.md)）。
