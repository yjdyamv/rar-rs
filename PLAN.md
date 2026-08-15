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

- [ ] **RAR5 过滤器缺 4 种**：只实现 Delta/E8/E8E9/ARM（`src/codec/filters.rs`；`parse_filter` 只读 3-bit 类型 0-7）。缺 ARMT(4)/IA64(5)/PPC(6)/SPARC(7)。影响：WinRAR 用这 4 种过滤器压的归档（IA64/PPC/SPARC 代码、ARM64 固件等）解压数据错误 → CRC 校验失败（不静默损坏，但读不了）。unrar 开源实现可参考
- [ ] **字典大小**：写侧无 `-md`，窗口按等级固定（最大 8 MiB）；读侧 `MAX_DICT_SIZE_LOG=13`（1 GiB）硬上限。WinRAR 7.x 支持到 64 GB、>4 GB 可非 2 幂（RAR7 大字典归档目前直接拒读）
- [ ] **RAR4 创建**（`-ma4`）：现只读不建
- [ ] **RAR4 PPMd / RAR4 加密解压**：不支持（RAR4 只读 LZSS+VM）
- [ ] **solid + 分卷创建**：现拒绝（`opts.solid && volume_size`）。WinRAR 支持；P4 流式化后 encoder_state 跨卷保持是最后一步

### 命令

- [ ] `ch`（修改归档参数，如 -cl/-cu/-tl 组合）
- [ ] `p`（rar 侧打印文件到 stdout；unrar 已有 p）
- [ ] unrar 列表变体：`lt/lta/lb/vt/vta/vb`
- [ ] unrar `s`（转 SFX）

### 开关

- [ ] 路径/掩码：`-ep2/-ep3/-ep4`；`-x@lf/-n@lf`（掩码列表文件，`-x/-n` 本身已有）；`-r/-r0`（rar-rs 目录默认递归，只有 `-r-`）
- [ ] 时间：`-ta/-tb/-tn/-to`（时间过滤）、`-tk/-tl`、`-ts[m,c,a]`（现在只存 mtime 秒+ns，不存 ctime/atime）
- [ ] 归档组织：`-ag`（日期自动命名）、`-ad/-am/-as`（同步）、`-ed`（不存空目录）、`-e[+]<attr>`；`-z<file>`（注释从文件读，现 `c` 只走 stdin）、`-c-`；`-ver[n]`（文件版本控制）；`-si`（stdin 流式添加——CLI 没有，README 曾宣称 done 需纠正）
- [ ] 文件系统语义：`-ol/-oh`（库 API `add_redirect` 已支持类型 1/2/4/5，CLI 未接线）；`-os`（NTFS 流）；`-ow`（读侧解析 OWNER 记录、写侧不生成）；`-ac/-ad/-ai`
- [ ] 交互/消息：`-y`、`-o[+|-]`（提取覆盖）、`-p-`、`-w<p>`；`-id*/-inul/-ierr/-ilog/-ieml/-ioff/-isnd/-iver`；`-cfg-` 与 rar.ini/rarfiles.lst 配置体系
- [ ] 其他：`-sl/-sm`（大小过滤）、`-sc/-oni/-ri/-mlp`、`-vp/-vd/-vn`（分卷）、`-oi`（流选项）

### 工程

- [ ] `-mt`：>256 MiB 大文件走顺序流式路径（parallel 只在 wave/chunk 级生效）
- [ ] SFX 模块需外部文件（WinRAR 自带 Default/Default32/WinCon/zip）
- [ ] 大字典内存防护：WinRAR 有 `-mdx` 提取上限开关；rar-rs 是 1 GiB 硬上限（更保守、缺灵活性）

### 一致拒绝（别"修"）

- 分卷 + 内联恢复记录（-rr）：WinRAR 分卷只能用 .rev
- 分卷 append：官方 rar 也拒绝
- 分卷 lock / 注释：官方 rar 对分卷 lock 也有限制

### 建议实施顺序

1. RAR5 过滤器补齐（ARMT/IA64/PPC/SPARC）
2. `-md` 字典选择 + 放宽读侧上限（至少 4 GB / 非 2 幂）
3. CLI 接 `-ol/-oh`（库已就绪）+ `-si`
4. solid + 分卷
5. 时间戳扩展（ctime/atime + `-ts`）与时间过滤
6. `ch`、`-z`、`-ag`、`-y/-o` 等高频小开关

## 备注

- 分卷 + 内联恢复记录（-rr）WinRAR 本身不支持（分卷只能用 .rev），rar-rs 的拒绝是对的，别"修"掉
- 卷大小精确性：WinRAR 卷 = 精确 volume_size；rar-rs 现在也精确（-hp 修复时顺手修了目录项记账）。以后加新块类型（QO 等）时注意配额要一起算
- 加密块 padding 是 **zero-fill** 不是 PKCS7，7-Zip 会检查 padding 区全零，别改
