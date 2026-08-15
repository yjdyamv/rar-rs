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
- [x] **字典大小**（2026-08）：写侧支持 `-md<size>[k|m|g]`（128K..4G 2 幂，无单位=MB，非法值报 `Unknown option: md...` 与 WinRAR 一致）；默认字典改为 WinRAR 7.23 语义（32 MiB，所有压缩等级一致），并按 `min(-md, 2×floor_pow2(文件大小))` 裁剪（16MB 文件→32MB、200MB 文件→256MB，实测与 Rar.exe 字段级一致）；读侧上限 1 GiB → **4 GiB**（`MAX_DICT_SIZE_LOG` 13→15，RAR5 格式上限，与 WinRAR 7.23 接受范围一致；>4G/非 2 幂只存在于 RAR7，不支持）。unrar 接受 `-md/-mdx`（无效果，RAR5 ≤4G 恒可解）
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
- [ ] 归档组织：`-ad/-am/-as`（a 命令同步侧）、`-e[+]<attr>`；`-ver[n]`（文件版本控制）
- [ ] 文件系统语义：`-os`（NTFS 流）；`-ow`（读侧解析 OWNER 记录、写侧不生成）；`-ac/-ai`
- [x] 交互/消息（批次 3）：`-p-`（无密码，与 WinRAR 实测一致）、`-ierr`（消息到 stderr）
- [ ] 交互/消息：`-id[c,d,n,p]` 细分、`-ilog/-ieml/-ioff/-isnd/-iver`；`-cfg-` 与 rar.ini/rarfiles.lst 配置体系
- [x] 其他（批次 3）：`-sl/-sm`（大小过滤）
- [ ] 其他：`-sc/-oni/-ri/-mlp`、`-vp/-vd/-vn`（分卷）、`-oi`（流选项）

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
7. 剩余小开关批量：`-id[c,d,n,p]` 细分、`-ver[n]`、`-ow` 写侧、`-ac/-ai`、`-e[+]<attr>`、`-ad/-am/-as`、`-os`、`-sc/-oni/-ri/-mlp`、`-vp/-vd/-vn`、`-oi`、`-ilog` 系列

> 进度：下一步 = 第 7 项（剩余小开关批量：`-id[c,d,n,p]` 细分、`-ver[n]`、`-ow` 写侧、`-ac/-ai`、`-e[+]<attr>`、`-ad/-am/-as`、`-os`、`-sc/-oni/-ri/-mlp`、`-vp/-vd/-vn`、`-oi`、`-ilog` 系列；`-tsp` 顺带）。每项对照本机 WinRAR/UnRAR 验证、跑全测试、提交、同步勾选。

## 备注

- 分卷 + 内联恢复记录（-rr）WinRAR 本身不支持（分卷只能用 .rev），rar-rs 的拒绝是对的，别"修"掉
- 卷大小精确性：WinRAR 卷 = 精确 volume_size；rar-rs 现在也精确（-hp 修复时顺手修了目录项记账）。以后加新块类型（QO 等）时注意配额要一起算
- 加密块 padding 是 **zero-fill** 不是 PKCS7，7-Zip 会检查 padding 区全零，别改
