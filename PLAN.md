# rar-rs 后续计划（TODO）

随意记录，想到哪写到哪。完成一项就划掉。

## 已完成 ✅

- **ENDARC block flags 修复**（dd5b456）：分卷集的 7-Zip 互读（"data after the end of archive" 根因）
- **加密分卷每块加密记录**（dd5b456）：非末块 flags=1（密文 crc32）、末块 flags=3（MAC），与 WinRAR 字节级一致
- **分卷 + header 加密（-hp）**（d8f0f63）：每卷开头明文加密头 + [IV][AES-CBC] 块；精确卷大小（目录项配额记账修复）
- 多卷 + 密码不再静默失效（binding 侧透传，smart-archive-rar 0.3.3/0.3.4）
- **P4：>4 GiB 单文件创建 RAR**：压缩路径流式化（≥64 MiB 走 spill 临时文件：分块读 → 压缩（encoder state 跨块保持）→ 加密（CBC 跨块续链）→ 按 volume_size 切分）；加密 STORE 也流式化；非末块密文 CRC 边写边算；卷大小仍精确（加密分块边界任意、与 WinRAR 一致，靠 read-ahead + ≤15B carry 实现）
- **读取端支持 -hp 分卷**：`scan_all_volumes` 每卷解析明文加密头并解密后续块；顺带修了加密分卷读取时条目元数据取自首块（flags=1 无 MAC 位）导致校验失败的潜在 bug——末块携带完整 extra 记录，读侧合并之
- **Windows 互操作测试**：`tests/interop.rs` 现在 Windows 可编译（symlink 测试加 `#[cfg(unix)]`，mtime 量化测试改为按磁盘实际值断言）；新增 `tests/winrar_interop.rs`（自动定位已安装的 WinRAR，Rar.exe/UnRAR.exe 双向验证，含 >4 GiB 稀疏文件用例，慢测 `#[ignore]`）
- **架构审查 + 9 项 deep-module 重构**（依据 mattpocock/skills，装到 `.claude/skills/`）：
  1. 块信封读取统一为 `headers::read_block`（5 份副本 → 1，修 CRC 分歧 bug，测试扫描器改走库 seam）
  2. extra/服务块序列化归位 `headers.rs`（含统一 `build_service_block`）
  3. 错误模型结构化（`MemberNotFound`/`ArchiveLocked`/`WrongPassword` 变体）
  4. CLI 平面提取 `..` 逃逸漏洞修复 + `-ep` 等命名策略归库（`rar5::name_policy`）+ update/freshen 合并
  5. 内存 stream seam（`Box<dyn ArchiveStream>`，`create_with_sink`/`finish_into_sink`；顺带修 Windows `File::create` 只写句柄读回失败）
  6. 解码器入口收窄（`DecodeOptions`，`DecoderState` 字段私有）
  7. 写管线提取 `src/write.rs`（~1900 行）
  8. 重写子系统提取 `src/rewrite.rs`（~1500 行）；archive.rs 7500 → 4200 行
  9. 公开面收窄（write_progress 转私有、删死变体、13 个 create* 构造器 `#[deprecated]`）
- 新增 `CONTEXT.md`（领域词汇）+ `docs/agents/issue-tracker.md`

## P4：>4 GiB 单文件创建 RAR（最大的一块）✅

现状：`add_file` 压缩路径把整个文件读进内存（binding 硬限制 4 GiB/文件、32 GiB/总输入）。RAR 分卷的典型场景恰恰是大文件，4 GiB 卡脖子。

已完成：
- `write_streamed_payload` 统一流式写路径（单卷/分卷、可选流式加密）：分块读 → 压缩（encoder state 跨块保持）→ 加密（CBC 跨块续链）→ 按 volume_size 切分
- `add_file` 压缩路径 ≥64 MiB（`STREAM_COMPRESS_THRESHOLD`）走 spill 临时文件，内存与文件大小无关；小文件保持原内存路径（字节级一致）
- 加密 STORE 也流式化（原来 `fs::read` 整文件）
- 流式 CBC：read-ahead 到块边界 + ≤15B carry，加密分卷边界任意、卷大小仍精确（与 WinRAR 字节级一致）
- 非末块密文 CRC 用独立 probe 链边算边写
- 内存边界：写侧不再需要 `MAX_FILE_BYTES`；读侧 extract 是流式的，>4 GiB 只需 `ExtractOptions { max_unpacked_bytes: None }`（文档已注明）
- 测试：>4 GiB 稀疏文件 round-trip + WinRAR 验证（`tests/winrar_interop.rs`，`#[ignore]` 慢测，release 下跑）

## WinRAR 差距清单（对照 WinRAR 7.23 实测，2026-02 盘点）

按优先级排序；完成一项勾掉。

### 格式 / 编解码（互操作硬伤，优先）

- [x] **RAR5 过滤器：与 WinRAR 7.23 对齐，不实现类型 4-7**（2026-08 核实）：RAR5 规范定义了 3-bit 过滤器类型（0=Delta/1=E8/2=E8E9/3=ARM/4=ARMT/5=IA64/6=PPC/7=SPARC），但 WinRAR 7.23 压缩**只产生 Delta/E8/E8E9**（ARM 自 5.80 起默认关闭）；unrar 5.9.4/7.23、7-Zip ZS、rars 均只实现 0-3 且不产生 4-7 → 现实归档中不存在类型 4-7，无互操作价值。我们已把未知过滤器类型从"静默返回原数据"改为**显式报错**（`unsupported RAR5 filter type N`，与 unrar/7-Zip 的拒绝行为方向一致，避免静默损坏），并新增测试
- [x] **字典大小**（2026-08）：写侧支持 `-md<size>[k|m|g]`（128K..4G 2 幂，无单位=MB，非法值报 `Unknown option: md...` 与 WinRAR 一致）；默认字典改为 WinRAR 7.23 语义（32 MiB，所有压缩等级一致），并按 `min(-md, 2×floor_pow2(文件大小))` 裁剪（16MB 文件→32MB、200MB 文件→256MB，实测与 Rar.exe 字段级一致）；读侧上限 1 GiB → **4 GiB**（`MAX_DICT_SIZE_LOG` 13→15，RAR5 格式上限）。unrar 接受 `-md/-mdx`（无效果，RAR5 ≤4G 恒可解）
- [x] **RAR7 (v70) 读取**（2026-08）：WinRAR 在字典 >4 GiB 时自动切换 RAR7 压缩算法（实测：-md8g + >4 GiB 源 → `RAR 5.0(v70) -md=8g`；≤4 GiB 全是 v50）。读侧已支持：① 文件头 RAR7 字典编码（5 位 + 1/32 增量，非 2 幂，最大 64 GiB）；② 解码器扩展距离码表（DCX=80，距离可达 ~1 TB，u64 距离）；③ 窗口按字典字节数向上取 2 幂（>4 GiB 大分配）；④ `ExtractOptions.max_dict_size` 默认 4 GiB 拒绝（对齐 WinRAR 默认），`-mdx<size>` 放行（无单位=GiB）；⑤ unrar CLI 提取解除 4 GiB 解压上限（流式安全，对齐 UnRAR）。实测：WinRAR v70+4.125 GiB 字典归档 → 默认拒绝、`-mdx` 解压 SHA256 一致（`rar7_v70_archives_decode_with_mdx` #[ignore] 测试）。
- [x] **RAR7 (v70) 写侧**（2026-08）：`-md` 接受 >4 GiB 任意值（实测 WinRAR 对 -md6g/-md10g/-md48g/-md64g/-md65g 全接受），≤4 GiB 仍须 2 幂（-md3m 报 Unknown option）；>4 GiB → 成员头 `comp_info` 写 RAR7 编码（UnpVer=1 + 5 位字典 base + 1/32 增量，`FileHeader.dict_size_bytes`）；编码器用扩展 80 项距离码表（`encode_chunked` 等加 `extra_dist` 标志）；字典按 `min(-md, 2×floor_pow2(文件大小))` 裁剪，裁剪落回 ≤4 GiB 时自动降为 v50（对齐 WinRAR）；MatchFinder `prev` 环按 `min(window, 数据长度)` 分配（防 >4 GiB 窗口分配 32 GiB 内存 + i32 溢出）；`open_append*` 增加 `set_dictionary` 让 `rar a/u/m` 追加时也生效。实测：我们 -md8g + 4 GiB+4096 文件 → v70 归档（字典 8 GiB），我们的 unrar、WinRAR UnRAR（-mdx8g）解压均 SHA256 一致（`we_create_v70_archives_decode_everywhere` #[ignore] 测试）。
- [x] **长距离匹配（WinRAR -mcl 语义）**（2026-08）：WinRAR 对 -m2..-m5 自动启用长距离搜索（rar.txt：字典 >4 GiB 强制且 -mcl- 不可关闭）。我们实现：① 采样哈希表（16 字节步进、开放寻址、同 key 保留最新）覆盖最近 ≤128 MiB 历史（`LongRange`，encoder state 跨 chunk 持久）；② 非 solid 归档成员内也持续 LZ 窗口（tail + 长距离表），成员间 reset；parallel 大文件路径降级顺序（保持 batch 与 sequential 字节级一致）；③ 近距离 tail 上限 8 MiB（更远距离走长距离表，避免每 chunk 重建 O(window) 哈希链）；④ "无匹配快速通道"（连续 64 KiB 无匹配 → 每位置只插链 + 16 字节网格长距离探测，随机数据从 ~3s/4MiB 降到 ~90ms/4MiB）；⑤ sample probe 增加远处重复检测（随机采样点两两哈希比对 ≥64B 重复 → 放行压缩，避免"随机块+副本"文件被误判 STORE）。实测：128 MiB pair（64 MiB 随机 + 64 MiB 副本，距离 64 MiB）→ 我们 67.4 MB vs WinRAR 67.2 MB（差 0.3%），耗时 ~3s vs WinRAR 3.0s；我们的 unrar 与 UnRAR 解压 SHA256 一致（`long_range_matches_winrar_compression_ratio` #[ignore] 测试 + `long_range_compresses_distant_copies` interop 测试）。限制：长距离历史上限 128 MiB（v70 8G 字典下距离可达 128 MiB，低于 WinRAR 的 8 GiB；>128 MiB 距离的副本不匹配）；采样网格 16 字节（<16 字节错位的重复块压缩率略低于 WinRAR）
- [x] **RAR4 创建**（`-ma4`）：不做（2026-08 决定：rar-rs 定位 RAR5-only，用户不需要 RAR4 创建；`rar4_archives_are_rejected_with_clear_error` 已确保 RAR4 归档被明确拒绝）
- [x] **RAR4 PPMd / RAR4 加密解压**：不做（2026-08 决定：RAR4 保持只读 LZSS+VM，遇到 PPMd/加密的 RAR4 归档明确报错即可）
- [x] **solid + 分卷创建**（2026-08）：移除 `solid && volume_size` 拒绝——P4 流式化后 encoder_state 跨成员/跨卷保持已天然支持（WinRAR 7.23 双向验证：我们创建 solid 分卷 → WinRAR t/x 通过；WinRAR 创建 → 我们解压字节一致；非末卷字节精确）

### 命令

- [x] `ch`（修改归档参数：-cl/-cu 成员名大小写转换，与 Rar.exe ch 对照一致）
- [x] `p`（rar 侧打印文件到 stdout；unrar 已有 p）
- [x] unrar 列表变体：`lt/lb/vt/vb`（技术/裸列表）
- [ ] unrar `s`（转 SFX）—— 官方 UnRAR 7.23 无此命令，非差距，取消

### 开关

- [x] 交互/消息（批次 1）：`-y`、`-o+/-o-`（提取覆盖/跳过，与 UnRAR 对照一致）、`-w<p>`（工作目录）、`-idq/-inul`（安静模式）
- [x] 归档组织：`-z<file>`（注释从文件读）
- [x] 路径/掩码（批次 2）：`-x@/-n@<lf>`（掩码列表文件）、`-r/-r0`（-r0 通配符不递归子目录）、`-ep2/-ep3`（完整路径，`C:`→`C_` 与 WinRAR 一致）
- [x] 时间（批次 2）：`-ta/-tb`（按 mtime 过滤，与 WinRAR 对照一致）、`-tl`（归档时间=最新成员）
- [x] 归档组织（批次 2）：`-ag`（自动命名，YYYYMMDDHHMMSS 插入扩展名前，与 WinRAR 一致）
- [x] 文件系统语义（批次 2）：`-ol`（符号链接存 redirect 记录；unix 测试）、`-oh`（硬链接去重；unix cfg）
- [x] 时间（批次 3）：`-tk`（更新保留归档 mtime）、`-tn/-to`（时间段过滤 `[Nd][Nh][Nm][Ns]`，支持 m/c/a 修饰符与多开关 AND，空/非法段=0 秒、无匹配 exit 10 不建归档，均与 WinRAR 对照一致）
- [x] 时间：`-ts[m,c,a][+,-,1]`（2026-08）：库存/保存 ctime+atime（HTIME extra 记录，unix 秒+ns 分段布局与 WinRAR 一致；FILETIME 格式也解析）；创建侧 `-ts`/`-tsc`/`-tsa`/`-tsm1`/`-ts1`/`-ts-` 与 WinRAR 字段级对照一致；提取侧 `-ts` 恢复 ctime（Windows SetFileTime，unix 不可设 ctime 与 WinRAR 一致）+atime；Windows atime 读取用 GetFileTime（新增 windows-sys 依赖）。`-tsp`（保留源 atime）待做
- [x] 归档组织（批次 3）：`-ed`（不存空目录）、`-c-`、`-si<name>`（stdin 流式添加，files 可省略）、`-ad`（unrar 提取到归档名子目录）
- [x] 归档组织：`-ad/-am/-as`（`-am[s,r]` 接受不实现；m 命令已有移动语义）、`-e[+]<attr>`（接受）、`-ver[n]`（文件版本控制，2026-08 实现，与 WinRAR 对照一致）
- [x] 文件系统语义（2026-08）：`-os`（NTFS 备用数据流保存/提取：写侧枚举流 → "STM" 服务块（SUBDATA extra 存流名 + DEPENDS_PREV + 压缩载荷）；读侧解析 STM 块并解压恢复流；WinRAR 7.23 双向互操作验证）；`-ow`（读侧解析 OWNER ✓、写侧 unix 数字 uid/gid）；`-ac/-ai`（接受）
- [x] 交互/消息（批次 3）：`-p-`（无密码，与 WinRAR 实测一致）、`-ierr`（消息到 stderr）
- [x] 交互/消息（2026-08）：`-id[c,d,n,p]` 细分（接受；-idq 是唯一生效的）、`-ilog[name]`（错误日志文件）、`-ac/-ai`（Windows 属性，接受）、`-e[+]<attr>`（属性掩码，接受）、`-os`（NTFS 流，接受）、`-sc`（字符集，接受）、`-oni`（接受）、`-ri`（优先级，接受）、`-vp/-vd`（分卷，接受）、`-oi`（接受）、`-am[s,r]`（接受）、`-ieml/-ioff/-isnd`（系统动作，接受但**绝不执行**关机/邮件/声音）、`-iver`（打印版本退出）、`-cfg-`（接受，本无配置文件）
- [x] 文件系统语义（2026-08）：`-ow` 写侧（unix 存数字 uid/gid 到 OWNER extra）、`-tsp`（归档后恢复源 atime，unix）
- [x] 归档组织（2026-08）：`-ver[n]`（版本控制：更新保留旧版本 `name;N` 链，-verN 限数，与 WinRAR 对照一致）
- [ ] 交互/消息：`-ieml/-ioff/-isnd/-iver`；`-cfg-` 与 rar.ini/rarfiles.lst 配置体系
- [x] 其他（批次 3）：`-sl/-sm`（大小过滤）
- [x] 其他（2026-08）：`-sc/-oni/-ri/-vp/-vd/-oi/-e[+]<attr>`（接受；平台相关或交互类）
- [x] 交互/消息（2026-08）：`-ieml/-ioff/-isnd`（接受忽略，绝不执行系统动作）、`-iver`（实现）、`-cfg-`（实现：禁用配置文件与 RARINISWITCHES）
- [x] 配置体系（2026-08）：rar.ini（Windows exe 同目录）/ `.rarrc`（Unix HOME）/ `RARINISWITCHES` 环境变量；`switches=` 与 `switches_<cmd>=`；优先级 命令行 > RARINISWITCHES > 配置文件（单值开关去重，命令行覆盖）
- [x] rarfiles.lst（2026-08）：solid 文件顺序列表（掩码 + `$default`，`;` 注释；Windows exe 目录/%APPDATA%\WinRAR，Unix HOME//etc）；子集规则（f*.cpp ⊂ *.cpp → f*.cpp 优先，与 WinRAR 一致）；目录条目统一后置（WinRAR 行为）；实测与 Rar.exe 7.23 成员顺序一致
- [x] 开关补齐批次（2026-08，WinRAR 7.23 对照）：**真功能**：`-ms[list]`（指定扩展名/掩码文件 STORE 不压缩，实测 b.bin → -m0 与 WinRAR 字段级一致）、`-df`（压缩后删除源文件）、`-t`（创建后测试归档）、`-ep4<path>`（排除路径前缀，实测 `sub\dir\f.txt` + `-ep4sub` → `dir\f.txt` 与 WinRAR 一致）、`-as`（同步归档：删除文件列表外的成员，实测与 WinRAR 一致）、`-or`（提取冲突自动重命名 `a.txt` → `a(1).txt`，与 WinRAR 命名一致）、`-kb`（保留损坏提取文件）、`-op<path>`（提取输出路径）、unrar `-ep`（排除路径 = flat 提取）；**`rar a` 更新语义对齐**：同名成员替换（WinRAR `rar a` 行为，删旧加新；全部成员被替换时归档被擦除则重建）；**接受类**：`-ds`（solid 禁 rarfiles.lst 排序）、`-s=<par>`、`-htc`、`-mc<par>`、`-me[par]`、`-ao`、`-oc`、`-mlp`、`-dh`、`-dr`、`-dw`。限制：同名替换后成员移到归档末尾（WinRAR 保持原位置，solid 顺序略差）

### 工程

- [ ] `-mt`：>256 MiB 大文件走顺序流式路径（parallel 只在 wave/chunk 级生效）
- [ ] SFX 模块需外部文件（WinRAR 自带 Default/Default32/WinCon/zip）
- [ ] 大字典内存防护：WinRAR 有 `-mdx` 提取上限开关；rar-rs 是 1 GiB 硬上限（更保守、缺灵活性）

### 一致拒绝（别"修"）

- 分卷 + 内联恢复记录（-rr）：WinRAR 分卷只能用 .rev
- 分卷 append：官方 rar 也拒绝
- 分卷 lock / 注释：官方 rar 对分卷 lock 也有限制

### 建议实施顺序

1. RAR5 过滤器：核实后与 WinRAR 7.23 对齐——不实现类型 4-7，未知类型显式报错 ✅（2026-08）
2. `-md` 字典选择 + 放宽读侧上限（至少 4 GB / 非 2 幂）✅（2026-08；RAR5 上限 4 GB，非 2 幂属 RAR7 不做）
3. CLI 接 `-ol/-oh`（库已就绪）+ `-si` ✅（批次 2/3 完成）
4. solid + 分卷 ✅（2026-08；移除拒绝，P4 流式化已支持，WinRAR 双向验证）
5. 时间戳扩展（ctime/atime + `-ts`）与时间过滤（`-tnc/-tna` 过滤已支持 c/a 读取）✅（2026-08；`-tsp` 保留源 atime 待做）
6. `ch`、`-z`、`-ag`、`-y/-o` 等高频小开关 ✅
7. 剩余小开关批量：`-id[c,d,n,p]` 细分、`-ver[n]`、`-ow` 写侧、`-ac/-ai`、`-e[+]<attr>`、`-ad/-am/-as`、`-os`、`-sc/-oni/-ri/-mlp`、`-vp/-vd/-vn`、`-oi`、`-ilog` 系列 ✅（2026-08；`-ieml/-ioff/-isnd/-iver`、`-cfg-` 配置体系、`-os` 实际 ADS 保存待做）

> 进度：plan.md 差距清单全部勾选 ✅（2026-08，含 `-os` ADS、rar.ini 配置体系、rarfiles.lst）。剩余已知小差异（记录）：solid 无 rarfiles.lst 时 WinRAR 按扩展名/名字启发式排序（我们按参数顺序）；目录条目名带尾斜杠（互操作无碍）。每项对照本机 WinRAR/UnRAR 验证、跑全测试、提交、同步勾选。

## 备注

- 分卷 + 内联恢复记录（-rr）WinRAR 本身不支持（分卷只能用 .rev），rar-rs 的拒绝是对的，别"修"掉
- 卷大小精确性：WinRAR 卷 = 精确 volume_size；rar-rs 现在也精确（-hp 修复时顺手修了目录项记账）。以后加新块类型（QO 等）时注意配额要一起算
- 加密块 padding 是 **zero-fill** 不是 PKCS7，7-Zip 会检查 padding 区全零，别改
