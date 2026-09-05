# CODE_REVIEW_2026-09-05 — origin/main..HEAD 架构审查

- 范围：`origin/main..HEAD`（21 commits，117 files，+3790/−2616）：API v2 门面解体 + 物理目录收敛（Phase 5）+ 版本表统一（ADR 0004）+ crate 更名 `rar-rs`。
- 审查焦点（用户指定）：**代码架构 / 文件名 / 文件夹架构 / API 名**。
- 方法：`code-review` skill 双轴并行子代理——**Standards**（对照仓库文档化标准 + Fowler 气味基线）与 **Spec**（对照 `plan-architecture-api-v2.md` + ADR 0002/0003/0004），两轴各自独立报告、不合并排序。
- 状态：**已记录，未修复**（2026-09-05）。
- 审查时点验证：fmt / clippy `-D warnings` / check（workspace + fuzz + no-default-features）/ test 全绿；`cargo doc` 无新增链接 warning。

## Standards 轴

文件级核对通过项（与文档化标准吻合）：

- Phase 5 目标目录树逐字吻合：`archive/{reader,writer,editor,transaction,discovery}`（`rewrite.rs`→`transaction.rs`、`discover.rs`→`discovery.rs`）、`codec/{common,legacy,modern}`、`format/rar4`+`format/rar5`、`fs/{atomic,safe_path,volume}`、CLI `src/bin/{rar,unrar}`、napi `lib/options/tasks/error`。
- `rar40`/`rar50`/`lzss_huff` re-export shim 尊重变更规则 4（re-export shim 优先于同步副本）；两次目录搬迁都记录在 plan 的收敛笔记里。
- `ArchiveVersion` V15–V70 两位数命名与 CONTEXT/ADR 0004 一致；fs/model 保持叶层（`architecture_boundaries.rs` 门禁通过）。
- **无硬性违规**。

判断性发现（气味级，非违规；文档化标准未覆盖处按基线判定）：

1. **版本轴命名分裂**（naming coherence）——`archive/writer.rs` 字段 `format_version`→`compression`，但 `options.rs:45` 的 `CreateOptions::format_version` 仍是同一 `ArchiveVersion` 类型。ADR 0004 收敛为单一表，两个公共 API 却叫法不同。已被 ADR 0004 记录（plan:376-378），但概念应只有一个名字；见下修复事项 FX-01。
2. **napi 陈旧 crate 名文档**（Mysterious/stale name）——`crates/rar-napi/src/{error.rs:1, lib.rs:3, options.rs:2, tasks.rs:20}` 的 docstring 仍写 "rar5 `RarError`/crate/library"，crate 更名未清扫。见 FX-02。
3. **误导性 import 别名**（Mysterious Name）——`format/rar5/extract.rs:25`：`use crate::format::rar5::write as archive_write;`，别名未说清是 RAR5 写模块，`rar5_write` 更诚实。见 FX-03。
4. **重复校验串**（Duplicated Code）——`"only versions v29, v50 and v70 are writable, got {}"` 在 `options.rs`（`CreateOptions::validate`）与 `archive/writer.rs`（`WriterOptions::validate`）各一份。漂移风险低，但可抽共享 helper。见 FX-04。
5. **V36 vs V29 双变体**——版本表含两个同 codec 变体；保留 wire `unp_ver` 映射、CONTEXT 明示有意，**不视为问题**，无动作。

## Spec 轴

确证无误（与 plan/ADR 逐项核对）：

- crate 更名 `rar-rs`/`rar_rs`/`rar-rs-fuzz` 全仓一致（package keys、workspace dep、examples、fuzz、napi、CI.yml）。
- 物理树与 Phase 5 目标布局吻合；`ArchiveVersion` 表、`CreateOptions::format_version`、`WriterOptions::compression`、`ArchiveEntry::version()`、CLI `-ma` 映射均合 ADR 0004。
- deprecated facade 集合与此计划 20 个名字完全一致（create_with_options/close/add/add_as/add_directory_only/add_bytes/add_redirect/add_batch/add_recovery_record/set_comment/delete/delete_with_progress/rename/lock/list/get_entry/namelist/read/extract_all/extract）。
- **无实质越界**：`ArchiveFormat`/`CompressionVersion` 移除系 ADR 0004 要求（supersede b8cf7c3）。

(a) 缺失 / 不完整：

1. **`model/` 目标树未完成**——计划 Phase 5 目标布局列出 `model/{entry.rs, chunk.rs, path.rs, timestamp.rs, compression.rs, redirect.rs}`，实际仅 `entry.rs` + `chunk.rs`（+`mod.rs`），且 21 commits 未触碰 `model/`。既有缺口非本轮引入，但 "Phase 5 convergence complete"（`5d7e618`）与计划"精确目标树"不符。见 SP-01。
2. **CLI 单一 open 路径的说法过强**——计划声称 "`open_reader` is now the CLI's single open path"，但 `cmd_comment_write`（`rar cw`）仍走 `RarArchive::open_with_password`/`get_comment()`（`bin/rar.rs:2228-2231`），`cmd_repair` 用 `RarArchive::open`（:2175）。见 SP-02。

(b) 越界：无实质越界。

(c) 实现与规格偏差：

1. **`format::rar4` 实非 internal**——ADR 0003 决策 3 称 "`format::rar4` is internal"，但 `lib.rs:40` 保留 `#[doc(hidden)] pub mod format;` 且 `format/mod.rs:5` 为 `pub mod rar4;`，不启用 `raw` 也可触达 `rar_rs::format::rar4`（仅 `rar40` 别名被门控）。与 plan 字面一致，但 ADR 措辞不实（项为 `pub(crate)`，功能无实质泄漏）。见 SP-03。
2. 次要：上述两处 `RarArchive` 调用残留于 "CLI 全部跑在 typed role 上" 的论断之外。

## 可执行修复事项（待办）

| # | 位置 | 建议修复 | 轴 | 优先级 |
|---|---|---|---|---|
| FX-01 | `archive/writer.rs` `compression` vs `options.rs:45` `format_version` | 统一版本轴字段命名（二选一）；确定后同步 ADR 0004 措辞 | Standards | 中 |
| FX-02 | `crates/rar-napi/src/{error,lib,options,tasks}.rs` docstring | "rar5 …" → "rar-rs …"，清扫更名残留 | Standards | 低 |
| FX-03 | `format/rar5/extract.rs:25` | `archive_write` 别名 → `rar5_write` | Standards | 低 |
| FX-04 | `options.rs` + `archive/writer.rs` | 抽取可写校验 `ArchiveVersion` 共享报错 helper（消除重复串） | Standards | 低 |
| SP-01 | `model/` 目录 | 定夺：补建 `model/{path,timestamp,compression,redirect}.rs`，或把 plan 目标树改为"以实际为准" | Spec | 中 |
| SP-02 | CLI `cmd_comment_write`/`cmd_repair`（`bin/rar.rs:2228` / `:2175`） | 迁到 typed role 读取，或改 plan "single open path" 论断 | Spec | 中 |
| SP-03 | ADR 0003 决策 3 + `format` 树可见性 | 修 ADR 措辞，或把 `#[doc(hidden)] pub mod format` 收敛回 `pub(crate)` | Spec | 低 |

## 结论

- **Standards 轴**：0 硬性违规 + 5 判断性发现；最需处理 `FX-01`（版本轴双名）。
- **Spec 轴**：2 缺失 + 0 越界 + 3 偏差；最需处理 `model/` 目标树缺口（SP-01）与 "CLI 单一 open 路径" 论断失实（SP-02）。