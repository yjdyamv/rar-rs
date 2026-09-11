# ADR 0005: RAR4 Edit Architecture

- Status: accepted (phases A–C landed 2026-09)
- Date: 2026-09-08 (original), 2026-09 (phases A–C landed)
- Related: [ADR 0001](0001-rar4-creation-architecture.md) (creation side), [ADR 0004](0004-single-archive-version-table.md) (version table)

rar-rs 将补上「已存在 RAR4 归档的编辑」能力（官方 `rar`/WinRAR 的 `u/f/d/rn/ch/k/c/rr` 作用于 RAR4）。核心决策：

1. **对齐官方能力，进承诺面**：官方对 RAR4 的全部编辑命令都支持（不报错，仅代价是慢），我们同样全做。目标工具是 WinRAR 6.23（最后一代可产 RAR4 的官方版本）与 rar 5.30 手册。

2. **机制分层**：
   - **头/块级操作**（`rr`/`k`/`c`/`cw`/`rn`/`ch`）：结构补丁，不触碰任何成员压缩数据，solid 档案同样可做——名字/属性/注释/恢复记录不在压缩窗口语义内。
   - **非 solid 成员操作**（`d`/`u`/`f`/`a`）：块拷贝 + 现有 RAR4 写侧重发新成员 + 新 ENDARC，纯搬运，不重压。
   - **solid 成员操作**（`d`/`u`/`f`/`a`）：**整档 repack（解码→重编）**，与官方一致。

3. **solid 编辑机制 = 整档 repack，不做 surgical 部分重处理**。官方依据（rarlab What's New）：WinRAR 7.20 曾对 solid 删除做「无关数据原样拷贝、只重处理变更部分」的优化，在 **RAR4 格式上产出坏档**；7.21 起回退为「deleting files in solid RAR4 archives now involves the full archive repacking, similar to WinRAR versions preceding 7.20. It doesn't affect archives in the modern RAR5 format」——surgical 仅存于 RAR5。我们照 7.21+ 行为实现，不仿 7.20。

4. **v1 边界（清晰报错拒绝，不静默）**：
   - 分卷归档编辑：拒绝（需卷重平衡，后续）。
   - ~~`-hp` 头加密归档编辑：拒绝~~ **已解除（2026-09）**：主头是明文标记（带 MHD_PASSWORD），布局扫描用归档口令逐块解密头、重写时对重建/插入的块用新盐重加密；未改动的块整段（含密文）原样拷贝。仅剩分卷与已锁定档拒绝。
   - 已锁定归档（主头 LOCK 位）：拒绝，与 RAR5 编辑一致（`RarError::ArchiveLocked`）。

5. **编辑保持 RAR4 格式输出**。官方依据（WinRAR 帮助：Archive name and parameters dialog）：update 现有归档时格式选项被忽略，沿用被更新归档的格式，不转换。

## 现状代码位置（2026-09 实施前快照；阶段 A–C 已落地，见下）

- `archive/editor.rs` `ensure_rewritable`（`archive.rar4` → `Unsupported`，拦 `d/rn/k/c/rr`）
- `archive/writer.rs` `append_with`（拦 `a/u/f`）
- `rar4` 读侧扫描跳过注释块、文件注释不暴露到公共模型（`format/rar4/mod.rs`）
- `rar r` 修复与 `legacy_rr` 恢复记录读写已存在（`recovery/legacy.rs`），RR 写侧在创建路径 `finish_writing_rar4` 内，需抽为可对已存在归档调用的形式

## 实施阶段（分段提交）

| 阶段 | 内容 | 验证锁定 |
|---|---|---|
| **A 头级操作** | `rr` 原地补/换（主头 MHD_RECOVERY + 尾 ENDARC 前插/换 NEWSUB 0x7a，复用 `legacy_rr`；换 % = 剥旧插新）；`k` 锁（主头 LOCK 位 + CRC16）；`rn`/`ch`（重建 FILE_HEAD，数据字节不动）；`c`/`cw` 归档注释读+写（读侧现跳过，先补解析与公共模型暴露）；公共脚手架：staged 临时文件原子替换、各拒绝的清晰报错 | WinRAR 6.23 双向（`Rar.exe t` / `UnRAR.exe t`）+ 自读回 + `repair` 往返；注释展示与读写各需 6.23 夹具 |
| **B 非 solid 成员操作** | `d`/`u`/`f`/`a`：块拷贝、截 ENDARC 重发新成员（复用 `emit_rar4_prepared`/`add_file_rar4` 写侧）、新 ENDARC；原带 RR 的档按官方行为处理 | 6.23 `t` + 与官方同操作结果比对（成员集合/解出字节一致） |
| **C solid repack** | solid 档 `d/u/f/a`：全档解码→重编；跨 session 链状态由全链解码重建（读侧持久 solid 状态机制已存在）；重编参数按原成员 method 与文本探测回退默认 | self roundtrip（repack 前/后解出字节一致）+ 6.23 `t` |

~~`-hp` 与分卷编辑为后续阶段，届时分别补「按口令重加密」与「卷重平衡」。~~

**`-hp` 头加密编辑已落地（2026-09）**：`format/rar4/mod.rs::decrypt_encrypted_header` 提供内存态头解密；`archive/rar4_edit.rs` 的 `scan_layout`/`read_comment`/`edit_rar4`/`append_prelude` 全部按口令工作，`recovery/legacy.rs::scan_protect_with_password` 让恢复记录在 `-hp` 档上可定位/重建（记录自身的标签表与奇偶区不加密，仍可修复）。solid repack 与 create 路径用同一口令重建保护（`-hp` 同时含数据加密，与官方一致）。写侧 `emit_pending_rar4_comment` 让注释块在 `-hp` 档里也只加密 35 字节头。仅「卷重平衡」仍待办。

## Considered Options

- **接受「RAR4 = 只读 + 创建」边界**：被拒。项目定位为对齐官方 `rar` 命令面，官方对 RAR4 编辑全部支持（仅 solid 更新慢），边界会留下真实命令缺口。
- **solid 编辑只做受影响段的 surgical 部分重处理**：被拒。官方 7.20 已试并在 RAR4 上产坏档、7.21 回退为整档 repack；不仿已证伪的方案。
- **复用 RAR5 编辑引擎（`archive/transaction.rs`）**：被拒。其词汇（RAR5 block/CRC32/BLAKE2/service block、surgical 重写）与 RAR4 固定宽度 CRC16 头完全不同；RAR4 需要自己的 transaction 编排层，仅 CLI 契约（`ArchiveEditor`/append 角色）与错误语义对齐。

## Consequences

- `archive/editor.rs` `ensure_rewritable` 的 RAR4 拒绝改为按阶段路由到 RAR4 编辑路径；新增 RAR4 版 transaction/重写层（镜像 RAR5 的 plan/execute 结构，词汇是 `Rar4Block`）。
- `rar4` 读侧扫描需输出可寻址的块流（offset/长度/类型），供结构层定位成员与 ENDARC。
- 注释：读侧解析归档注释块与文件注释（FHD_COMMENT），暴露进公共模型（`get_comment`/列表展示）；写侧随阶段 A。
- CLI 行为不变，报错面收窄（RAR4 不再整类 Unsupported，仅分卷/锁定拒绝；`-hp` 需口令，缺口令报 `Encrypted`）。
- 互操作验证基准为 WinRAR 6.23 与 rar 5.30 手册；solid repack 的对照行为以 7.21+ 变更日志为准。
