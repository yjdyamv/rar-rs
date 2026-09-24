# CONTEXT — rar-rs

> 最后核对：2026-09-22 @ `fbe2f8c`；实现细节以源码为准。

领域词汇（本仓库术语的单一来源）。给架构审查和后续 skill 使用；新术语先查这里，
模糊了就地改。非词汇信息（模块地图、工程状态、限制）不放这里，见文末指针。

## 领域词汇

- **Archive（归档）** — 一个 RAR 归档：单卷文件或 `.partN.rar` 分卷集。RAR5
  容器（8 字节签名）或 RAR4 老容器（7 字节签名）。
- **ArchiveVersion（归档版本）** — 2026-09 收敛后的**单一版本表**
  （`version.rs`；废弃 `ArchiveFormat` 公共容器轴与 `CompressionVersion`，见 ADR
  0004）：`V14/V15/V20/V26/V29/V36/V50/V70`。**容器由版本推导** （`is_rar13()` /
  `is_legacy()` / RAR5），不再有公共容器类型。写侧可写子集
  `{v14, v15, v20, v29, v50, v70}`（`is_writable()`）；**v26（同 v20 codec）与
  v36（同 v29 codec）只读**，validate 报 `InvalidOption`。**LegacyCodec**
  （`version.rs`，`pub(crate)`）是 legacy 成员 codec 的单一身份：`from_unp_ver`
  折叠只读别名（15→Rar15、20/26→Rar20、29/36→Rar29），读解码与密码、solid 链判定
  与重建、写编码/加密/批处理、repack 都按它分派，不再各自匹配 raw
  `unp_ver`。读侧 经 `ArchiveEntry::version()` 报告。
- **Member（成员）** — 归档中的一个条目（文件 / 目录 /
  重定向），对应一个文件头 + 数据区。
- **Volume（分卷）** — 多卷归档的单个 `.partN.rar` 文件；成员数据按卷切成
  Chunk。
- **Chunk（分块）** — 跨卷成员在某卷中的数据段。非末块头携带该块密文
  CRC32；末块携带（hash-key MAC 过的）明文 CRC，并携带完整 extra 记录。
- **Solid chain（固态链）** — 连续压缩成员共享一个 LZ
  窗口；EncoderState/DecoderState 跨成员保持。单卷与分卷均已支持。
- **SolidChainState（`transaction/solid.rs`）** — RAR5
  外科重写共享的固态链状态与管线：`start(window)` 按链头成员经
  `member_dict_window` 解析出的真实字典建共享 DecoderState/EncoderState（v70 按
  `dict_size_bytes`，非 4-bit 字段），`decode_member`（按成员声明窗口按需
  `grow_window` 后推进窗口，删除成员用）/`recompress_member`（解码 + 重压 +
  STORE 回退 + 加密 + 发射；保留原成员 FILE_TIME/OWNER 记录剥掉旧
  ENCR/HASH，且只对真正加密过的成员重新加密——无关口令不再污染明文成员）经
  `ChunkReader` seam
  同时服务单卷（`SingleFileReader`）与多卷（`VolumeReaders`）重写；`read_member_packed`
  是两路径共用的打包读取。编辑时的链范围 `edit.rs::chain_range_around`
  与读取侧同规则（目录透明：向前只跨 `comp_solid`
  参与者，锚点跳过目录；链内目录的复制仍进 QO 记录）（2026-09）。
- **EncoderState / DecoderState** — 跨块/跨成员保持的编解码状态（lookbehind
  tail、dist cache、last length、Huffman 表），定义在
  `codec/modern/lzss_huff/encoder/`（`mod.rs` 共享词汇 +
  `chunked`/`parse`/`emit`/`filter` 角色模块）与
  `codec/modern/lzss_huff/decoder/`（`mod.rs` 共享词汇 +
  `symbols`/`engine`/`analysis`/`tables` 角色模块）；DecoderState = 窗口 +
  SymbolState。解码距离经窗口校验（0 或越窗即 `Format` 报错，`copy_match`
  不再静默掩码别名；RAR7 扩展表的 34 位字段经 `read_bits_u64`
  不截断）（2026-09）。
- **SymbolReader / SymbolState（`decoder/symbols.rs`）** — RAR5/RAR7
  符号流唯一状态机：块框架、头校验和、Huffman 表刷新、dist cache/last
  length、filter 记录解析，产出 `Literal` /
  `Match{dist,len,kind: Match/Cache/Repeat}` / `Filter` /
  `BlockStart`。`engine`（窗口+输出）、`analysis` /
  `trace_stream`（统计/追踪）都只是它的消费者；成员解码只有这一条状态机与一条
  engine 循环，无 buffered 双核（2026-09）。
- **Emitted block（发射块）/ parse block（解析块）** — 压缩流两种块：写侧把 LZSS
  符号流切成**发射块**（≤ 4
  MiB，局部字面量/距离/长度分布漂移时提前闭合，每块独立 Huffman
  表）；解析/预算侧分块上限仍 128
  KiB（`MAX_BLOCK_SIZE`）。发射块大小与解析块解耦（自适应发射块，2026-09）；策略单一
  owner：`EMITTED_BLOCK_SIZE`（4 MiB）与 `find_block_end_adaptive` 同驻
  `codec/modern/lzss_huff/encoder/parse/block.rs`，普通/滤波器、顺序/MT 四条管线
  共用（`parse/` 另分 `collect.rs` 匹配收集与 `optimal.rs` 定价解析）。
- **MemberDecoder** —
  `format/rar5/payload.rs`：统一成员读/解码门面（`ChunkReader` trait +
  `read_packed` + `decode_member`），STORE
  直通与压缩解码共用；串行、并行（`extract_all_parallel`）与 RAR5
  外科重写三条路径都调它。
- **Spill file（溢出文件）** — 大文件（≥ `STREAM_COMPRESS_THRESHOLD`，64
  MiB）压缩路径的临时落盘文件：压缩流先溢出，头写出后再流式进归档，保证内存有界。
- **Streaming payload（流式负载）** —
  `write_streamed_payload`（`format/rar5/write/stream.rs`）：统一流式写路径（单卷/分卷 +
  可选流式 AES-256-CBC）；`write_store_member` 是其 STORE 特例（明文 STORE
  不声明字节字典，与旧 `write_stored_file` 一致）。
- **MemberPlan（`engine/plan.rs`）** — 成员发射的命名值：文件头字段 + extra
  记录（`push_extra` 追加 FILE_TIME/OWNER）；内存 `write_file_entry`、流式
  `write_streamed_payload`、`write_store_member`
  与多卷切分共用同一引用，`PreparedEntry`（batch）= plan + payload，原
  `SplitParams` 已并入（2026-09）。零长度成员（空文件经流式 STORE
  路径）也发射文件头（`write_split_member` 的空成员特例）；成员首个 chunk
  落卷前换卷时不置 `DATA_CONTINUES`。
- **Multi-volume rewrite（`transaction/multivolume.rs`）** — RAR5
  分卷重写：保留成员按原压缩载荷在新卷上限处重新切分，固态链重压（`SolidChainState`），`.rev`
  一起 journaled 提交；归档注释（CMT）读出后在重建主头后原样再发射，存活成员的
  "STM" 流记录经 `read_streams_with` 解码后由 `write_stream_record`
  重新发射（原加密流重新加密、明文流保持明文），主头声明分卷但只发现单卷（缺卷）的任何编辑拒改（`edit_plan`
  的 `main_header_declares_volume_set` 守卫，2026-09）。
- **StagedFile / StagedSet / StagedCopy（`fs/atomic/`）** — staged
  写入的所有权值：`StagedFile`
  创建即原子命名（`read_write_create`，绝不截断既有文件）、`commit()` 走
  `install_durable`、未提交时 Drop 自动清理（RAR5 单卷编辑重写、RAR5/legacy
  恢复修复走它）；`StagedSet` 是 journaled 多文件事务（开集时自动
  `recover_interrupted_commit`、`track`/`park`/`commit`、Drop 清理未提交
  staged；`park` 先把既有 final 记入 journal 再改名，回滚还原、成功保留（rev3
  损坏卷 → `*.bad`），kill 落在 park 与 install 之间也由 recovery
  还原）。**`StagedCopy`（公开）** 是「拷贝原件 → 在副本上操作 → `commit` 一次
  durable 安装」的值：CLI `rar u`/`f`/`a` 的替换事务走它，未提交即 Drop
  清理。创建/append/多卷 set/`.rev` 构建仍用 `install_durable`/`commit_files` +
  `PendingCommit`（2026-09）。
- **NTFS stream（ADS，`-os`）** — 成员附属的 NTFS alternate data stream，存为
  owner 之后随的 "STM" 服务块（`DEPENDS_PREV`，明文 CRC32；`-p`/`-hp` 时每流独立
  ENCR 记录 + 加密载荷，CRC 不 MAC）。写侧
  `write/stream.rs::write_member_streams`（Windows 枚举，batch
  并行自动退回顺序）；读侧 `StreamRecord`（`engine/state.rs`）+
  `extract/decode.rs::read_member_streams`（读取时校验口令/派生密钥、解密、CRC
  校验；锁定档仍可列表）。
- **Mark of the Web（MOTW，`-om`）** — 浏览器给下载文件打的 `Zone.Identifier`
  ADS；`-om` 把归档文件自身的该流传播到解出的成员（默认只保留 `ZoneTransfer`
  区与 `ZoneId=`，`1` 全字段，可按扩展名过滤；Windows only）。库侧
  `options::MarkOfTheWeb` 作为 `ExtractOptions::mark_web`（逐次抽取的策略数据，
  与 `threads`/`skip_existing` 同处），`rar`/`unrar` 双侧接线（输出与 WinRAR
  逐字节一致）。
- **CbcRangeEmitter** — `format/rar5/write/engine.rs` 中连续 CBC
  密文按任意字节区间发出的机制（read-ahead 到块边界 + ≤15B
  carry），使加密分块边界任意、卷大小仍精确（与 WinRAR 字节级一致）。
- **Header encryption（-hp）** — 归档级加密头（每卷开头明文），其后所有块为
  `[IV][AES-256-CBC 加密头]`；AES
  密钥每归档派生一次并缓存（`archive_header_key`，多卷同
  key），`write_block_header`/locator patch/lock 复用（2026-09）。
- **MemberEncryption（`crypto/rar50.rs`）** — 写侧每成员加密会话：随机 salt/IV +
  一次 KDF 派生的 `DerivedKeys`；`mac_crc32`/`mac_hash32`/`encrypt`/`key_iv`
  复用同一份密钥（此前每成员派生 3–4 次），STM 服务记录走
  `generate_with_flags(.., ENCR_FLAG_CHECKSUM)`（2026-09）。
- **Recovery record（恢复记录）** — 内联 "RR" 服务块，奇偶校验保护归档前缀
  （GF(2^16) Cauchy 矩阵，见 `recovery/rar50/`：`plan`/`gf16`/`encode`/`repair`/
  `stream` 角色模块）。**分卷时每卷各带一份**（2026-09-24 对拍官方 7.23： `-rr`
  配 `-v`，每卷记录只保护该卷自己的前缀，且卷内预留其字节以保证不超过
  `volume_size`）；创建与分卷重写共用卷收尾 `create.rs::finish_volume`。legacy
  RAR4 同形（每卷一条 NEWSUB `Protect+`）。
- **Recovery volumes（.rev 恢复卷）** — 分卷集的 Reed-Solomon
  奇偶校验卷，可重建缺失/损坏卷（`rar rv`/`rc`）。RAR5 用 REV5
  容器（`recovery/rev50.rs`，GF(2^16) Cauchy + 每卷 CRC/大小表）；RAR 1.5–4.x 用
  `recovery/rev3/`（GF(2^8) `rs8.rs` + 名称/尾部元数据）：trailer 布局（末 7
  字节 = `data-1/rec-1/index/CRC32`，只保护 `len-7`，重建尾 7 字节置零；新命名
  `base.partNN.rev`，老命名 `baseN.rev`）（`.rev` 文件名**解析** ASCII
  大小写不敏感，但候选要对现存数据卷评分，所以"这个名字属于哪个 base"最终跟随
  文件系统的大小写语义：Windows 下 `SET44_2_1.REV` 能配 `set.rar`，POSIX 下
  不能（官方工具在 POSIX 同样不配）；base 以数字结尾的歧义（`set44_2_1.rev` →
  `set`+44 或 `set4`+4）按现存数据卷评分消解，stale 清理复用同一判定）与 legacy
  全量奇偶布局（`base<data>_<rec>_<idx>.rev`，新命名带 `.part` 中缀）；WinRAR
  按卷尾是否为零字节选择布局，我们逐字节一致；损坏卷用 syndrome+Berlekamp-Massey
  定位后改名 `*.bad` 重建。
- **ParitySet（`recovery/parity.rs`）** — `.rev`/重建卷的 staged
  安装值：`stage(final)` 建 temp sibling + 写句柄、`commit()` 一次 journaled
  安装（拒非文件 final、失败自动 sweep、成功返回 final 路径）；REV5
  `.rev`、legacy `.rev`、`rc` 重建卷三条构建路径共用同一生命周期（2026-09）。
- **Quick-open（QO）** — 主头 locator + 末尾 "QO"
  服务块，缓存文件头副本加速列表。payload 布局唯一 owner
  `format/rar5/headers/quick_open.rs`（`encode_entry`/`decode_payload`：条目
  CRC、尺寸→usize 校验；decode 只出 `(rel, header bytes)`
  对）；`extract/open.rs` 只负责把 rel 折算成
  data_offset、条目上限与文件头块解析。
- **BLAKE2sp / hash-key MAC** — 成员哈希记录（`-htb`）；加密成员的校验和用 hash
  key MAC 保护（`format/rar5/blake2sp.rs`、`crypto/rar50.rs`）。
- **Redirect（重定向）** — symlink / hardlink / file-copy 成员（无数据区，仅
  extra 记录）。写侧携带链接自身 mtime（秒 + 非零纳秒进 FILE_TIME
  extra，`add_redirect_with_time`）；`-ol` 下目录 symlink/junction 也存为
  redirect（不跟随目标），Windows 写 2/3 型（symlink/junction）与官方一致；官方
  `-sfx` 前置于 RAR 1.3/1.4 会被拒绝（官方只认 DOS stub）。
- **SFX** — 归档前带 stub 的自解压文件；`detect::sfx_offset_of` 定位归档起点。
- **Locator（定位器）** — 主头中的 QO/RR 偏移记录，close
  时回填。**官方默认模式下 恒写**（2026-09-23 起我们照此）：无 QO 记录时 QO
  字段照样发、偏移写 0 占位，RR 只在
  有恢复记录时出现；读侧（`split_main_extra`、QO 快路径）把 **0
  偏移**当「无记录」。 主头唯一构造者
  `headers/locator.rs::build_main_header`（把 locator 追加到调用方 extra 后、经
  `ArchiveHeader::to_bytes` 发射，并返回 QO/RR 字段的 header
  相对偏移），`patch_locator_fields`
  负责原地回填；调用方不再手数字段宽度。偏移字段是定长 5 字节 vint（35
  位）：超过 32 GiB 无法命名的偏移写入哨兵 0（QO 退化为全扫、RR
  视为无记录），不再静默回绕（2026-09；官方按写头时的预计大小预留 3–6 字节，见
  `PLAN.md`「已知小差异」）。
- **RAR5 block envelope（`frame_block`）** —
  `format/rar5/headers/serialize.rs`：`[CRC32 LE][size vint][body]`（CRC 覆盖
  size vint +
  body）的唯一发射者；所有头序列化器与外科重写路径都经它（2026-09）。
- **Service 块解析** — `format/rar5/headers/parse.rs`
  独占：`parse_service_block_name`（块名 QO/RR/STM/CMT，截断返回 `None`
  不再手走字段）、`parse_service_recovery_percent`（RR SUBDATA
  单字节）、`parse_service_subdata`（SUBDATA 载荷，STM 流名）；extract 扫描与
  archive 事务消费同一实现。
- **MainHeader opener（`read_main_header`）** — 归档起点唯一打开者：可选明文
  ENCR 头（校验口令、保留后续块密钥）+ 主头，返回
  `MainHeader { meta, parsed, encrypt_header }`（encrypt_header
  供重写原样再发射）；append/lock/rewrite plan/锁定检查四条路径共用，ENCR 分支与
  "missing the main header" 错误只存在一处（2026-09）。
- **BlockCursor（`headers/parse.rs`）** — 单文件 RAR5 块遍历器：固定
  key、逐块跳过 data area（越出文件即停）、END 返回一次后终止；append/rewrite
  plan/get_comment 三处循环共用同一文件长度约束（2026-09）。
- **Platform metadata（`platform.rs`）** — 按宿主平台写 RAR5
  成员元数据的唯一出处 （2026-09-23）：`host_os()`（Windows 0 / Unix
  1）、属性（Windows 取 DOS 位、目录 `0x10`、symlink `0x420`、junction
  `0x410`、hardlink/copy `0x20`；Unix 取 `st_mode`）、 时间载体
  `file_time_is_windows()`（Windows 上清 `FILE_FLAG_TIME_UNIX`、时间放进
  FILE_TIME 记录并写 Windows FILETIME；`-ts1` 仍用 unix 秒）。`engine` 与
  `format`
  都经它取头字段，因此同一输入在两平台各产出与官方对应平台一致的元数据。RAR4 的
  DOS 属性字段也走这里（`rar4_file_attributes`/`rar4_dir_attributes`：Windows
  直拷 文件属性、非 Windows 落 `0x20`/`0x10`）。
- **ExtractionReport（`format/shared/extract/members.rs`，经
  `archive/reader.rs`）** —
  抽取操作的唯一回报值（2026-09）：`written`（真正写出的文件与创建的链接，按归档序）+
  `skipped`（`-o-` 未动的成员，带目标路径）+
  `refused`（目标逃出目的目录、被安全策略拒绝的链接 ——
  只拒该链接、不中止整轮，CLI 按 WinRAR 记 exit
  1）；目录条目不记（创建无文件数据），`-ol-`
  跳过的链接也不记。`extract_all_with_options`/`extract_ids_with_options`
  返回它，写入循环自己记录，因此不可能与落盘不一致；CLI 的 `Skipping` 行与
  `Extracted N file(s)` 计数直接来自它（预测式
  `count_extracted`/`destination_key`/`taken` 已删）。按 id 抽取同样受
  `max_total_unpacked_bytes` 约束（与整档一致）。
- **ExtractRequest（`crates/rar-cli` ops.rs）** — 两二进制四个 `x`/`e`
  臂的**唯一** 抽取请求值；落盘选项组装只此一处，`-so` 走 `extract_to_stdout`
  自己的选项。 **磁盘抽取是流式的，尺寸上限默认不限**（与官方一致），但可用
  `--max-unpacked` / `--max-total-unpacked` 收紧；字典上限始终保留（WinRAR
  默认拒 >4 GiB 字典，`-mdx` 可抬）。`-f`/`-u` 映射为
  `freshen`/`update`（按归档与目标 mtime 比较；freshen 跳过缺失目标、update
  解出它们；显式 `-o-` 仍优先），设置任一者即替代 CLI 的非交互 skip-existing
  默认。全部选中成员被跳过时报 `No files to extract` 并 exit 10。
- **Catalog identity（EntryId / catalog token）** — `EntryId` 由
  `ReadState.catalog_token` + 目录序号 + 首 chunk 的 packed
  偏移构成；`EntryId::resolve(entries, token)`
  是读/编辑两个门面共用的唯一解析约定（token 不符即 `StaleEntryId`；同 token
  下先按序号命中、再按 packed 偏移定位，quick-open 重排仍指向同一成员）。token
  只存于 `ReadState` 一处、`RarArchive::reset_catalog_token` 一处轮换（open 与
  `ArchiveEditor::apply`/`apply_rar4` 成功后），编辑器不再自持
  token（2026-09）。
- **Quick-open fast path（QO 快路径）** — `RarArchive::open_quick`：只读主头
  locator + QO 记录即得成员列表（O(QO) 而非 O(归档)）；无 QO 时透明回退全扫。
- **CatalogBuilder（`format/rar5/extract/open.rs`）** — RAR5
  成员目录的唯一扫描器：单卷（`self.stream` 作唯一 source）与分卷（逐卷 `File`
  source）走同一条 `scan_source`，continuation 合并、条目/chunk 上限、STM
  owner+volume、ENCR 每卷密钥重派生都只此一处；`rebuild_catalog(_capped)`
  负责定位与装配（2026-09）。
- **Streaming repair（流式修复）** — `repair_archive_path(src, dst)`：文件版
  `{RB}` 扫描 + shard
  级按需读取，只驻留恢复数据与损坏分片；完好不写输出、失败不残留。
- **Cancel flag（取消钩子）** —
  `set_cancel_flag(Arc<AtomicBool>)`：长操作在逐成员/逐块检查点返回
  `RarError::Cancelled`；binding 映射 AbortSignal。
- **Zero-padded volumes（零填充卷）** — WinRAR
  把卷号填充到总卷数位数（`part01..part15`）；发现/重建/.rev 命名均识别。
- **Rar13 family（RAR 1.3/1.4，`RE~^`）** — 4 字节签名的 DOS 时代容器
  （`format/rar13/`）：7 字节主头（**无头 CRC**）+ 21 字节固定文件头、16 位滚动
  校验、`LHD_PASSWORD` 走加性流密码、注释在主头扩展与
  `LHD_COMMENT`；成员解码复用
  `Rar15Decoder`。**写侧（2026-09）**：单卷与旧命名分卷（`base.rar` /
  `base.r00…z99`，**上限 901 卷**，超出 `InvalidOption`）；**无
  ENDARC**；成员跨卷 时每片重复文件头（中间片存累计 packed
  校验、末片存整成员校验）；`-p` 整段加密后
  再切片（**阅读侧也在拼装后才解密**）；卷体精确填满
  `volume_size`。**明确拒绝**： 字典 / quick-open / recovery / `-hp` / owner /
  streams / blake2 （`validate_rar13_only`），append 与
  editor（`Unsupported`），以及 `-sfx`。CLI 面：`-ma13` / `-ma14`、`-z`
  建前排队。
- **Legacy family（老容器族）** — `Rar!\x1a\x07\x00` 7 字节签名的 RAR 1.5–4.x
  容器（`format/rar4/`）：固定宽度头 + 16 位头 CRC（**ext-time
  尾不在覆盖内**）。 DOS 时间按「本地 civil 秒」存：写侧把 Unix
  即时转本地后打包（2 秒精度，奇数秒经 ext-time `ADD_SECOND`
  补回），抽取时转回；**`-ts-` 只对 RAR5 省略时间**。读写路径 见
  `format/rar4/{mod,read,write}`。**写侧全能力（2026-09）**：LZSS m1–m5 + PPMd
  （含 solid 链模型延续）、六大标准 VM 过滤器、`-hp`、NEWSUB 0x7a RR 恢复记录、
  非 solid 多文件并行 batch（字节与顺序一致）、v15/v20 老编码器（`-p` 按版本分派
  RAR15 流 XOR / RAR20 块密码，**均无盐**）。**solid 重置**：`-se` 仅 v29 保留；
  pre-RAR3 的 `-se` 与全 legacy 的 `-sv` 由 `validate_solid_reset`
  拒绝。多卷发现 `discover_volumes` 支持 `.partN.rar`（新命名）与
  `.rar/.rNN`（老命名，任意卷 入口）；solid repack 重发成员时**保留原 DOS
  属性字节**。
- **RAR4 block envelope（`format/rar4/envelope.rs`）** — RAR 1.5–4.x
  块头的**唯一 读取器**（2026-09）。`EnvelopePolicy` 四态：`SCAN`（校验
  CRC、不留原始字节：列表
  扫描）、`PLAN`（不校验、不留：头已被前次扫描校验过）、`EDIT`（不校验、**保留原始
  字节**：字节级重写）、`REPAIR`（不校验、不留：损坏归档扫描）。**`-hp` 由调用方
  逐块闩锁后传 `encrypted`；加密块读越界一律 `WrongPassword`，明文头读越界一律
  `Format`。**
  消费方：`mod.rs`（SCAN）、`rar4_edit/{layout,comment}.rs`（PLAN）、
  `engine.rs`（EDIT）、`recovery/{legacy.rs,rev3}`（REPAIR；**rev3
  拿不到口令、按 明文走，`-hp` 缺口已知不再修**）。
- **Rar29Decoder（`codec/legacy/rar29.rs`）** — RAR 3.x/4.x（unp_ver ≥ 29）成员
  解码器（rars 解码半移植；位读器/规范 Huffman/滑窗走
  `codec/legacy/lz.rs`）。成员 = 块序列，块头（byte 对齐）是 PPMd 标记或 LZ
  头（keep-tables 位 + 可选新表），
  块间可混切模式；成员尾消费块控制符（`SameFileNewTable` / `NewFileKeepTables` /
  `NewFileNewTables`，PPMd 走 esc 结束符）——**solid
  链跨成员靠它续表/换表**。窗口 保留 ≤ 4 MiB（`MAX_HISTORY`）。VM 过滤记录解析为
  `VmFilter` / `VmProgram`：标准 过滤器按指纹识别并原生执行，其余字节码由
  `codec/legacy/rarvm.rs` 解释，globals 跨调用持久化。
- **Rar15Decoder（`codec/legacy/rar15.rs`）** — RAR 1.5（unp_ver
  15）解码器（rars `Unpack15` 近逐字提取）：标志位驱动 LZ + 自适应
  Huffman（`ch_set*`/`n_to_pl*` 表随解码自组织）+ st 运行模式，64 KiB
  环窗（`window`/`unp_ptr`）；流以 `new_final`
  结尾标记读取（尾部零填充）。`solid` 参数保留窗口/表，rar4 读侧按归档级
  MHD_SOLID 跨成员链接（见 Legacy solid chain）。写侧为 rars 移植的
  `rar15_encoder.rs`（`Unpack15Encoder`，见 ArchiveVersion）。
- **Legacy LZ core（`codec/legacy/lz.rs`）** — RAR 2.x 与 3.x/4.x
  解码器的共享核心（2026-09）：`BitReader`（MSB 位读器，含
  `align_byte`/`peek_bit`/`from_bytes`/`read_encoded_u32` 与 `PpmdByteReader`
  impl）、规范 `Huffman`（`from_lengths`/`decode`/`is_empty`）+
  `validate_huffman_counts`、`fill_levels`、`History`（滑窗：`current_pos` /
  `raw_byte` / `raw_range` / `push` / `trim` / `copy_match` /
  `drain_pending_match`（`trim(flushed_pos)` 返回窗口起点，rar29
  据此清掉落在窗后的 VM filter），越窗匹配零填充与 pending match
  语义只此一处；`limit` 按族传入——RAR20 1 MiB / RAR29 4 MiB）与共享
  `Error`（`Truncated`/`Bad`，`into_rar(stream)`
  加族标签）。两族保留各自的表形状、`read_tables`/level 读法、音频、PPMd、VM
  filter 与 streaming shell；`rar15`/`ppmd` 仍自含。
- **Legacy encode core（`codec/legacy/encode_core.rs`）** — RAR20/RAR29
  编码共享底座（2026-09）：LENGTH/SHORT 表、槽窗口查找（`slot_for` /
  `length_slot_for_match` / `offset_slot_for` / `short_slot_for_match` /
  `match_length_adjustment`）、`push_old_offset`、`LevelToken` + `LevelAlphabet`
  trait（`Rar20LevelMap`：字面量 level、单一 repeat 形态；`Rar29LevelMap`：base
  delta、长短 repeat 形态）与通用 level 表 token 生成；两个编码器共用槽/level
  机制（解码器共用其中的表与 offset
  环），输出经差分探针验证与拆分前逐字节一致，并有逐 level 的字节 golden 测试。
- **Rar20Decoder（`codec/legacy/rar20.rs`）** — RAR 2.x（unp_ver
  20/26）LZSS+Huffman 解码器（rars 解码半移植）：位读器/规范 Huffman/滑窗走
  `codec/legacy/lz.rs` 共享核心；块头 16 位 peek——bit15=**音频块**（每通道
  Huffman 表 + 自适应 delta 预测，`AudioState`），bit14=keep-tables，其余为 LZ
  块（主 298 符号：256 重末匹配/257–260 旧偏移/261–268 短距/269 块尾/270–297
  全长匹配）；level 长度 19×4bit 直读（无 RAR3 的 0xF 逃逸）。成员尾
  `read_last_tables` 消费块尾标记以续链。solid RAR2.x 链读侧已支持：常驻
  `Rar20Decoder` 跨成员保窗/保表（见 Legacy solid chain）。写侧为 rars 移植的
  `rar20_encoder.rs`（`Unpack20Encoder` + 自含 `Rar20MatchFinder` +
  音频块编码，见 ArchiveVersion）。
- **PpmdDecoder（`codec/legacy/ppmd.rs`）** — PPMd 变体 H 解码器（rars
  解码半移植，编码器不移植）：Suballocator（12 B 单元、双端 bump + 空闲桶 +
  glue）+ 上下文模型（contexts Vec 模拟 C 指针布局）；`decode_init` 由块头 init
  byte（reset/阶/字典 MB/esc 标记）重启模型，`decode_symbol` 出符号。错误走自带
  `Error`（InvalidData/NeedMoreInput），rar29 侧 From 映射。
- **Legacy solid chain（老固态链）** — `format/rar4/extract.rs` 对 RAR4 用常驻
  legacy decoder（`ReadState.legacy`）+ decoded-through 索引镜像 RAR5
  链解码；链内 STORE 成员断链（窗口重开）。solid 排序由 WinRAR
  决定，链起点=归档首文件。
- **路径约定（host path vs archive name）** —
  **主机路径**（磁盘文件、目标目录、卷
  文件、`-w`/`-op`/`--dest`、位置式目标目录）一律走 `std::path`，分隔符判定用
  `std::path::is_separator`（`/` 全平台、反斜杠仅
  Windows）；**禁止手写反斜杠字面量 判定，CI 有 grep
  防护**。**归档成员名是格式空间**：RAR 用反斜杠作分隔符，内部统一 规范化为
  `/`（写侧、`safe_path`、`selector`、展示都做该替换），与主机平台无关； 因此
  Unix 文件名里的反斜杠无法与目录分隔区分（与官方一致，不做特例）。

## 分层结构与项目事实

本文件只维护**领域词汇**（上层术语）。模块地图、分层、设计笔记与 CLI/测试布局见
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)；工程状态、下一步与限制见
[`PLAN.md`](PLAN.md)（不再在此重复）；测试怎么跑见
[`docs/testing.md`](docs/testing.md)；文档导航见
[`docs/README.md`](docs/README.md)。
