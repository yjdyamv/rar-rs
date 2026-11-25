# 不变量与地雷（改代码前先看）

> 最后核对：2026-11-25 @ `6c6803d8`；实现细节以源码为准。

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
  `data_size`/`unpacked_size`/`comp_info` 一律 ≥2 字节（成员头在大成员上还会额外
  分档预留，见下一条）；STM 服务块按 `vint_size(unpacked_size << 12)` 预留（官方
  先写头后回填，故更宽）。契约 `member_size_fields_use_a_two_byte_minimum` /
  `stream_block_size_fields_use_the_reserved_width`。
- **成员头的 `data_size`/`unpacked_size` 宽度按成员总大小分档预留**：
  成员总大小超过阈值后官方不再写最小宽度，而是**预留**（实测 7.23： < 7 680 →
  最小宽度（下限 2 字节）、7 680–1 048 063 → 3 字节、 1 048 064–1 048 575 → 4
  字节、1 048 576–8 388 607 → 5 字节、 ≥8 388 608 → 6 字节，256 MiB 仍是 6）。3
  字节那一档最容易漏（阈值 `2^13 - 512`）：7.5 KiB 的成员头因此比按值算宽 2
  字节，后面每个成员都偏移， 分卷归档会整体错位。规律按成员 **unpacked**
  总大小跳档（8 MiB 成员压到 4 MiB 仍预留 6 字节），
  且**单卷成员同样预留**；分卷时每个卷都重复成员总大小，两个尺寸字段共用这一个
  宽度（而不是本卷数据块自己的宽度：200 000 字节成员按 32 KiB 分卷，末卷 3 793
  字节的数据块仍写在 3 字节里），按本卷大小写最小宽度会让头短 1 字节。契约
  `member_size_fields_use_the_reserved_width_tiers` /
  `rar5_size_fields_follow_the_official_reservation_tiers` /
  `multivolume_sets_are_byte_identical_to_official_rar`。
- **主头 locator：默认（automatic）模式总会预留，只有 `-qo-` 才整条不写**。
  官方默认（不带 `-qo`）的主头**始终**带 `LOCATOR` 记录：没有 QO 记录时两个
  偏移都写 0（3 字节字段，`05 01 01 80 80 00`），而 `-qo-` 的主头缩到
  `type/flags/归档 flags` 三个字节。我们按「`quick_open`（含 automatic）或 `-rr`
  才写 locator」实现。写了 locator 时 QO 标志先置、偏移写 0 占位，关闭时
  回填。**有 QO 与实际偏移无关的判断只看偏移非零，不看标志位**（只看标志会凭空
  重建 QO 记录、破坏前缀逐字节不变）。契约
  `locator_is_present_with_a_zero_qo_placeholder` /
  `no_locator_without_quick_open_or_recovery` /
  `locator_parity`（`tests/format_parity.rs`）。
- **locator 偏移字段的宽度按「输入总大小」分档**：官方用 `locator_offset_width`
  那张表（< 512 → 3、< 64 KiB → 4、< 8 MiB → 5、否则 6），但喂进去的估算值是
  **成员原始大小之和**，不是压缩后的大小、也不是卷大小：600 字节输入压到 103
  字节仍用 4 字节字段，100 000 字节输入压到 161 字节仍用 5 字节，200 000 字节
  输入按 20 KiB 分卷仍是 5 字节（按卷大小算只会给 4）。CLI 在 `create` 时把
  文件大小求和后交给 `WriterOptions::estimated_size`（压缩只会变小，故输入总量
  是安全上界；每档宽度还留了约 32 倍余量，估小了也不会溢出，真放不下时
  `patch_locator_fields` 会把偏移降级成 0＝「没有记录」）。
- **QO 服务块没有 `SUBDATA` extra 记录**：官方 7.23 的 `QO` 块 flags 是
  `0x06`（`DATA_AREA|SKIP_IF_UNKNOWN`）、extra 区为空，载荷就是 data area；`RR`
  块相反，`0x07` + extra 里的百分比字节。多发一条空 `SUBDATA` 记录（`type`
  only）会让每个归档多 3 字节且 flags 不同。
- **QO 条目 CRC 覆盖「体长 vint + 体」**：官方 7.23 的条目是
  `[crc32][body size vint][body]`，校验和从 body size 那个字节算起；只覆盖 body
  会让官方读到校验失败。契约 `decode_qo`（`tests/format_parity.rs`）按官方口径
  校验，字节级对拍才能一致。
- **服务块（QO/RR/CMT）也写写方 host_os**：官方按自己运行的平台填，Windows 是
  0、Unix 是 1；写死 `OS_UNIX` 会让 Windows 官方产出的归档在服务块里差一字节
  （`locator_width_follows_official_size_tiers` 就是这么发现的）。用
  `crate::platform::host_os()`。
- **QO 记录在编辑/追加时整体重建，不是原样拷贝**：记录里存的是每个成员头
  的副本与「回退到该头的相对偏移」，所以删除/改名/注释/`rr` 之后偏移必须重算。
  编辑走 `transaction`（保留原有「有无
  QO」的判定：`had_qo &&
  !header_encryption`，`-hp`
  归档因此不带记录），追加先截断尾部的 QO/RR 再按 现有+新增成员重写，关闭时回填
  locator。契约 `edits_rebuild_the_quick_open_record` /
  `edits_do_not_invent_a_quick_open_record`。注意我们总是缓存**全部**成员头，
  而官方默认只缓存相对大的文件，故编辑官方归档后记录会变大（合法但有别）。
- **`-qo` 裸写法＝官方默认的自动规则**：成员数据块 > 4096 字节才写 QO 记录
  （实测 4096 不写、4097 写），`-qo+` 强制每卷都写、`-qo-` 关掉。CLI 把裸 `-qo`
  映射成 `--quick-open-auto`、`-qo+` 映射成 `--quick-open`；
  `WriterOptions::quick_open(true)` 对应 `-qo+`，与官方对拍时必须让官方也带
  `-qo+`，否则官方在几十 KB 的归档上根本不写 QO 记录。自动模式下主头仍预留
  locator（偏移 0），所以「小归档没有记录但有 locator」是正常形态。 RAR4/RAR13
  只能报错（`-ma4 -qo` 走 `quick_open || quick_open_auto` 的矩阵被
  `validate_rar4_only` 拒掉）。
- **自动规则要在每条发射路径上打点**：RAR5 有三处把成员数据写进当前卷，每处都
  必须先调 `mark_quick_open_data`（`emit::write_file_entry` 的单卷分支、
  `emit::write_split_member` 的每块预算之前、`stream::write_streamed_payload` 的
  STORE 单卷分支），漏一处就出现「CLI 建出来有 QO 记录、库 API 建出来没有」
  这种同参数不同字节的诡异现象（STORE 走的是 `stream` 而不是
  `emit`，最容易漏）。 分卷路径要在 `volume_tail_reserve`
  之前打点，否则挣来的记录没进卷预算、 写完才溢出。契约
  `quick_open_automatic_matches_official`。
- **`-qo` + 分卷：每卷各带一份目录**。官方 7.23 跑 `-ma5 -m0 -v32k -qo+`
  时**每卷各写一个 QO 服务块**（紧贴 ENDARC 之前，单成员时头 14 字节 + 载荷 50
  字节），并在**每卷主头**的 extra 区加一条 `LOCATOR` 记录；`-qo-`
  两样都没有。条目布局与单卷完全一致，`rel` 仍是「从本卷 QO
  块起点回退到本卷那份分片头」的距离（实测 part1 = 32683−25 = 32658，part2 =
  32683−26 = 32657），即分卷里每个分片头各自
  生成一条条目，而不是全档共用一个绝对偏移。**我们照这个布局写**：
  `record_quick_open_entry` 在每卷写头时就地记下该头相对本卷 QO 位置的
  距离，卷尾 `write_quick_open_record` 打包；跨卷成员在下一卷开新头的
  情形（同一卷里两个分片头）自然得到两条条目。预算上，卷尾要把 `ENDARC` + QO
  块的最大长度一起算进 `quick_open_volume_reserve`， 并在 QO 放不下时整卷不写
  QO（宁可少一份目录也不溢出）。契约
  `quick_open_volumes_cache_each_volume_headers_in_that_volume`、
  `official_unrar_validates_quick_open_volumes`。官方每卷在 ENDARC 后预留 8
  字节松弛（与 `-rr` + `-v` 同一类），我们的卷尾预算同样计入并把非末卷补零到
  `V`，故逐字节一致（而非「填满不留」）。
- **`quick_open` + `volume_size` 不再互斥**：`validate_combinations` 里原先
  的拒绝项已删除，`header_encrypted` + `quick_open` 仍然互斥（`-hp` 没有主头可放
  locator）。
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
  `0x20`/`0x10`——即 RAR4 的 `host_os`/`attr` 是**写死 Windows 风格**的，而官方
  Linux 版写 `host_os 3` + `st_mode`。所以 Linux 上的 RAR4 对拍必须遮掉这三处
  （`host_os`、`attr` 与覆盖它们的 file header CRC），其余字节仍逐字节相同；
  契约 `only_the_host_fields_may_follow_the_platform`（任意宿主上都跑：把官方头
  改写成 Unix 形状，再验证遮罩范围既够又不多）。RAR5 两侧都按平台写，不用遮。
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
- **RAR5 的 parity 按 64 KiB 分块写，每块各带一个完整头**：一个 recovery shard
  的奇偶数据不是一整块，而是切成 `blocks = ceil(group_count / 65536)`
  个块，**每块各写一条 `{RB}` 头**（`header_size` 字节，含它自己那份
  `data_shard_states` 表），块内 `0x16` 字段是该块在 shard 内的字节偏移、
  末块的数据长度按真实末 shard 长度 clamp。故
  `shard_size = group_count + blocks * header_size`（不是
  `header_size + group_count`）。少分块会让 WinRAR 把记录长度算错：3.5 GB 归档
  5% 记录被显示成 `Recovery record 395 TB`，官方 `rar r` 报
  `cannot recover data`；官方 `t` 仍能通过，所以只有修复路径能暴露。
- **`data_shard_states` 是逐块算的，不是逐 shard**：第 `b` 块的 `state[i]` =
  `crc64(prefix[i*gc + b*65536 .. min(i*gc + (b+1)*65536, (i+1)*gc, prot)])`，
  **init 为 0、块间不链式**、末 shard 按 `protected_size` 截断（不补零）。
  每个块只能写自己那 64 KiB 切片的表；把整 shard 的 CRC 抄进每个块，
  头部长度仍对但官方修复会退化（`0 blocks are recovered`）。历史上的单块旧
  布局（一块覆盖整个 shard）仍可读，解析侧以 `legacy_whole_shard` 分支兼容。
  头末 8 字节（`header_size - 8`）官方是**随机 nonce**（`Rar7.exe` 里由
  `CryptGenRandom` 填充，已逆向定论），故官方 `-rr` 产物本就不确定、不可能逐字节
  对拍；我们写确定的 `crc64_rar_state(parity[0])`，两侧修复路径都忽略该字段。
- **`-rr` 配 `-v` 时每卷各带一份内联记录**，每卷只保护自己的前缀；卷预算按
  `recovery_volume_reserve(prefix_len)`
  的几何长度做**候选前缀二分**（直接按最大记录收窄每卷会白扔约 2 KB）。
  预算里还得算上成员头**按大小分档预留的尺寸字段**：跨 1 MiB 档的成员每卷多 3
  字节头，漏算就溢出一卷（`recovery_volume_budgets_count_the_reserved_size_fields`
  用 1.5 MB 成员 + 256 KiB 卷钉住「非末卷恰好填满请求的尺寸」与
  `data_width =
  5`）。
- **`rc` 重建卷沿用数据卷自己的零填充宽度**（`.rev`
  的宽度是恢复卷号，两者位数可不同），没有现存数据卷时才用 `layout.width` 兜底。
- **加密块 padding 是 zero-fill，不是 PKCS7**（7-Zip 会校验 padding 区全零）。
- **卷大小必须精确**：新增块类型（如 QO）记得同步配额记账。
- **x86 滤镜（E8/E8E9）的减法必须照旧回绕，不要加 `addr >= offset` 之类的
  守卫**：官方流里确实存在操作数小于 `offset` 的情况，官方的逆变换同样回绕。加
  守卫会把那些字节原样留下、**解坏真实归档**——实测官方 7.23 `-m3` 压 80 MiB 的
  `MRT.exe`，加守卫后回环 CRC 报错（`0x775C0113` vs `0xC449A434`），去掉即恢复。
  滤镜其余语义与官方逐条一致（操作码 `0xE8`、`!e8_only` 时加 `0xE9`；操作数位置
  `(CurPos + FileOffset) % 0x1000000`；命中跳 5、否则跳 1；5 字节下界），且它是
  `(缓冲区, 绝对起点, 长度)`
  的**纯函数**，所以**区域可任意重切**：流式窗口切出的 80/128
  字节碎片仍字节精确。锚点与取证见
  [`issues/rar5-decode/`](issues/rar5-decode/official-decode-path.md)。
- **过滤器类型 4–7 是保留值，必须显式报错而不是静默跳过**：盘面字段 3 位（0–7），
  但只有 0 Delta / 1 E8 / 2 E8E9 / 3 ARM 会被任何写入器产出（WinRAR 7.30/5.50/5.00、
  7-Zip 实测止于 3；ARM 自 5.80 起停用）。unrar 给 4–7 的那些名字是**编码器选型
  id、不是盘面语义**，也没有任何工具能产出带 4–7 的档供对拍。解码遇到 4–7 必须
  `Err`（`apply_filter_decode_in_place`）——当"无过滤"返回只会解出坏字节，以 CRC
  不匹配这种难查的方式炸。写入端同样拒写（`is_supported_filter_type`）。

## 有意不追平官方

- **`-rr<N>%` 的取整口径**：官方那套取整没有闭式（斜率等于
  P%，偏差是扇区上的加性常数且随尺寸变），我们的规则是
  `max(2, ceil(prefix*P/(100*512)))`——只向上取整、绝不少于请求的百分比。裸
  `-rr<N>`（计数）精确对齐。
- **带 `-rr` 的分卷：卷尾预算 + 零填充已对齐**。官方 7.23 先为记录预留
  （`FUN_14007de58` 的 `term`），非末卷再按「gap ≤ max(V/128, 1024) 就补零到
  `V`」填满；我们复刻了同一预算（`volume_tail_reserve` 的 `volume_term`）与补零
  规则（`pad_volume_tail`），故非末卷**逐字节一致**。唯一差别是 `{RB}` 头的随机
  nonce（见上），官方侧本就不确定。`.rev` 编码本身逐字节一致
  （`rar5_recovery_volumes_match_winrar_byte_for_byte`）。
- **`rar rr` 只在 3% 上写记录是官方缺陷**：6.23/7.23 无视
  `-rr10`/`-rr20`/已有记录一律写 3%。我们**显式强度照样生效**（与 `a`
  同一套单位），裸 `-rr` 才 3%。
- **0% 等于「不写记录」，两条记录都没有 0 这个档位**：RAR5 至少一个 shard、 RAR4
  至少两个扇区，把 0 传下去就会写出没人要的记录（1 KiB 归档凭空多 651
  字节），超过 200 KiB 时还会因 shard 数算出 0 而在关闭时报
  `RAR 5 recovery shard count is invalid`。归一化点在
  `Archive::new_with_options`（所有写入口共用的漏斗），CLI 的
  `--recovery-percent` 也只接 1..=100。编辑面无法表达「删掉记录」（缺省语义是
  「沿用归档自己的强度」），`SetRecovery(0)` 直接拒绝。契约
  `zero_percent_recovery_writes_no_record_in_either_format`。
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
- **不可压探测的门槛不能下调**：官方对**小**成员是真的压一遍再决定（1 MiB 纯随机
  → STORE，1 MiB 随机夹 8 KiB 零 → 压小 5.3 KB 照压），只在更大成员上先用抽样
  判“不可压”（3 MiB 随机夹 8 KiB 零 → 官方直接 STORE，没找到那块零）。我们的
  `SAMPLE_PROBE_MIN_INPUT` 就是 2 MiB 这条线：低于它的成员不探测、老老实实压。
  把 legacy 门槛降到 768 KiB（省下 1 MiB 随机成员的整轮 LZ+PPMd 试压，实测 10×1
  MiB 随机 solid m5 从 17 s 降到 0.5 s）会让“1 MiB 随机夹小块可压数据”的成员被
  我们 STORE、被官方压小——2026-10-27 因此没有采纳。
- **抽样探测看不见“远处的重复块”，双方都不完美**：成员主体是随机数据、只把一段
  字节在后文原样重复时，判语完全取决于那段重复落在哪里。实测（8 MiB 以内、
  `-ma5`）：“2 MiB 随机头 + 4 MiB 随机尾 + 头再来一遍”官方 `-m1` 直接 STORE
  （100%），我们 `-m1` 照压 75.04%；“128 KiB 随机头 + 4 MiB 随机尾 + 头再来一遍”
  反过来——官方 `-m2` 压到 97.24%，我们 STORE 100%（重复块在抽样点之外，
  `SAMPLE_REPEAT_MIN_MATCH` 的远重复逃生口只在**抽样区间内**找重复）。两个方向
  各输一次，量级都在 3pp 以内；要改就得让逃生口扫全成员（多一遍 O(n) I/O），
  目前不动。
- **RAR4 的 `-m5` 会比官方小一大截，这是对的**：6.23 的 RAR4 写入器根本 不产
  PPMd，所以同为 `-m5`，30 MB 文本官方出 4 237 187 B（纯 LZ）、我们出 2 734 467
  B（PPMd 胜）；纯 LZ 那条线双方同档（`-m3` 30 MB：5.7 s vs 5.9 s，
  尺寸也只差几个百分点）。别把“和官方不一样”当 bug 去砍 PPMd——读取侧 6.23/7.23
  都能解，这是 RAR4 唯一能拿到 PPMd 比率的路径 （见
  `docs/rar4-creation-spec.md`）。代价是 PPMd 试压比 LZ 慢约 4 倍 （30 MB
  文本、`-mt1`、匹配查找器优化后：18.9 s vs 5.0 s）。
- **solid 成员的顺序是官方的分组规则，不是参数顺序**：`-s` 且压缩时（`-m0` 与
  `-ds` 除外），官方先把成员按**扩展名**分组（大小写不敏感、无扩展名在前、 `a.`
  这类尾点算无扩展名、`a.tar.gz` 取 `gz`），组内按**基本名**（不含目录
  前缀）升序、同基本名再按**大小**升序、最后按全名；目录条目全部排在文件之后，
  且**按深度降序**（`a/b/c` 在 `a/b` 之前，同深度仍按上面的键）。`update`/
  `freshen` 追加的成员同样按扩展名键排序（`-m0`、非 solid 也一样），但**不看
  `rarfiles.lst`**（实测）。契约：`cli_solid_members_follow_winrars_group_order`、
  `cli_stored_and_no_sort_keep_the_input_order`、`cli_update_appends_members_in_winrars_order`。
  已知残余差异：同一文件既作为显式参数、又落在目录参数里时，官方按 `-o+` 的
  覆盖语义把它**移到末尾**，我们保留原位。
- **目录成员不带尾分隔符**：RAR5 用 `FILE_FLAG_DIRECTORY`、RAR4 用 `0x10`
  属性位标记目录，名字本身就是 `sub`（不是 `sub/`）。我们旧版 RAR5 写入器补了
  `/`，`lb` 会打出 `sub\`；2026-10-27 修正，`rename_entries` 对源名字带斜杠的
  外来归档保持原样（不强行归一）。契约：
  `cli_directory_members_carry_no_trailing_separator`。
- **`-idn` 仍不生效、`-idc` 恒等效**（我们不打印官方版权/Trial 横幅）、`-iver`
  文案与官方不同；`rar r` 对载荷损坏的处置见上。

## RAR4 读取

- **目录标志与 host 无关**：成员是目录，要么 `file_attr & 0x10`（host 0/1/2/4 的
  DOS 位），要么高位属性字是 Unix 目录模式（host 3/5）——两者是**并集**，不按 host
  分流。旧的 host 二选一实现把 OS/2/Mac 的目录读成 0 字节文件；UnRAR 6.23 对
  host=0..4 一律按并集判定。契约 `directory_flag_is_host_independent`。
- **`lt` 的 `Details:` 对 legacy 容器一律 `RAR 4`**（不论成员 `unp_ver`：`-ma15`
  的 v15 成员也是 `RAR 4`；RAR13 是 `RAR 1.4`、RAR5 是 `RAR 5`）——UnRAR 6.23
  实测。旧代码对 legacy 打 `RAR 1.5`。
- **`-tsc`/`-tsa` 的写与读**：写入把 ctime/atime 放进 `FHD_EXTTIME`——每字段
  `PRESENT|ADD_SECOND|3 个亚秒字节`；mtime nibble 无内容（偶秒且无小数）时缺席，
  ctime/atime 只要请求了就 present（自带 4 字节 DOS 时间）。文件成员与
  `-ma4 -tsc -tsa` 官方输出**逐字节一致**（实测 `f0ff80d612…`）。读取解回模型的
  ctime/atime（解包会恢复）。目录同样带，但 atime 取的是**枚举之后**的值（列目录
  本身会刷 atime）。

## 提取与安全

- **固实链里的滤波记录是成员内坐标**：解码器在 `apply_complete_filters` 用
  `block_start - member_start` 换算，因此同一链里可以有任意多个滤波成员，
  分块与跨卷都不影响；编码端只能在写数据前决策（共享 `EncoderState` 不能回滚），
  所以 `solid_filter_specs` 必须先探测后编码，且 `-mc` 策略要在探测前生效。
  `add_bytes` 走 `add_bytes_rar5`，不试滤波——只有文件路径（`add_rar5_data`）
  才有滤波；要体积对比必须用文件路径或 CLI。契约
  `rar5_solid_filters::solid_archive_filters_are_applied_and_roundtrip`。

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
- **KDF strength 上限 2²⁴ 是 DoS 护栏，不是待补功能**：ENCR 头/记录里的 `strength`
  是来自不信任归档的 1 字节；`derive_keys` 要跑 `2^strength` 次 HMAC，所以解析处
  把它封顶在 `MAX_KDF_COUNT_LOG = 24`（`crypto/rar50.rs`，头加密与成员加密两处解析
  都查），超过直接 `Err`——否则恶意档写 255 能触发天量迭代拖死进程。写侧默认
  2¹⁵；WinRAR 控制台没有抬升迭代的开关，没有已知实现产出 >24 的档，所以正常归档
  不会撞到它。要打开手工造的超限档需显式放宽（当前不提供开关）。

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
  与内联恢复记录**可以共存**（记录重建，`.rev`
  可选），`-k`（锁定档）先把整档重写（新 IV 重加密 main header），因此 main
  header 的 flags vint 变宽也能锁。重读 `-hp` 首块时**必须按明文读**（第一个
  块是无密钥的明文 ENCR 头）：已打开的归档再用档案密钥读它会解密密文 失败、报
  `CRC mismatch in block header`。 创建时每卷的文件头密文按 16
  字节对齐，而头部本身又按成员大小分档预留宽度，
  两个开销都要进卷预算（`header_encrypted_volumes_roundtrip_a_member_wider_than_a_size_tier`
  用 1.5 MB 成员 + 256 KiB 卷钔住「每卷不超过请求尺寸」，
  `official_unrar_validates_header_encrypted_volumes_with_wide_headers` 让官方
  UnRAR 解一遍）。
- **RAR4 `-hp` 编辑**：改名/注释/锁定逐卷重写，`-hp`
  档在同样加密下编辑（需要口令）；已有的逐卷 NEWSUB
  记录在重写时**按原强度重建**（丢弃旧记录再在末块前重发，否则 `rar r` 会用错
  parity 改坏数据），并在装好新卷后从新卷重建 legacy `.rev`。
  **显式请求新强度（`rar rr <set>` / `-rr`）自 2026-11 起也支持**：
  走整集重打包，按新强度逐卷重建记录并同步重建 `.rev`（见「一致拒绝」）。
- **分卷 append（RAR4/RAR5）是整集重打包，不是就地拼最后一卷**：`prepare_append`
  把整个卷集重写到暂存目录（原卷大小 = 除末卷外最小卷长、编号、固实标志、注释、
  逐卷 RR、`.rev` 比例），只留最后一卷的写入流打开，随后普通 `add` 追加；关闭时
  逐卷回填 QO/RR/ENDARC 并整体换卷。两个坑：跨 10 卷时 `part1` → `part01`
  重命名，调用方必须在 `{base}.rar` 上重新
  `discover_volumes`（旧首卷路径已不存在）； `discover_volumes`
  对不存在的输入会回退成单元素，不能拿它判断“有没有卷集”。
  追加成员另起非固实链（合法，且不需要旧链窗口）；多卷 SFX 的编辑/append
  仍拒绝。**对已有档显式 `-v<size>` 会改卷大小**：覆盖上面的「原卷大小」推导，
  单文件源则按该大小切成卷集（官方一律 `Cannot modify volume`，我们故意支持）。
  无 `-v` 时 RAR4 取**最大**卷长作上限（RAR5 取除末卷外最小），单文件源 `-v`
  默认新式编号。**`-sfx` 创建的卷集首卷命名对齐 WinRAR**：单卷为
  `{base}.exe`（Linux 为 `{base}.sfx`，官方 `rar s` 的拼法），卷集首卷为
  `{base}.part1.exe`（Linux `{base}.part1.sfx`），其余卷保持
  `{base}.partN.rar`；`discover_volumes` 把 `.part1.exe`/`.part1.sfx` 当作第 1
  卷（从任意后续卷或 base 名都能解析整套）。默认模块是**我们自带的控制台
  自解压器 `rar-sfx`**（与 `rar` 同目录构建，`RAR_SFX_STUB` 可覆盖），回退到
  官方控制台存根（`WinCon.SFX`/`default.sfx`），无需桌面即可自解压。自带存根
  链接 rar-rs，rodata 里含 RAR 签名常量——`detect` 因此按「签名后能解析出合法
  主头」挑选起点（`detect.rs`），否则会把存根里的假签名当归档开头。这条判定只有一个 owner：`rar s-` 的兜底与 CLI 的 `list::is_rar4_file`/`is_rar13_file` 都走 `detect::find_archive_start`，别再按「文件里哪个签名先出现」重扫——存根 rodata 里 RAR5 常量可能排在 RAR4 之前，那种写法会把 RAR4 SFX 判成 RAR5，还会随编译器排布漂移。存根的 CLI 对齐官方控制台 SFX（`-x`/`-t`/`-v`/`-?`、密码提示、覆盖提示、退出码 11/12）；创建的 SFX 在 Unix 上置可执行位（否则无法直接运行）。
- **删光成员后的重建保留档级元数据**：`a` 替换全部成员时 staging 档会被抹掉，
  `create` 必须把原档的格式（RAR4/RAR5）、卷大小（有 `-v` 时用请求值）、固实标志、注释、`-hp` 和 RR
  重新写回，否则会塌缩成单卷或静默丢注释/加密。`-hp` 重建还要关掉 quick-open
  （二者互斥）；`headers_encrypted()` 不能只读缓存状态（目录扫描会清掉缓存），
  需要探测首块 `ENCR`。RAR5 卷集注释自此也可编辑（`rewrite_multivolume`
  接受新注释），并且 `write_archive_header_vol` 会把固实标志写进每卷主头—— 此前
  RAR5 卷集的主头根本没有 solid 位，读取端一律当非固实处理。
- **PROTECT_HEAD（RAR 2.5 时代记录）可编辑/追加/重打包**：布局扫描越过 ENDARC
  继续找记录（首字节 3 位类型探测，不把尾部杂字节当格式错），删除/追加 把 0x78
  记录按原 `rec_sectors` 强度重建为 NEWSUB `Protect+`（官方 `rar r` 只认 NEWSUB
  形态）；官方 RAR 2.5 时代的 0x78 记录也能就地处理，不再要求重建归档。
- **`reconstruct` 保留的是容器族，不是成员 codec**：legacy 源重建为 RAR4
  容器、成员是 STORE（`unp_ver` 20）。**别把成员改回 29**，那会破坏 `-m0`
  的逐字节对拍。

## 整数宽度（32 位目标）

`usize` 是**指针宽度**，不是长度类型。在 Debian i386 的 rustc 目标 `i686-unknown-linux-gnu` 上 `usize == u32`，而且 release 构建会把算术溢出**静默回绕**。历史上 rar29 的 VM block start、rar5 的字典上限与编码器字典、rar4 的 `LONG_BLOCK` 收窄、恢复网格等 bug 全是这一形状：一个上界由格式定义（常 > 2³²）的量被放进了 `usize`。CI 的 `test-i686` job 是运行时兜底，跑的正是 `#[cfg(target_pointer_width = "32")]` 那些只有 32 位才存在的测试。

- **磁盘/格式量不进 `usize`**：成员大小、偏移、字典尺寸、扇区/块计数一律在 `u64`（或格式字段自身的宽度：RAR5 用 u64、RAR4 用 u32）里算，只在真正下标内存的那一步用 `usize::try_from(..)` 降下来。
- **收窄一律 `try_from`，禁止裸 `as`**：`as usize` / `as u32` / `as u8` 都可能截断；用 `try_from(..)?`（返回错误）或 `saturating_*`（有明确上限时）。clippy 的 `cast_possible_truncation` 是**目标宽度感知**的——在 x86_64 上也会为 `u64 as usize` 报警（"on targets with 32-bit wide pointers"），不需要 32 位主机就能拦。`format/rar5/sfx.rs` 与 `format/rar5/headers/encrypt.rs` 已用 `#![deny(clippy::cast_possible_truncation)]` 钉住，是照抄的模板；其余模块**清一个、钉一个**。
- **算术用 `checked_*`/`saturating_*`**：来自文件头或用户 `-md` 的值不要裸 `+ - * <<`；`block_start += 258`、`1usize << log`、`file_pow2 * 2` 都是这样来的。`recovery/legacy.rs` 第 238 行 `usize::try_from(..).unwrap_or(usize::MAX)` 是范例。
- **32 位回归测试**：能在 64 位也成立的断言就别 `cfg` 掉（`format/rar5/write/layout.rs` 的 multi-GiB 测试是无条件的）；只有 64 位根本复现不了（如 `0xFFFF_FFFF + 258`）才 `#[cfg(target_pointer_width = "32")]`——而每个这样的测试都必须有能跑它的 job。
- **待清点（burndown）**：`codec/*`、`recovery/{legacy,rev50}.rs`、`format/rar5/write/*` 等仍有大量收窄（多数是有界安全的：位读取 `read_bits(8)? as u8`、掩码后的 `(x & MASK) as u8`、`.min(u32::MAX) as u32`）。加 `#![deny]` 前先清到零；热循环里的有界转换就地写 `#[allow(clippy::cast_possible_truncation)] // 原因`。
- **清点时按门禁的 cfg 扫**：`check` 用 `--target x86_64-unknown-linux-gnu` 编译，所以计数也用它（`cargo clippy -p rar-rs --all-features --all-targets --target x86_64-unknown-linux-gnu -- -W clippy::cast_possible_truncation`）；`#[cfg(unix)]`/`#[cfg(windows)]` 分支必须各自的目标各扫一遍——只扫开发机（Windows）会漏掉 unix-only 的收窄。

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
- **测试不在主 CI 里跑**：CI 的确定性 smoke 只跑库 `--all-features` 与 CLI
  `--bins --test cli_behavior --test volume_bare_name`；**生成套件例外**——两个
  job 都先 `python3 scripts/fetch_winrar.py` 拉官方工具再跑它们（Windows 带
  `SA_REQUIRE_WINRAR=1`，Linux 因 2.90/3.00 无 Linux 版而允许自跳）。JS/WASI
  绑定套件仍是本地闸门。**改行为后本地必须自己跑受影响套件**——0.12.0 曾带着
  过期断言发布。
- **文档一致性靠人工**：面向实现的文档在标题下标注
  `最后核对：YYYY-MM-DD @ <短 commit>`（清单见 [`README.md`](README.md)），改行为要同步刷新。
  项目**不加**强制校验脚本（已定）。
- **`MAX_CATALOG_ENTRIES` / `MAX_MEMBER_CHUNKS` 等上限对每一族都要设**：RAR13
  每条 21 字节头也会 push 条目，legacy 跨卷合并同样要查 chunk
  上限（`check_chunk_cap`）。
- **RAR5 打包读取要读满声明长度并防御缺卷**（`get(vol)` 而非索引），与 RAR4 的
  `read_exact` 口径一致。
- **RAR4 清空归档先查 victim 非文件**（与 RAR5 同款守卫），否则会把同名目录 park
  成隐藏备份并遗留。
- **RAR 1.5/2.x 的归档注释在主头里**：RAR 3.0 才用 `rar` 后的 `CMT` 块，1.5/2.x
  把注释作为嵌套 `0x75` 块接在主头 13 字节之后（主头 HEAD_SIZE 覆盖整块，
  `MHD_COMMENT` 置位）。字段：`UNP_SIZE(2) UNP_VER(1) METHOD(1) COMM_CRC(2)` +
  数据；块头 CRC 只覆盖块自己的固定 11 字节（`block[2..13]`，**不含**数据），
  主头 CRC 只覆盖 `main[2..13]`，`COMM_CRC = crc32(text) & 0xffff`。载荷必须用
  容器同代编解码器压缩并声明 method 4（0x34）：官方 1.5 Beta 用 Unpack15、 2.90
  用 Unpack20，**即使压完更大也照压**；别图省事写 method 0x30（2.90 能
  容忍，但更老的 pre-3.0 读取器会判 "Comment header broken"，且与官方字节形状
  不一致）。四个后果： ① 注释必须在第一个成员之前写进去（`-z`
  在创建时就入队，并行批量路径必须先 冲刷队列，否则被静默丢掉）；②
  编辑/追加无法替换它，`ArchiveEditor` 直接 拒绝（"keeps its comment inside the
  main header"），别去写 `CMT` —— 老读取器 看不见；③
  读取端先认主头里的嵌套块，再回退到 `CMT`。
  契约：`legacy_versions_embed_the_archive_comment`、`legacy_comment_edits_are_refused_with_a_reason`、
  `cli_legacy_comment_is_embedded_at_creation`、`patch_main_header_preserves_an_embedded_comment`。
- **测试归档不落盘、不提交**：legacy 套件一律现场生成
  （`crates/rar/tests/support/generate.rs`，配方见同目录 `corpus.rs`；crate 内用
  `crate::test_winrar`，同一个 `#[path]` 文件）。铁律：**7.23 不能写 `-ma4`**
  （RAR4 写入上限 6.23）、`cf` 文件注释在 5.00 起被删（上限 4.20）、`rv3` 到
  RAR5 消失（4.20 是最后一个现代布局）、没有任何官方版本能写 v14/v15/v26/v36/v70
  （用我们自己的 writer 或改 `unp_ver` 字节）。缺工具时打 `SKIP:` 并跳过，
  `SA_REQUIRE_WINRAR=1` 把跳过变失败——**声称跑过生成套件就带上它**。
  另一个坑：`-z<file>` / `cw` 读注释文件用的是**控制台 ANSI 代码页**（本机
  cp936）， 拿 UTF-8 文件喂它就会被当成 GBK 存成另一个字符串（UnRAR
  回显反而看着正常—— GBK 往返）。要测 UTF-16 注释就把注释文件写成 **UTF-16LE +
  BOM**，工具会原样存； 断言用 `generate::comment_zh_text()`（UTF-8
  文本），不要用注释文件本身的字节。
- **工具缓存只有一个下载入口**：`scripts/fetch_winrar.py`（Python 3 标准库，
  Windows/macOS/Linux 同一份），`scripts/wsl/` 不再自带下载器。rarlab 的 Windows
  包是自解压 PE：**≤5.00 内嵌 RAR4、≥5.50 内嵌 RAR5**，只许用 7-Zip
  或（新到旧的）缓存 `Rar.exe` 解，**永不执行**——直接跑会启动安装程序；旧
  `Rar.exe` 只认得自己那代的载荷，所以要新版本先装。**Linux 只有 3.93+ 的
  tarball**，2.90/3.00 的配方在 Linux 自跳；且 Linux `rar` 写 `host_os = 3`
  （Windows 是 2），host 断言必须按平台取值。rarlab 对个别老文件会在最后一
  字节后重置连接，下载是否完好要看载荷能否完整解出，而不是看传输是否报错。
- **不要用带绝对路径的载荷名写归档**：`-m0` 会把源路径原样存进成员名 （历史
  fixture 里那个 `Users\yuan\AppData\...` 成员就是这么来的）。
- **标记编译门用 `cfg(unix)`/`cfg(windows)`，别用格式常量**：`OS_WINDOWS`
  是格式常量不是编译门；`wasm32-wasip1-threads` 既非 unix 也非 windows，漏门会让
  CI 红。
- **`RarArchive`
  的读状态必须装箱**（`read: Option<Box<ReadState>>`，别改回内联）： `ReadState`
  内联了解码器的 Huffman 表（~19 KiB），而 `RarArchive` / `ArchiveReader` 在
  open/write 路径里大量**按值**传递——内联时门面达 21 KB，未优化
  构建下这些按值拷贝吃掉 ~600 KiB 栈，CLI `rar a` 追加到已有档的调用链随即撑爆
  Windows 约 1 MiB 的主线程栈（`STATUS_STACK_OVERFLOW`；release 与 `cargo test`
  因 `[profile.test] opt-level = 2` 都不受影响，所以 CI 抓不到，只有 debug 的
  `cargo run`/本地构建会踩）。装箱后门面降到 1744 B、debug 栈需求降到 ~200 KiB。
  契约 `archive_engine_stays_small_enough_for_the_main_thread_stack`。

## 压缩内核（匹配器与解析器）

- **候选数量不是瓶颈，单价才是**：哈希环把每位置候选降到 4–8 个（48.2%），位置
  索引链把候选质量拉回树的水准（47.8%），但每走一个候选都要 `match_length_at`
  整段测长并多一次随机读，单价高一个数量级（13.9 s vs 4.7 s）。BT4 每个下降步
  只比一个字节并顺带收紧上界——这是它十年是标准做法的原因。整轮重写实测后
  **已回退**，判定与数字见 [ADR 0010](adr/0010-hash-ring-finder.md)。
- **别指望换掉 BT4 再“顺手”拿比例**：环/链/更长键/更多深度/更大表（2^18–2^20）
  全部在 47.3–50.9% 区间饱和，而 BT4 在同一水平上是 46.29%；要动匹配器，先想
  清代价模型，再动数据结构。

- **热点要先量，不要猜**：`RAR_RS_PROFILE=1` 会打出 `collect / dp / prep / emit`
  的分相时间。`ntoskrnl` m3 实测是 collect 66%、dp 22%、prep 8.7%、emit 2.6%
  ——“DP 是最贵的”这个假设错了两轮（旧数字是 BT4 cut16 + 两遍定价时的口径）。
  计数字段同样有用：93% 的位置都要下降一次树、每 2.9 个位置一次短匹配探测。
- **近窗口的大小就是工作集**：`NEAR_WINDOW_MAX` 既限尾上下文，也决定树的
  `son`（每字节两个 `u32`）。`ntoskrnl` m3 实测（`RAR_RS_NEAR`，KiB）： 8192 =
  4576 ms / 5,832,147 B；6144 = 4328 / 5,848,138；4096 = 4104 / 5,874,129；2048
  = 3638 / 5,920,097；1024 = 3269 / 5,980,476。约 10% 时间 换
  0.3pp，是“更小的树、少一趟 DRAM”的代价。m1 是个例外：4096 既快 16% 又小 9.7
  KB（贪婪路径的链环形表被稀释得更少）。
- **树预算 ≥24 就不再是旋钮**：m3 上 cut 24/32/48/96 的字节数相差不到 40 B、
  时间相差不到 5%（下降自己会先结束）；cut ≤16 才是用比例换时间。m4/m5 同理。
  后来把链长拉到 256/1024（m4/m5）实测也证实：collect 2728.5 / 2727.7 / 2772.0
  ms， 比例 44.61 / 44.52 / 44.50%——**树先走空，预算根本不咬合**。
- **截断的 `len_limit` 不得收养子树**：BT4 在 `len == len_limit`
  时直接认领对方的
  两个子节点（`son[ptr0] = child_greater; son[ptr1] = child_less`），前提是限长是
  调用方的真实上界。`collect` 给帧尾 64 字节内的位置传的是
  `avail.min(verify_cap())`——**被输入尾部截断**的限长，前缀相同证明不了后面的字节，
  收养之后树的序就坏了，之后每次走行都虚报长度，并且这棵坏子树活到成员结束。
  症状极隐蔽：只有每块最后几个位置写坏，表现为 `match_length_at` 复核偶尔把 16
  字节 砍成 7
  字节，所以一直当成“树不可靠、需要复核”而不是“树被写坏”。现在下降遇到
  `input.len()` 就割断两个挂接点、既不猜方向也不收养，复核那趟整趟删掉（ collect
  -3.3%，比例不变）。改动：`match_finder.rs` 的 `descent`，提交 `9cbb354`。
- **表头位宽随窗口走，不是固定
  2^20**：`head_bits_for(window) = window.trailing_zeros().clamp(23, 24)`，
  表头存 `pos + 1`、0 表示空，于是 `vec![0; 1 << bits]` 走 calloc
  惰性缺页；`grow_to` 只增不减。 位宽只决定桶深（=
  白走的步数），不影响候选质量：2^20/2^22/2^24 三种位宽的比例都是
  44.61%，collect 却是 3380/2824/2677 ms。总收益 ntoskrnl m3 -14.3%、d2d1 -8%、
  小文件不回归。下限 23 由 `long_range_matches_compress_distant_repeats`
  看守：2^21 时树 太浅，长距探针抓不到 2 MiB 外的副本。见
  [ADR 0011](adr/0011-official-shaped-matcher.md)。
- **距离类被两种形态各证否一次**：8 棵独立的类树 = +18% collect / -0.56pp 比例；
  官方真形态（一份位置索引 `son` + 8 份 head 快照 + 共享远预算）= +193% collect
  /
  比例持平。原因是**入口免费、远走行贵**：远快照的入口点在窗口末尾往回一块，每步
  一次 DRAM 缺页（~16 ns/步），24 步 × 13.9 M 位置 ≈ 5.3 s；而 BT4
  在同一次按内容 排序的下降里本来就能走到这些候选。实测：只用最新一块（4 MiB）是
  45.54%， 把远段补回来正好回到 44.61%，即距离类买到的每一个 pp，BT4
  都是白送的。 见 [ADR 0011](adr/0011-official-shaped-matcher.md) 的两个 P2
  小节。
- **重定价（多遍 DP）不值得**：用上一遍的真实码长再跑一次 DP，只买 0.23pp， 却让
  m3 多跑一遍 DP（+20~37% 时间）。seq 已改为单遍（`parse_one_block` 不再有
  `passes`）；MT 的“块间顺延价格表”（`windowed_priced_parse` 的
  `carried`）是零额外 DP 的版本，保留。
- **每候选单价已到地板**：BT4 的代价是「每位置 5.6 次依赖的随机访存」，ns/步
  随工作集 上升（1 MiB 21.8 → 4 MiB 28.9 → 13.3 MiB
  34.4），而缩小树只换来比例损失（NEAR 8192→512 KiB： 41.8→31.5 ns/步，比例
  44.61→45.29%）。DP 那边同样没有余量：13.9 M 位置 917 ms = 66 ns/位置， 字面量
  7.5 + rep 探针 20 + 多步定价 12 + reaches/plan/写回 25；探针全关省 284 ms 却要
  +0.71pp， `RAR_RS_COMMIT=32` 省 218 ms 要 +0.09pp，`NICE_MATCH_LENGTH` 64→32
  则三项全无影响。 剩下的路只有换候选生成形态（ADR 0009
  的多槽链，体积换速度）。见 [ADR 0011](adr/0011-official-shaped-matcher.md)。
- **官方形态本身不是速度来源**：把 `Rar723.exe` 的链式候选生成整个照搬（每位置
  `u32` 链、 首字节+哈希双发散即返回、`len-5` look-ahead、`best_dist*0x4000`
  剪枝、预算 100/25）后， 仍然 collect 2739.7 ms / 45.45%（BT4 2423.0 ms /
  44.61%）。原因是**每步信息量**：BT4 每步把候选空间砍半（5.2
  步摸到长匹配），链每步只排除一个候选（8–25 步），而两者都要
  一次随机访存；距离序遍历又让「近而短」的匹配先出现，之后更远的候选必须严格更长才被记录，
  长候选常常轮不到。官方 1466 ms
  只能来自「搜多少个位置」而不是「每个位置怎么搜」—— 它的 `+0x300b8`/`+0x300bc`
  计数器会驱动快速模式把大量位置直接跳过。 见
  [ADR 0011](adr/0011-official-shaped-matcher.md) 的「官方形态的哈希链」一节。
- **一步的单价是吞吐不是延迟**：把树压到 256 KiB（全在 L2）每步仍要 28.4
  ns，13.3 MiB 时 32.8 ns——只有约 4 ns
  属于访存缺失。所以预取的收益很小（预取两个子节点的链接对： collect
  −3.3%；只预取输入字节反而 +5.3%），侧向选择改成无分支则毫无变化。两位置交错
  下降的上限也就是藏掉那 4 ns（约
  12%），不值得为它把逐位置循环改成两宽流水并放弃
  「候选流逐字节不变」的验证手段。见
  [ADR 0011](adr/0011-official-shaped-matcher.md) 的 「下降步的单价」一节。
- **便宜解析要配距离类候选**：官方没有代价 DP（`+0x20058` 的唯一读者
  `FUN_1400478e0` 是一步 惰性匹配，判据在 `FUN_140047698`，容差 255/2047/32767
  是距离码边界）。把它原样实现 （`RAR_RS_LAZY`）确实把 dp 从 897 ms 压到 91
  ms（6.5 ns/位置），但比例 44.61 → 48.15 %； 再叠 `RAR_RS_STOP=12` 是 1996 ms /
  49.27 %。原因：我们的候选是「一列按长度递增的最长匹配」，
  惰性判据只能在这列上做近者优先（再掉 1.2
  pp），而官方的价格判据在搜索函数内部就执行，
  每位置拿到的是各距离类的最佳候选。想让便宜解析可用，先要让 collector
  按距离类产出候选。
- **官方 m3 的时间构成（实测采样）**：搜索 57.7 %、建直方图 10.5 %、rep 结算 9.9
  %、每位置 包装 5.1 %、解析只有 2.3
  %；指令级热点是链上那两次串行依赖访存。所以官方的 42.30 % 不是
  「更聪明的解析」，而是「候选更齐 + 判据更早」。方法见
  [ADR 0011](adr/0011-official-shaped-matcher.md)
  的「采样剖析」一节（`scripts/winrar-profile/sample.ps1` + capstone
  归因，无需管理员/ETW）。
- **「少搜一些」只剩比例换速度**：官方计数器驱动的快速模式在我们这里是
  `RAR_RS_STALL` （连续 N 个空位置后把步数上限降到
  `cut`，一有候选就复位）。它只把步数降 2.5%——
  空位置本来就没走满预算，步数集中在有产出的位置上，那里缩短下降直接用候选换时间。
  曲线单调：8/4 = 44.67%/−2.5%，1/4 = 45.01%/−10%，1/1 = 45.62%/−25%（比既有的
  `RAR_RS_COMMIT=16` 的 +0.53pp/−17% 更划算，两者可叠加）。默认关闭。 见
  [ADR 0011](adr/0011-official-shaped-matcher.md) 的「停滞降档」一节。
- **宁可做判据，不要做试验**：forced-combined 一开始是「每 64 KiB 块
  plain/delta/E8/E8E9 各压一遍取最小」，在 ntoskrnl m3 上 5,828,867 → 5,789,172
  B，代价是 3 份 13.9 MB 的 m1 编码、约 1.2 s。改成贪婪哈希覆盖判据后**又小
  8,963 B、又快 2 s**：试验的排序本身 就有噪声，而且它测不到跨块匹配。见
  [ADR 0011](adr/0011-official-shaped-matcher.md) 的「免试验的滤镜决策」一节。
- **试验编码的方法不等于真编码的方法**：用 m3 试验比用 m1 试验**更差**
  （ntoskrnl 5,850,197 vs 5,789,172 B）——高方法下 plain
  更容易赢，块被判成不用滤镜。 试验只是排序信号，便宜的那个反而更准。
- **逐块 delta：挑帧别看占比，用不用要看余量**：`block_delta_channel` 只按「差值
  ≤ 8 的字节数」挑帧，挑帧**不要**加占比门（60 % 门会让 ntoskrnl 的 .rsrc 段少 7
  个 delta 块、多 35 KB）。但「这一块到底用不用
  delta」必须要求匹配覆盖**大胜**（8 %，
  `RAR_RS_HEUR_DELTA_MARGIN`）：kernel32.dll 上块内试验以 68 B 的平局发了 delta
  记录， 在 32 MB 字典的流里值 **+11 KiB**。
- **块内试验看不见跨块匹配，所以它对平局没有判断力**：试验是「孤立压 64
  KiB」，而真 编码的字典跨块。E8 与 E8E9 的块内差常常只有几十字节，换掉 E9
  的处理却会改变这一块 与前后块的匹配关系（kernel32 上 3 个块值 11 KiB）——x86
  类型要在**成员级**用整段流 的覆盖定，不要逐块定。
- **DP 跳步省不了时间**：`RAR_RS_DP_SKIP`（默认 64，DP
  一旦接受长匹配就跳过其覆盖的位置） 降到 16 只让 `waves` 从 1,040 掉到 1,022
  ms，比例却从 42.28 % 掉到 42.51 %。跳过位置几乎 不花钱，贵的是一开始那些位置的
  run 定价，所以「选择性定价」这条路不成立。
- **DP 块大小是「价格交接频率」的旋钮**：96 KiB 在 ntoskrnl 上少发 4.8
  KiB（5,879,871， 速度不变），但 5.7 MiB 文本上多发 196 B。默认保持 256
  KiB，`RAR_RS_MT_DP_BLOCK` 可试。
- **MT 只在成员能切出几片时才划算**：2 MiB 片地板下，845 KiB 的 kernel32 从
  40.23 % 掉到 41.03 % 且更慢，4.35 MiB 的 win32kfull 从 42.57 % 掉到 43.41
  %。判据是 `data.len() >= RAR_RS_MT_MIN_LEN`（默认 8 MiB =
  四个切片），不是线程数。
- **MT 的每字节地板是 DP 本身**：`RAR_RS_MT_DEPTH=1` 时 `waves` 559 ms / 13.9 MB
  ≈ 279 ms/MiB，与顺序路径的 288 ms/MiB 相当；候选每层再加约 20 ms（depth 64 ≈
  1.0 s）。 想接近官方的 305 ms 不是调 depth 能到的——那要放弃逐位置定价。
- **MT 的比例损失来自解析层，不是切片数**：`-mt2`（7 MB 片）已经 42.29 %、`-mt8`
  42.28 %，把 2 MiB 的片地板降到 512 KiB 只再掉 0.04
  pp。所以想省时间时可以放心把片切小， 但别指望靠「片大一点」找回比例——MT
  层用的是共享行索引 + 有界候选（`MT_ROW_INDEX_DEPTH`）， 不是 BT4 树。
- **滤镜扫描是编码前的串行大块**：块统计要跑 11 个 delta 通道 + 2 个 x86
  类型的覆盖扫描， ntoskrnl `-mt8` 上是 354 ms（1,714 vs 1,360 ms
  的差）。`block_stats_all` 已改成 `compression_pool()` 分块并行，`-mt1`
  走串行分支；新加串行前置工作时先想想它能不能进池。
- **哈希链喂不饱代价 DP**：官方 `Rar723.exe` 用的是 3/4/5 字节哈希链 + 贪婪跳步
  （`FUN_140044794`，没有价格表、没有 DP）。照搬链、保留 DP 结果是 45.94 % / 2.6
  s （链上每位置只有 3 个候选，DP 无解可挑）或 42.49 % / 6.2 s（走满 100
  步，比树还慢）。
  候选数量对「代价最优」的门槛远高于对贪婪的门槛——要换的是解析形态，不是候选来源。
  （`RAR_RS_CHAIN` / `RAR_RS_CHAIN_SPAN`）
- **熵不是匹配覆盖**：`H(next|prev)` 只说明字节偏斜，不说明会重复。ntoskrnl 的
  PE 头块 上 delta 把 H1 从 2.214 降到 1.790 bit/B，实际却大 533 B；`argmin H1`
  判据损失 +22.3 KB，`argmax` 贪婪 4 字节哈希覆盖只损失 +7.4 KB（对照三路试验
  oracle）。
- **滤镜的免试验判据已经落地**：`match_cover` 对原样/delta/x86
  各做一遍贪婪哈希扫描， `heuristic_spec` 按覆盖选，x86 类型在成员级定。ntoskrnl
  m3 上 5,780,209 B / 4.1 s （试验路径是 5,789,172 B / 6.1 s，旧默认 5,828,867 B
  / 3.9 s）——比试验**又小又 快**。`RAR_RS_FILTER_TRIAL=1` 可退回试验路径对照。
- **覆盖扫描不要只看块首**：`RAR_RS_HEUR_SAMPLE` 把判据的覆盖测量限制在块首 N
  字节， 16 KiB 只省 ~100 ms 却赔 2.6 KB，8 KiB 赔 32 KB，4 KiB 赔 99
  KB（ntoskrnl m3）。 结构在 64 KiB
  块里分布得比直觉均匀，抽样在这里没有便宜可占。默认整块。
- **四字节表当主候选只买到比例**：`RAR_RS_HASH4` 的 `head4`（4 Mi 槽）每位置
  O(1) 探一个候选，把树当补充：全开 41.36 % / 4.5 s（默认 41.53 % / 4.0
  s），只开表 46.17 % / 1.9 s。单候选喂不饱代价
  DP——它要同一位置上的多个距离与长度档。
  想让时间掉下来，要改的是「哪些位置需要候选」，不是候选从哪来。
- **树搜索的步数与时延是线性关系**：STALL(2,2) 把步数 75.4 M → 64.5 M（−14.5
  %）， collect 2,313 → 2,025 ms（−12.5 %）；每步 15.8 ns ≈ 60 周期，代价是 64
  MB `son` 数组的随机访问（md32m 的 8
  B/位置），不是算术。所以「少走步」只能换比例，
  想同时要时间就得让**搜索次数**降下来（官方 3.45 M 次 vs 我们 12.6 M 次）。
- **`RAR_RS_STALL` 是单个整数，不是 `a/b`**：停滞档位是
  `RAR_RS_STALL=<空降次数>` + `RAR_RS_STALL_CUT=<步数>`；把 ADR
  里「2/2」的写法直接传给 `RAR_RS_STALL` 会 `parse`
  失败、静默关闭，看起来像「这个旋钮没效果」。
- **逐块分配是可见成本**：DP 的 per-position 状态 32 B（price/长度/距离/ 四个
  rep/上次长度），64 KiB 块约 2 MB，13 MiB 成员有 200 多个块；改成按 chunk
  复用的 `DpArrays` 后，只保留同量的 memset，不再每次都新映射页面。

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
  **两条抽取路径必须共用同一个实现**（`members.rs::record_member_outcome`），
  否则同一个 `ExtractOptions` 会因为「id 列表是否恰好覆盖整档」而给出两种行为
  ——CLI 的每一次带选择器的 `x`/`e` 都走按 id 那条，2026-09-30 修的就是它： 按 id
  路径曾经直接 `?` 抛出，`Collect` 静默退化成 `Abort`。
- **进度 sink 只借用，绝不取走**：`ExtractionProgress` 是
  `Arc<Mutex<Option<Box<dyn FnMut>>>>`，抽取**不得** `guard.take()` 把调用方的
  闭包移进本轮 tracker——`ExtractOptions` 是 `Clone`
  且文档承诺「克隆体报告给同一个 回调」，取走之后第二轮静默无报告，而被清空的
  `Option` 看起来只是"没装回调"。 正确做法（`members.rs::start_progress`）：克隆
  Arc，把闭包**包**进本轮 tracker，逐成员基线留在 tracker 里，于是复用同一
  options 值的两轮各自从 0 走到自己的 `total`。同理，`bytes_total`
  是**本轮将写的成员**之和：按 id 抽取 时是被选中的那些，否则过滤抽取永远走不到
  100%。
- **RAR5
  解码热路径的四个不变量**：解码速度靠四处结构，改动前先想清楚谁在付钱——①`BitReader`
  是**移位寄存器**（`u64` 里高位是未消费比特，一次 8 字节大端装载能填进 7
  个字节），所以 `peek_bits` 是一次移位、`skip_bits` 又是一次，别退回逐字节
  `copy_from_slice`：那是每次 peek 拷 8
  字节，实测约占总解码时间的四成；②`DecodeTable::quick` 把**码长和符号压进一个
  `u32`**（高 16 位码长、低 16 位符号），一个 `QUICK_BITS`
  前缀一次装载，拆回两个并行数组会让每个字面量多一次内存访问；③`SlidingWindow::copy_match`
  在**源不跨环尾**时走 memmove：`dist >= len` 是一次
  `copy_within`，`dist < len`（自我重叠）则先搬一个 `dist`
  周期的前缀、然后**倍增**（`done = dist; while done < len { copy_within(dst..dst+n, dst+done); done += n }`），代价从
  `len/dist` 次 memmove 降到 `log2(len/dist)`
  次；只有源跨环尾（每绕一圈最多一次）才退逐字节——逐字节版每字节一次掩码，是仅次于
  ① 的开销。别把倍增「简化」回「每次搬 `dist` 字节」：真实二进制里自我重叠只占
  0.26%，看不出差别，但 13 MB 全零这种长游程（`dist=1`）会退化成上百万次单字节
  memmove，端到端实测 **0.106 s → 0.047 s（−56%）**，13 MB 的 `abcd` 重复是
  −25%。契约 `copy_match_overlapping_is_the_repeating_pattern`（`window.rs`
  单元测试，扫 `dist∈{1,2,3,4,7}` × `len∈{1..4096}`
  逐字节比对周期）；④`get_output` 只留给原地调用，流式路径用
  `append_output_into` **直接追加到 staging**，少一次中间 `Vec`
  和它的分配。⑤**`SymbolReader::next` 的返回类型只许两字宽**：`RarError` 是 88
  字节，早先返回 `RarResult<Option<Symbol>>` 就是每个符号把这 88
  字节压一遍栈（一颗 13 MiB 成员约 660
  万次），实测占了符号流开销的一大半——现在错误装在 `SymbolReader::error` 里，由
  `take_error()` 在循环结束后取，`fail()` 挂
  `#[cold] #[inline(never)]`，热路径上只有两个字；⑥**热路径上的
  `peek_bits`/`skip_bits`/`fill`/`decode_symbol`/`next`/`decode_symbol`（`symbols.rs`
  那层）必须挂 `#[inline(always)]`，稀有半边（`decode_non_literal`）反而要
  `#[inline(never)]`**：`#[inline]` 只是提示，实测 LLVM 会把
  `huffman::decode_symbol` 留成外联调用，每个符号多吃一次调用和一次 24 字节 sret
  返回，强制内联后同机交替 A/B 是 −10~−17%（`ntoskrnl` 无过滤 m5 0.221→0.195
  s、text 13 MiB 0.084→0.072 s）。⑦`BitReader::consumed_bits()` 直接算
  `pos*8 - bits`，别用
  `byte_position()*8 + bit_position()`——后者每次带一次除法和一次取余，而块结束判断是**每个符号**都要问的；⑧**`next_symbol`
  要小到能被内联，`read_block_header` 必须 `#[inline(never)]`
  **：块头里建霍夫曼表是几百条指令，一旦被内联进 `next_symbol`，它就长到 LLVM
  不肯再往 `run_engine` 里塞——而 `next_symbol`
  是**每符号一次**的调用点，这一层外联调用就是每个符号白付的调用+返回。判据不是看源码长短而是数汇编：`cargo rustc -p rar-rs --release --lib -- --emit asm`
  之后在 `run_engine` 那一段里 `grep callq`，出现 `next_symbol`
  就说明没内联、出现 `read_bits` 就说明位读取也被踢出局（修好后只剩
  `read_block_header`
  一次，那是每成员几次的量级）。收益要按场景说清楚：合成语料（`decode_bench`
  三个语料的中位数）**+6~7%**，而 `ntoskrnl` 的 m5 真实归档只有
  **−1~2%**（长匹配多、符号数相对少），别把前者当成后者的承诺。契约：库内 580
  个单元测试中的往返类（`mt_tests::*`、`member_larger_than_shared_window_decodes`、`split_filter_records_decode_across_staging_boundaries`）覆盖正确性；性能对比必须用
  `--release` 且**交替跑两个二进制取最小值**，同机上单次运行抖动可达
  ±30%；仓库里能复现的解压基线是
  `cargo run --release --example decode_bench`（自建语料、固定种子、打印 CRC32
  与 packed 比例，`--keep` 会把归档留在临时目录好交给官方 `UnRAR`
  对拍——官方的进程启动约 0.10 s，端到端比 `t`/`x`
  必须扣掉这个常数，否则会得出「官方更慢」的假结论）。
- **RAR4 solid 成员搜索可以并行，胜负必须串行裁决**：没开 `parallel` 时的
  `search_solid_member_serial` 是**参照序**，并行版只改“何时算”不改“谁赢”——每个
  候选（plain LZ、逐个 `-mc` 过滤候选、PPMd 试压）都只读进入该成员之前的链状态
  （`history` 只借用、`levels` 各拿一份、被携带的 PPMd
  模型克隆或新建），结果再按 串行顺序排名（最小者胜、平局留靠前、PPMd
  严格更小才赢）。PPMd 试压因此可以 提前放弃：区间编码器只追加字节，超过当前 LZ
  最优（并行时是所有 LZ 候选共享的
  原子计数器）就必然输，放弃不可能改字节；并行版把 `limit` 换成那个活计数器，
  所以它**只能**让试压更早停，不能改判。成员超过
  `RAR29_PARALLEL_MEMBER_THRESHOLD` 时根本不进这条路（它自己的块级并行会把切片
  排到彼此后面）。契约：`solid_search_parallel_matches_serial`（同一链两种跑法逐
  字节相等，并覆盖 PPMd 胜、LZ 过滤胜、STORE 三条分支）。
- **v20 块级 MT：每块 fresh `ParseState`，按块序写位流**：RAR2.x 成员本就是 LZ 块
  序列（块间主符号 269 `EndOfBlock`、各自读表），块间唯一依赖是 `ParseState`
  （`old_offsets`/last-match）。编码时每块用**全新**状态、块窗口取
  `history ++ input[..块首]` 的末尾 `max_match_distance`，所以块间独立、分析可在
  压缩池上并行；解码端 `old_offsets` 虽跨块保留，但块只引用自己推入的偏移（陈旧
  槽位在编码器里是 0/无效、永不引用），因此字节一致。建表与写块（`write_lz_block`）
  必须**按块序**串行。契约 `block_mt_matches_sequential`（串行 vs 池化逐字节相等 +
  解码回环）。**v15 不能这样**：单连续自适应流，没有可分块边界。
- **DP 解析的两笔账：价格表只算一次，收敛了就停**：`optimal_parse_tokens` 的每个
  位置、每个长度槽都要问「这个长度的价格区间到哪结束」（`same_price_run_end`），
  早先它每次都回头去扫 `state.reps` 找「该距离是否在缓存里」并重算
  `length_bonus`——这两件事 `EncoderMatchState::plan` 在同一个位置已经算过一遍
  （`plan.cache_index` 是否 `Some` 就是「在缓存里」，`plan.fresh.bonus` 就是那个
  加成）。改成 `same_price_run_end(&plan, length, max_match)`
  之后逐字节相同，同机 交替 A/B：`ntoskrnl` 12.5 MiB `-ma5 -m5 -mt1` 与
  `-m3 -mt1`、30 MB 文本 `-m5`、 以及 `-mt16` 全部 **−4.5~−8.9%**（20 组归档
  sha256 全等）。另一半是重定价的 不动点：`-m2..-m5` 会跑 `OPTIMAL_PARSE_PASSES`
  次「按上一轮码长表重新定价」， 一旦某轮产出的 token
  序列与上一轮完全相同，后面几轮必然产出同一个序列（价格只 由 token
  序列决定），所以 `if repriced == tokens { break; }` 是白送的早退；实测只
  在二进制数据上触发（`ntoskrnl` m5 601 次重定价里 39 次收敛、m3 201 次里 1 次，
  4 MB 文本 0 次），文本类语料别指望它。**别试图把这两处再往前推**：按 (位置,
  距离) 缓存重复距离探测结果（跨 pass 复用 `match_length_at`）实测无收益
  （每位置多 16 字节流式流量比省下的远地址读更贵），预取未来位置的两个重复距离
  同样无收益（±13% 的机器抖动就能吃掉全部信号），把 `OPT_BLOCK_SIZE` 从 64 KiB
  降 到 16 KiB 只换来 3.7% 且改变字节——**DP 的 55%
  是远地址探测的缓存缺失加每步定价
  本身，不是数组足迹**。做这类对比必须交替跑两个二进制取最小值：同机抖动可达
  ±30%，单跑一轮会得出反的结论。
- **槽步进定价的融合版：一个 run 只解一次距离侧，参考实现必须留着当判据**：
  `optimal_parse_tokens`
  的每一步原本问两次——`same_price_run_end`（这个长度的价格 区间到哪结束）与
  `match_cost`/`estimated_match_cost`（那个 reach 值多少）——两次都 从 `plan`
  重新解一遍 `MatchShape`、一次 `encode_length_slot`（`leading_zeros`）、一次
  `distance_bits` 查表。但一个 run
  的**距离侧**（缓存槽的码长、新鲜距离的槽符号与额外
  位、长度加成、以及哪个长度走 repeat）对 run 里每个长度都一样，于是
  `SlotPricer::for_run` 把它一次分类好，`step` 每步只解长度槽：
  `bits = value.ilog2() - 2`、`sub = (value >> bits) & 3`、`slot = 4 * (bits + 1) + sub`、
  `natural_end = (((value >> bits) + 1) << bits) - 1 + 2 + bonus`——**槽号不需要
  `encode_length_slot`，也不需要 `last_value`**（`last_value` 的 `ilog2` 恒为
  `bits + 2`，sub 恒为同一次移位的结果）。 **唯一不能省的是 repeat
  截断**：`reach = min(…, repeat_length - 1)` 落到的槽可能比
  当前长度**更靠后**，那一格得按 `repeat_length - 1` 自己算，所以每 run 预解一次
  `price_of_one(repeat_length - 1)`（`Option`，可能根本不可编码）当
  `repeat_bound`； `run_end`
  截断则始终同槽，可以沿用本步价格。`same_price_run_end` / `match_cost` /
  `estimated_match_cost` / `shape` 全部保留为 `#[cfg(test)]` 的参照实现，
  `slot_pricer_tests`（4000 组随机
  `TokenPrices`/reps/last_length/距离/variant，逐长度 比对 reach、价格与
  `None`）是这道口子的闸门——把 `if reach > bound` 改成
  `if false && reach > bound` 它会立刻报
  `V50: left (161, Some(14)) right (156, Some(14))`。同理**不许动步数预算记账**：
  `MAX_PARSE_STEPS_PER_POSITION` 只数非最长者的步，reach
  序列一变预算就变、字节就变。 实测：packed 字节逐位相同（4 语料 × 3 档的 packed
  流 FNV 全等，`examples/ratiocheck.rs` 的 hash 列）， DP 阶段 −9.4%（wordtext
  m3）/ −9.6%（m5），端到端 wordtext m3 −8%、xml −6.4%、 dll-like −6.3%、x86syn
  −3.0%、text −1.4%、mixed/random 持平。
- **并行提取必须给逐成员语义让路**：开了进度回调、或策略不是 `Abort` 时，
  `extract_all_parallel` 一律返回 `None`
  退回串行——它先把所有成员解码完，"在第一个失败处停下"根本无法实现，与其猜不如让路。改这些条件前先想清楚顺序语义。
  （因此该函数**不接** progress tracker：资格条件已经排除了它的存在，
  传进去只会留一段永不执行的报告代码。）
- **并行校验的资格门槛**：`verify_indices_parallel`（`t` 的并行路径）接手 RAR5
  与 RAR4（RAR 1.3/1.4 除外）、非固实链成员、≥`PARALLEL_MIN_MEMBERS`(4)
  个成员、总解压量 ≥`PARALLEL_MIN_UNPACKED`(8 MiB)、且没有进度回调的批次。
  并发数由 `live_workers_for` 按预算算出：RAR5 预读的全部 packed 常驻，剩余
  `PARALLEL_PEAK_BUDGET`(512 MiB) 按成员解压缓冲从大到小装入，只放得下 1 个
  时退回串行（0/1 路都走串行）。不要把这件事改回"整档是否超预算"的一刀切： 8×90
  MB、200 MB packed 的档预算是 3 路，串行 1358 ms，3 路 1245 ms；而
  packed+unpacked 总量口径会直接把它打回串行。RAR5 的负载必须 先在**单个**
  `Engine` 游标上串行读出解密（`read_packed_data`），再并行 decode
  ——不要让多个线程共享 `Engine`；RAR4 反过来：工作线程各自 `File::open` 卷 0，
  由 `decode_member_bytes` 按 chunk 打开后续卷，所以分卷/跨卷成员也能并行，且
  不需要预读负载。一个成员一份缓冲，哈希算完就丢，峰值内存是"RAR5 全部 packed
  - 每线程一个解压缓冲"。错误按目录顺序汇总，`Cancelled` 仍然整体返回。门槛改 成
    64 MiB 时 87 MB 的 DLL 档就不会走并行，`t` 从 205 ms 退回 ~330 ms——这是
    故意调的，不要为了提高命中率去动内存预算。
- **整档 `x` 的并行路径是解码/落盘流水线**：工作线程解码后经**有界**
  `sync_channel` 交给主线程，主线程仍按目录顺序执行 `resolve_member_target` →
  `materialize_member_file` → `finish_member`，所以 `-o-`/`-or`/`-kb`/覆盖提示的
  语义与串行完全一致；重排窗由通道容量限制，不会攒下整档。写失败时先丢弃接收端，
  工作线程的 `send` 失败即退出，`scope` 不会死锁。`chunks.len() == 1` 的限制已
  取消：RAR5 多块/多卷成员按整成员预读，RAR4 由工作线程按 chunk 读卷。
- **RAR4 的 0x78/0x7a 恢复记录不是唯一的"跨卷"陷阱**：`is_rar4_solid_member` 对
  pre-RAR3 的 codec 依赖 `MHD_SOLID`（这些版本不写 `FHD_SOLID`），并行路径
  必须用同一个判定，否则会把固实链拆到多个线程、解码出垃圾。两种格式的判定统一
  走 `parallel_solid_member`。
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
- **WASI loader 的文件名不归我们选，补丁必须最后一步跑**：
  `napi build --target wasm32-wasip1-threads` 恒把 flavor loader 写成
  `<binaryName>.wasi.cjs`（`wasiLoaderSuffix()`，这里是
  `rar-rs-napi.wasi.cjs`）；`--js` 只改*平台解析器*的名字，对 wasi 目标会把
  一个用不到的 `binding.wasi.cjs` 落到 wasm 产物名上。所以 `build:wasi` 用
  `--no-js --dts rar-rs-napi.wasi.d.cts`，release 契约仍是 0.12.0 的
  `rar-rs-napi.wasi.cjs` / `.wasi.d.cts` / `.wasi-browser.js`。两个连带后果：
  **任何一次 `napi build` 都会把 wasi loader 重新生成为未打补丁版**，改完要
  `node scripts/patch-wasi-loader.mjs` 再跑；native 构建写入的
  `managedRootEntries` 会让随后的 `build:wasi` 删掉 `binding.js`，本地想同时
  持有两者就先 native 后补丁，别反过来。0.13.0 的 tag 首跑就是被这个骗了：
  补丁按旧名字找 `binding.wasi.cjs`，CI 的 build-wasm 在 patch 步骤硬失败。
- **仓库 URL 以远端为准**：根 `Cargo.toml` 的 `repository` 与
  `crates/rar-napi/package.json` 的 `repository`/`homepage`/`bugs`
  必须同源。曾长期写着一个不存在的 codeberg 路径，而真实远端是 GitHub（三个
  crate 的 metadata 一起错）。
- **`.d.ts` 必须被编译**：`npm run typecheck`（`tsc --noEmit` 加
  `__test__/types.test-d.ts` 的编译期断言）是它的闸门，CI 在 build 之后运行。
  挡住的是：公开面里声明了实现没有的东西。
- **跳过位置要配「为每个位置都填」的短表**：照官方主循环实现「按匹配长度跳」后，我们拿到
  −37 % 时间，代价是 +2.9 pp 体积（44.82 % vs 41.89 %）；官方只付 0.4
  pp，因为它的 1/2/4
  字节三张表是预计算阶段为**每个位置**填的，跳过的位置照样有短匹配可用。另外这三张表
  必须在整段解析里共享（`ParseBuffers::tables`），按 64 KiB 块新建会每块 memset
  4.5 MB， 213 个块就是 1 秒。
- **一步惰性的「能否延长」判据几乎不触发**：按 `FUN_1400478e0`
  加上「挂起匹配在下一位能否 延长」再决定是否切换，比例 47.55 → 47.58
  %（噪声内）。真正的判据在 `FUN_14004600c`—— 它在 `pos+1`
  上重搜一遍链——不在这一行。
- **官方那三张短表是按位置索引的，不是哈希桶**：`+0xb1e8`（字节）/`+0xb1f0`（u16）/
  `+0xb1f8`（u32）都以 `pos - +0xb208`
  为下标，一次装载就是候选、写入顺序；我们照哈希桶实现时 4 字节表要 4 MB
  且随机写，实测「4 % 时间换 0.29 pp」，删掉后 `2151 → 2065 ms`。同一支预计算 里
  8 个「类」的键是 `h5 / (window / classes)`——**哈希域，不是距离**，ADR 0011
  早先写的 「8 个距离类」是误读。
- **短表改成位置索引只值 1.6 %，钱不在那儿**：把 `prev2`/`prev3`
  从哈希桶改成「区块长度的距离 数组 + 跨区块 `last`
  映射」，顺序写、一次装载取候选，seq 2065 → 2031 ms、归档 2186 → 2164
  ms、比例不变。真正贵的是 4 字节表（4 MB 随机写；删掉换 4 %
  时间）。融合路径每搜索 位置仍 ~350 ns（官方 ~200
  ns），差距在每位置的其余簿记与 `finder.matches` 调用开销，不在下降 （2.9
  步/次）也不在这两张表。
- **跳着搜就必须自己预取桶头**：融合路径（`RAR_RS_INTERLEAVE`）同时压着两张短表、候选与
  token，树的两级访存全掉到 DRAM，每次搜索 243 ns 而 `walkbench` 里只有 15.8
  ns/步；在 `pos + 16` 处 `prefetch_head_for` 后 seq 2192 → 1847
  ms、比例一位不变。串行 collector 早就在
  做同一件事（`PREFETCH_AHEAD`），只是当时没有「跳读」把它逼出来。
- **预取要分两级、且不能推太深**：一级 `prefetch_head_for(pos + 16)`
  只藏住桶头（2192 → 1847 ms）； 再加
  `prefetch_node_for(pos + 16)`（读上一步预热过的桶头、预取该节点的 `son`
  对与输入字节）并把 桶头推到 `pos + 32`，才把 183 ns/次的节点缺失藏住（→
  1734–1784 ms）。都推到 +48/+32 反而退回 1883
  ms：提前读到的桶头在用到之前已被刷掉。
- **稀疏搜索时树的成本是第一步，不是步数**：融合路径 `cut=1` 1208 ms / 50.64
  %、`cut=2` 1293 ms / 49.77 %、`cut=96`（2.9 步/次）1731 ms / 47.93 % ⇒ 第一步
  72 ns/搜索（桶头 + 首节点 各一次缺失）、第二步 15 ns、更深的步 ~84
  ns。桶表位宽（21/22/23/24）在同一噪声带内，不是杠杆。
- **照抄官方的信用节流在二进制数据上不咬合**：`+0x300b8`/`+0x300bc`
  那套计数，实现后 seq 1764/1738 → 1738/1757 ms、比例 47.93 → 47.94
  %，等于没动。原因是我们的融合路径在长匹配
  后整段跳过，字面量少、信用贴上限，节流不触发；官方不跳位置才让它有意义。
- **懒一步的价值取决于「下一次搜索有多贵」**：融合路径的
  park-再比对让每个匹配访两次位 置，seq 1871
  ms；官方主循环是真贪心（`pos += best_len`），改过来是 1465 ms / 49.36 %
  （`RAR_RS_GREEDY`）。第一次改成贪心反而更慢，是因为漏了 rep
  更新——**改解析形态时必须 同时保住 rep 缓存与
  `last_length`**，否则距离码开销会吃掉全部收益。
- **融合路径每位置的单价已经和官方同级，差的是「访问了多少位置」**：`ntoskrnl`
  m3、 `RAR_RS_INTERLEAVE=1` 下 `positions=5334130`、内部 1626 ms ⇒ 440
  ns/位置；官方 `-mt1` 1422 ms 约 3.4 M 次搜索 ⇒ 418
  ns/位置。也就是说拿「每位置更快」做目标已经 没有空间，剩下的 2.3 pp 全在 token
  质量上（我们平均 8.8 字节、官方 ~10.4）。
- **把融合路径的树换成官方式的 5 字节链，两边都赔**：`RAR_RS_FUSED_CHAIN=64`
  实现在 `interleaved_tokens` 里用 `Chains::probe` 取代 `finder.matches`，得到
  43.81 % / 4.2 s；`+RAR_RS_CHAIN_SPAN=1`（距离早退）2.44 s 但掉到 46.90
  %；`+RAR_RS_CHAIN_FIRST=1` （最近优先、首个改进即停）45.05 % / 4.3–4.7
  s。原因两条：链要求每次访问都把**匹配覆盖
  的位置也链进去**（`link_through`，13.9 M 次 ×3 条链），且链候选的长度要用整段
  `match_length_at`
  量，而树的一条下降只比一个字节并收紧上界。树在融合路径里是长匹配的
  唯一来源（`RAR_RS_NOTREE=1` 掉到 52.78 %）。
- **成帧粒度是比例的一个旋钮，别只在解析上找**：官方 m3 的块均 26.9 KB（219
  块）、滤镜 220 条 （类型 0 带通道 / 1 / 2 混用）；我们 55.0 KB（106 块）、52
  条类型 1。判价函数 （`FUN_14004600c`）逆完只值 0.16 pp，所以剩下的 2.9 pp
  更可能在块大小与滤镜上。

## MT 行索引与 solid 成员

- **solid 成员必须走逐块滤镜选择器**：`try_solid_filters` 原来调的是整成员探针
  `lzss_huff::solid_filter_specs`（`filter_transform_wins` + 3 % 余量），它在
  ntoskrnl 上**不选 x86 滤镜**，于是 solid 比自己的非 solid 还大 487 KB
  （7,257,425 vs 6,770,399，官方 6,712,267）。改成先调
  `filter_policy::combined_filter_specs`（与非 solid 同一个逐 64 KiB
  `block_stats` 判据）、失败才回退探针后：6,839,224。**新增 solid 路径时，滤镜
  决策要和非 solid 共用同一个选择器**，否则整条链白丢 6 %。
- **行索引的桶定位要 O(1)**：`collect` 原来每次位置都 `hash → starts[hash]`（4
  MB 随机）再 `partition_point` 二分找桶尾，19 B/位置里有一半是这次定位。 现在
  build 时顺手写 `slot[pos]`（该位置在自己桶里的下标），查询只需
  `index - starts[hash]` 得到可用候选数。collect 2.75 s → 1.55 s（整文件 depth
  64），输出逐字节不变。**多写 6 B/位置的构建成本是赚的**。
- **行索引 build 可以按连续位置区间并行**：分块统计桶计数 → 串行合并前缀和 →
  给每块一个写游标 → 并行填 `positions`/`slot`（块内位置递增保证桶内顺序不变）。
  155 → 82–90 ms。块数上限 8（16 块 123 ms，合并流量压过收益），每块最小 1
  MiB。这是本模块唯一需要 `unsafe impl Sync` 的地方（写区间互不相交），
  改结构体字段时要重新检查这个不变量。
- **MT 切片下限对齐官方的 128 KiB**：官方 wave 粒度是 `threads × 128 KiB`
  （`FUN_14004264c`），我们原来是 2 MiB 地板，13.9 MB 的成员永远只有 7 片，
  `-mt8`/`-mt16`/`-mt24` 同速（waves 600–620 ms）。降到 128 KiB 后 48 片， mt24
  1065 → 620 ms。**还要按整 wave 取整**：`div_ceil(waves * workers)`， 否则 48
  片后面挂一个 16 字节的尾片独占一整波。
- **单成员 MT 不值得喂长表，solid
  链必须喂**：`LONG_RANGE_INSIDE_MEMBER =
  false` 让 `entry_len == 0`
  的成员跳过采样长表——比例一个字节不变（行索引本身 就能覆盖整个窗口），wall 省
  10–12 %。但**上一个成员的字节不在 `data` 里**， 行索引看不到，solid
  链只能靠长表；早期"直接关掉"的写法会把
  `solid_members_share_the_window_through_mt` 弄挂。
- **第 5 字节过滤必须进 hash，别做候选循环里的过滤**：给 `RowIndex` 加一条与
  `positions` 对齐的 `key` 流（5 字节 hash 的高 8 位），在候选循环里 `continue`
  是安全的（前缀不同 ⇒ 长度 ≤ 4 ⇒ 不可能超过 `best_length`），但在 ntoskrnl 上
  几乎从不命中，多一条 14 MB 流反而 1.56 s → 1.96 s。要省得让 hash 本身吃掉第 5
  字节。
- **桶位数不是 MT 的速度杠杆**：`ROW_INDEX_HASH_BITS` 20 → 21 后 `waves` 仍是
  300–305 ms（收集成本被内存流吃住，不是被候选数），`row_index` 反而 85 → 125 ms
  （计数表 8 块 × 8 MB），总体 wall 616–649 → 615–628 ms、体积 5,911,145 →
  5,910,583。21 位唯一的好处是 20 位的哈希碰撞会占掉 `depth` 预算（碰撞项在
  长度检查里被丢掉），所以更高 depth 下差距会放大；为这点收益付双倍的 build
  内存不划算，留在 20 位。
- **小成员的 MT 不是「开了就赚」**：`encode_with_filters_mt` 的
  `RAR_RS_MT_MIN_LEN` 默认 8 MiB，看着保守，但把 ntdll（2.5 MiB）放进来是 593 →
  176 ms（3.4x）却 990,282 → 1,014,263（**+2.4%**）。拆两项：升 `MT_DEPTH` 64 →
  128 → 256 是 1,014,263 → 1,009,464 → 1,005,938（187 / 238 ms），即**候选质量占
  0.8 pp** （行索引有界 depth vs 顺序路径的 BT4 树）；剩下的 1.6 pp 在切片本身——
  2.5 MiB 被切成 24 片 ×105 KiB，每片一个 DP 块、价格从头估，而顺序路径 4 MiB
  一个 chunk 只起一次价。想在 1–8 MiB 档拿到 MT 的速度，得先做**跨片价格携带**
  或按整成员频次预算价，单纯降 `MT_MIN_LEN` 是拿比例换时间。
- **滤镜通道扫描里别用 `i % channels`**：`block_stats` 选 delta
  通道时对每个字节取模 选 lane，11 个通道 × 全成员 = 1.5
  亿次整数除法，成了编码前最大的一块串行成本。 改成「按 lane 起头、步长 =
  channels」的走法后算术完全等价（同一 `prev` 初值、
  同一比较顺序，体积逐字节不变 5,911,145），mt24 wall 616–652 → 570–603 ms。
  写这种"每字节多次扫描"的判据时，先确认循环里没有除法/取模。
- **别为了「省一次随机读」去查桶起点表**：`collect` 原来用
  `slot[pos] - starts[hash(pos)]` 算可用候选数，每位置要随机读一次 4 MB 的
  `starts`；改成与 `positions` 平行的 `avail: Vec<u16>`（每位置同 hash 前驱数，
  饱和到 u16::MAX）后是一条顺读流，**位相同**（mt1 5,780,169 / mt24 5,911,145
  不变），mt8 876 → 772–792 ms、mt16 682 → 657–673 ms、mt24 570–604（噪声内）。
  build 端多写 2 B/位置（row_index 80–90 → 92–115 ms），净赚。
- **别裁剪 delta 通道候选集**：把 `BLOCK_DELTA_CHANNELS` 从
  `[1,2,3,4,5,6,8,9,12,16,32]` 收到 `[1,2,4,8,16]`，mt24 只从 578 → 561 ms （−17
  ms），体积却 5,911,145 → 5,954,837（**+43.7 KB / +0.74%**）——ntoskrnl 的块里
  3/5/6/9/12/32 通道确实会赢。这条成本不能靠"少扫几个通道"来省，
  要省就得像官方那样少发记录（判据层面），而不是少算候选。
- **官方的提前退出不能照搬到代价 DP 上**：`FUN_1400468d4` 里
  `best > 2 && distance > best * 0x4000 && length < best + 5` 就直接 return，
  因为它是**贪婪**解析、远距离同长度匹配不值得。我们照搬进 `collect` 后 ntoskrnl
  mt24 体积 5,911,145 → **6,113,355（+202 KB / +3.4%）**，速度没有变好 （waves
  362–386 vs 基线同批次 374–460）。代价 DP 会用"短而近"换"长而远"，
  砍掉远候选等于砍掉它的选择空间。要照搬官方行为，得连解析形态一起换（见 ADR
  0012 的两段式），不能只搬退出条件。
