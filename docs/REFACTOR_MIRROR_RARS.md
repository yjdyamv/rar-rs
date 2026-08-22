# 迁移计划：仿 rars 架构重构 rar-rs

> 蓝图来源：`C:\Users\yuan\Desktop\rars`（参考项目，Apache-2.0/MIT，bitplane/rars）。
> 目标：把 `rar-rs` 从单 crate + 巨型 `archive.rs` 重构为 rars 式 workspace + 深度分层。
> 原则：**行为零变更**——Phase 1–2 只搬代码不改逻辑，靠 interop 字节级断言 + WinRAR 互操作测试兜底。

## 决策记录（2026-08-22 确认）

| 项 | 决策 |
|---|---|
| 库 crate 名 | `rar`（不用 rar5，为将来低版本支持留空间） |
| 工作区 | 根 workspace：`crates/rar`（库）+ `crates/rar-cli`（rar + unrar 两二进制） |
| 公共 API | 保持 `RarArchive` 组合式 API 不变（CLI/测试零语义改动），内部按 rars 分层 |
| 文件布局 | 严格镜像 rars（`rar50/`、`crypto/`、`codec/rar50.rs`、`detect.rs`、`parallel.rs`…） |
| 范围 | 代码 + 测试重组 + 文档修正（不做 golden bless 工具链、python/wasm 绑定、scripts/justfile） |
| 偏差记录 | 见"风险与偏差"§ |

## 目标结构

```
rar-rs/
├── Cargo.toml                 # workspace 根（workspace.package / workspace.dependencies / lints）
├── crates/
│   ├── rar/                   # 库 crate（原 src/，RAR5-only，公共 API 不变）
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs         # facade：RarArchive 结构体 + ArchiveEntry/BatchEntry + 全部 pub use
│   │   │   ├── error.rs       # 不变（RarError/RarResult）
│   │   │   ├── options.rs     # 不变（CreateOptions/ExtractOptions）
│   │   │   ├── detect.rs      # 新：sfx_offset_of / verify_signature / SFX_SCAN_LIMIT ← archive.rs 1970–2013, 3677–3692
│   │   │   ├── parallel.rs    # 新：压缩/解压线程池 ← archive.rs 74–196
│   │   │   ├── io_util.rs     # 新：read_up_to / 原子文件替换 / temp 辅助 ← archive.rs 3978–4109
│   │   │   ├── version.rs     # 新（薄）：ArchiveVersion { Rar50, Rar70 }
│   │   │   ├── features.rs    # 新（薄）：FeatureSet { solid, header_encryption, quick_open }
│   │   │   ├── write_plan.rs  # 新：能力校验表 + StoreFallback ← 散落 refuse 检查 + sample_is_incompressible 逻辑
│   │   │   ├── streaming.rs   # 新：EntrySource + WriterResources + Spool ← SpillGuard/spill_path_for/内存预算
│   │   │   ├── source.rs      # 新（薄）：ArchiveStream 读侧 trait + 流包装
│   │   │   ├── write_progress.rs  # 不变（私有）
│   │   │   ├── rewrite.rs     # 吸收 RewriteOp/RewritePlan/VolumeReaders/CopyPipeline（内部自治化）
│   │   │   ├── rar50/         # RAR5 格式层（镜像 rars rar50/）
│   │   │   │   ├── mod.rs     # 容器层：RAR5 常量 + FileHeader/ArchiveHeader/DataChunk/RawBlock + 头解析 ← constants.rs + headers.rs 读侧
│   │   │   │   ├── vint.rs    # ← src/vint.rs
│   │   │   │   ├── blake2sp.rs# ← src/blake2sp.rs
│   │   │   │   ├── extract.rs # 读路径：open/scan_blocks/list/read/extract*/solid 链/verify_integrity ← archive.rs 读侧（2015–2130, 2410–3378, 3623–3676）+ 卷发现（4114–4203）
│   │   │   │   └── write/
│   │   │   │       ├── mod.rs     # 写门面：create*/close/finish/add*/add_batch*/write_file_entry/write_stored_file
│   │   │   │       ├── engine.rs  # 组装：write_streamed_payload + PayloadStream + CbcRangeEmitter + 定位器回填 + QO/RR 记录 + commit_pending
│   │   │   │       ├── headers.rs # 纯序列化：to_bytes/extra 记录构建 ← headers.rs 写侧 + write.rs time/owner 配置
│   │   │   │       ├── compress.rs# 批压缩：add_batch_parallel/prepare_* ← write.rs 824–1299
│   │   │   │       ├── layout.rs  # 字典选择 + 分卷切分 + 溢出策略 ← dict_params_for / volume 边界
│   │   │   │       └── win.rs     # cfg(windows)：ADS 流 + SetFileTime ← write.rs 10–159, 2215+
│   │   │   ├── codec/          # 编解码层（镜像 rars：一族一文件）
│   │   │   │   ├── mod.rs      # 公共再导出
│   │   │   │   ├── rar50.rs    # 合并 decoder.rs + encoder.rs + compression.rs + tables（~2500 行，rars 同风格）
│   │   │   │   ├── bitstream.rs# 保持
│   │   │   │   ├── huffman.rs  # 保持
│   │   │   │   ├── filters.rs  # 保持
│   │   │   │   ├── window.rs   # 保持
│   │   │   │   └── match_finder.rs  # lz_match.rs 更名
│   │   │   ├── crypto/         # 加密层（镜像 rars crypto/）
│   │   │   │   ├── mod.rs      # 门面
│   │   │   │   └── rar50.rs    # ← encryption.rs（AES-256-CBC/PBKDF2/hash-key MAC/EncryptionParams）
│   │   │   └── recovery/       # 不变（mod.rs, rar5.rs, rev5.rs）
│   │   ├── examples/bench.rs   # ← 根 examples/
│   │   └── tests/              # 测试重组后（Phase 4）
│   │       ├── rar50_roundtrip.rs
│   │       ├── format_assertions.rs
│   │       ├── rewrite.rs
│   │       ├── official_interop.rs   # 仅 SA_OFFICIAL_* 依赖
│   │       ├── rar4_rejection.rs
│   │       ├── robustness.rs         # 保持
│   │       ├── support/              # 共享 helper（rars #[path] 模式）
│   │       └── fixtures/rar50/       # fixtures 目录化 + 来源 README
│   └── rar-cli/                # 二进制 crate（原 src/bin/）
│       ├── Cargo.toml          # [[bin]] rar + [[bin]] unrar；dev-dep rar + tempfile
│       └── src/
│           ├── rar.rs          # bin rar：clap 定义 + 分发
│           ├── unrar.rs        # bin unrar
│           ├── common.rs       # WinRAR 兼容核心
│           ├── input.rs        # ← name_policy.rs + rarfiles.lst
│           ├── password.rs     # ← common.rs 的 PasswordArgs/配置系统
│           ├── output.rs       # ← common.rs 的 extract_dest/显示辅助
│           ├── progress.rs     # CLI 进度适配
│           ├── time.rs         # ← civil_from_days 等
│           ├── error.rs        # 退出码分类
│           └── volumes.rs      # 卷命名辅助
│       └── tests/              # CARGO_BIN_EXE 依赖者随二进制迁移
│           ├── cli_behavior.rs # ← interop.rs cli_* 段（含混合型 official 测试）
│           └── winrar_interop.rs
├── docs/                       # 保持（本文件所在）
├── .claude/                    # 保持
├── CONTEXT.md / PLAN.md / README.md  # Phase 5 修正
```

## 分阶段执行

每阶段结束：`cargo build --workspace --all-features` + `cargo test --workspace` 全绿。

### Phase 0 — 基线
- git 工作树干净；`cargo build --all-features && cargo test` 记录基线。

### Phase 1 — workspace 拆分（纯搬迁，零逻辑改动）
1. 根 `Cargo.toml` → workspace：
   - `[workspace] members = ["crates/rar", "crates/rar-cli"]`, resolver = "2"
   - `[workspace.package]` edition 2024、license BSD-2-Clause（保留本项目许可证，不从 rars）
   - `[workspace.dependencies]` 收编现有依赖：crc32fast/aes/hmac/sha2/rand/clap/zeroize + 可选 rayon/wide + windows-sys
   - `[workspace.lints.rust]` unused_must_use = "deny"；clippy dbg/todo/unimplemented = warn
   - **不启用 `unsafe_code = forbid`**（代码现有 ~15 处 unsafe，单独任务处理）
2. `src/` → `crates/rar/src/`；`src/bin/` → `crates/rar-cli/src/`；`examples/bench.rs` → `crates/rar/examples/`。
3. 改名 `rar5` → `rar`：`use rar5::` → `use rar::`（bin 89 处机械替换）+ 测试/示例/lib.rs 文档。
4. 迁移 `CARGO_BIN_EXE` 依赖测试：interop.rs 的 `cli_*` 段（~3168–4964）+ winrar_interop.rs（1033/1125 用 CARGO_BIN_EXE_rar）→ `crates/rar-cli/tests/`。interop.rs 其余部分仍单文件先迁到 `crates/rar/tests/`（Phase 4 再拆）。
5. 验证点：workspace build + test 全绿。

### Phase 2 — 库内按 rars 分层（大拆，逐文件搬移）
搬运顺序（先读后写、先容器后编解码），每搬一次 `cargo check -p rar`：
1. `rar50/mod.rs`：constants.rs 常量 + headers.rs 读侧（RawBlock/BlockMeta/read_block/parse_block_fields/from_raw/parse_extra_records/RedirectSpec 解析）+ 卷命名。
2. `rar50/extract.rs`：archive.rs 读路径（open/scan_blocks/scan_all_volumes/list/read/extract*/solid 链/decode/verify_integrity）+ IntegritySink + discover_volumes。
3. `rar50/write/`：archive.rs 写侧（create*/close/finish/写头/start_next_volume）+ write.rs 全部 impl：
   - `headers.rs`：headers.rs 写侧（to_bytes/build_*/extra 记录）+ write.rs time_extra_cfg/owner_extra_cfg
   - `compress.rs`：add_batch* 批压缩（BatchPrepareCtx/PreparedEntry）
   - `engine.rs`：write_streamed_payload/PayloadStream/CbcRangeEmitter/SpillGuard/patch_main_header_locator/QO/RR/commit_pending/PendingCommit
   - `layout.rs`：dict_log_for/dict_params_for/sample_is_incompressible*/分卷边界
   - `win.rs`：cfg(windows) ADS + SetFileTime + enumerate_windows_streams
   - `mod.rs`：其余写门面
4. `crypto/`：encryption.rs → crypto/mod.rs + crypto/rar50.rs。
5. `codec/`：decoder.rs + encoder.rs + compression.rs + tables → `codec/rar50.rs`；lz_match.rs → match_finder.rs。
6. 顶层新模块：detect.rs / parallel.rs / io_util.rs / version.rs / features.rs / write_plan.rs / streaming.rs / source.rs。
7. archive.rs 拆空 → lib.rs facade（RarArchive 结构体 + ArchiveEntry/BatchEntry + 再导出）；RewriteOp/RewritePlan/VolumeReaders/CopyPipeline 并入 rewrite.rs。
8. 内嵌 `#[test]` 随代码移动；可见性只调 `pub` ↔ `pub(crate)`，不改逻辑。
9. 验证点：`cargo test -p rar` 全绿（字节级断言兜底）。

### Phase 3 — CLI crate 重组
- common.rs 拆出：password.rs（PasswordArgs/配置系统）、output.rs（extract_dest/显示）、time.rs（civil_from_days）、volumes.rs、input.rs（name_policy + rarfiles.lst）。
- 验证点：`cargo test -p rar-cli` 全绿（cli_behavior + winrar_interop）。

### Phase 4 — 测试重组（interop.rs 4964 行按主题拆）
- `rar50_roundtrip.rs`：自举 roundtrip/batch/streaming/progress/长距/v70
- `format_assertions.rs`：字节级断言（locator/QO/RR/服务块）+ 共享 helper（read_vint/scan_blocks/service_name/main_header_locator/service_offset…）
- `rewrite.rs`：delete/append/rename/repair/comment/SFX
- `official_interop.rs`：仅 `SA_OFFICIAL_*` 交叉验证
- `rar4_rejection.rs`；`robustness.rs` 保持
- 混合型测试（如 `official_validates_cli_switch_archives`）按 CARGO_BIN_EXE 依赖归入 rar-cli 侧
- fixtures → `tests/fixtures/rar50/` + 来源 README；共享 helper → `tests/support/`（`#[path]` 模式）
- 验证点：`cargo test --workspace` 全绿。

### Phase 5 — 文档修正
- README.md：crate 名、模块布局段（含 RAR4 过时表述）、workspace 结构
- CONTEXT.md：热点行（archive.rs ~7500 已拆）、新增领域词（写管线 engine/layout/compress、codec 一族一文件、crypto/、并行层）、RAR4 表述修正
- PLAN.md：记录本次重构完成项

## 风险与偏差（有意为之，勿复制 rars 而未核）

1. **`unsafe_code = forbid` 不启用**：代码现有 ~15 处 unsafe（write.rs windows-sys 调用等），需单独清理任务。
2. **不建 `filter_search.rs`/`x86_filter_scan.rs`**：本项目过滤器只有显式 `FilterSpec`（codec/encoder.rs），无 rars 的自动过滤器搜索。
3. **不建 `crc32/` 模块**：沿用 crc32fast crate；rars 自研 Crc32 故有此模块。
4. **合并 `codec/rar50.rs`** 约 2500 行：rars 同风格（其 codec/rar50.rs 5496 行），为将来 codec/rar29.rs 铺路。
5. **保持 edition 2024**（rars 2021）、**BSD-2-Clause**（rars MIT/Apache）、**features `parallel`/`simd`**。
6. **CLI 测试迁移**：`CARGO_BIN_EXE_*` 只在同 package 集成测试生效 → cli_* 测试与 winrar_interop.rs 必须随二进制迁到 rar-cli crate。
7. **公共 API 不变**：不引入 rars 的 Builder/EntrySource/ArchiveReader 分离式 API（决策记录 §），`EntrySource` 仅作内部流式 seam 引入。
