# rar-rs 代码审查与整改基线（2026-09-05）

本文档记录 2026-09-05 对 `rar-rs` workspace 的代码、文件与文件名、公开 API、项目架构、CI、发布和许可证状态的审查结果，并作为后续整改的验收参照。

## 结论

项目核心编解码能力、互操作语料和回归测试资产较强，不建议重写。当前主要风险来自功能快速扩张后形成的边界漂移：CLI 有可复现的行为错误，格式解析和资源限制存在缺口，RAR4/RAR5 实现边界交叉，N-API/WASI 契约不一致，公开 API 含可触发 panic 的状态操作，文档与发布元数据落后于实际能力。

整改原则：

1. 先修正确性、安全和发布阻断问题。
2. 再稳定公开 API 和绑定契约。
3. 最后渐进拆分模块边界，禁止大爆炸重写。
4. 格式层改动必须有畸形输入测试、roundtrip 测试和既有 fixture 回归。
5. 破坏性重命名留到明确的主版本节点。

## P0：必须优先修复

### CLI 正确性与安全语义

- `rar update` 只新增成员时，因为以 `to_delete.is_empty()` 作为提前返回条件，命令返回成功但成员未写入。
- `rar` 与 `unrar` 的成员选择器包含 `requested.ends_with(member)`，例如请求 `data` 会误选成员 `a`。
- 裸 `-p` / `--password` 被解释为空密码并转成未加密归档，违背用户对交互密码提示的安全预期。
- `-mcl` 被更短的 `-mc` 前缀提前匹配；尺寸解析存在未检查乘法。
- 影响删除、擦除或数据完整性的未实现开关不应静默接受。

验收：增加纯新增 update、后缀碰撞选择器、裸密码、长前缀优先和尺寸溢出的回归测试。

### 格式解析与资源边界

- RAR5 明文头的 hsize vint 必须限制为最多 10 字节。
- 所有来自归档的长度必须先进行类型转换与 `checked_add`，不能直接参与偏移计算或切片。
- RAR4 STORE、压缩 packed payload、STM、CMT 等路径必须受统一 packed、metadata、单成员和总输出限制约束。
- 所有流式输出应通过统一的计数/限制 writer，声明大小和实际输出不一致时必须失败。

验收：畸形超长 vint、偏移溢出、超限 packed/metadata、STORE packed/unpacked 不一致测试。

### 文件提交与替换

- 临时文件必须随机且以 `create_new`/不跟随链接的方式创建。
- 不能在任意 rename 失败后直接删除原目标。
- 多卷提交应避免出现新旧卷混合；至少要保留可回滚的提交状态。

验收：目标已存在、目标占用/权限错误、临时名冲突和多卷中途失败测试。

### 许可证与 Release

- workspace 的纯 `BSD-2-Clause` 元数据与 `NOTICE` 中记录的 MIT/Apache/WTFPL 来源需完成逐文件核对；发布前应由维护者或法律顾问确认最终 SPDX 表达式。
- Release 必须携带 `LICENSE`、`NOTICE` 和第三方许可证清单。
- `SHA-256SUMS` 不得包含其自身的无效哈希。
- WASI 预期文件缺失不得被 `|| true` 静默吞掉。

## P1：稳定 API 与绑定

### Rust API

- `set_compression_threads`、`set_dictionary` 等状态相关 API 不得因读写模式错误而 panic，应返回结构化错误。
- `CreateOptions` 必须在单一入口完成格式、字典、线程、恢复和加密组合校验。
- `RarError` 应区分 `InvalidState`、`InvalidOption`、`Truncated` 等调用错误和格式错误。
- `codec`、`crypto`、`rar40`、`rar50` 等低层公开面应逐步收敛到 `raw`/`unstable` feature 或独立 crate；此项属于 SemVer 变更，不在修复提交中强行完成。

### N-API / WASI

- 所有 JS 数字必须在转换前验证符号、范围和 safe integer，禁止负数转 `u64` 和静默 clamp。
- JS 错误应有稳定 code，不能全部退化为 `GenericFailure` 文本。
- `readMember`、`testArchive` 和完整 listing 等重型调用应使用异步任务；如保留同步接口，应在名称中显式标注 `Sync`。
- Native 与 WASI 包装必须保留相同参数；所有返回路径必须从 guest path 映射回 host path。
- 分卷返回顺序必须按卷号而不是字符串字典序。
- 磁盘文件不应被绑定层无理由限制为 4 GiB；内存 bytes 输入和磁盘流式输入应采用不同限制。

## P1：架构整理方向

当前设计目标是 `archive facade -> format -> codec`，但 RAR4 仍依赖 `rar50::headers::{FileHeader, DataChunk}`，RAR4 提取/写入逻辑也混入 `rar50` 模块。核心热点文件超过 1,600–3,400 行，CLI 两个二进制还复制了成员选择、提取和列表逻辑。

推荐目标：

```text
archive/
  facade.rs
  transaction.rs
  extract.rs
  rewrite.rs
model/
  entry.rs
  chunk.rs
  timestamps.rs
format/
  rar4/{parse,read,write}.rs
  rar5/{parse,read,write}.rs
codec/
  legacy/
  rar5/
```

渐进步骤：

1. 先抽取中立的 entry/chunk 模型，RAR4 不再引用 RAR5 header 类型。
2. 再把 RAR4 提取和写入调度迁回 RAR4 模块。
3. `archive` 仅负责状态机、事务、格式分派和文件系统策略。
4. `rar-cli` 增加共享 library，抽取 selector/extract/list/open；两个 binary 只保留参数和 dispatch。
5. 不在安全修复中同时重命名 crate 或大规模移动 codec 热路径。

## P2：命名、文档和仓库组织

- crate `rar5` 已支持 RAR 1.5–RAR7。近期作为历史名称保留；下一个破坏性版本再评估格式中立名称。
- `ArchiveVersion` 混合容器家族和 codec 版本。~~未来可拆为 `ArchiveFormat` 与 `CompressionVersion`~~（2026-09 已按相反方向收敛：单一 `ArchiveVersion` 全表 v15–v70，容器由版本推导，见 ADR 0004）。
- CLI、N-API、Cargo description、rustdoc 中仍有“仅 RAR5”或“仅创建”的陈旧描述。
- `rar40/mod.rs`、`version.rs`、`CreateOptions` 的注释与现有 RAR4/分卷/头加密能力矛盾。
- `input.rs` 实际主要处理 `rarfiles.lst`，可在独立重构中改名为 `rarfiles.rs`。
- `.scratch` 同时被声明为本地忽略目录和已跟踪计划来源，必须明确其正式身份。
- 建议增加 `CHANGELOG.md`、`SECURITY.md`、贡献指南，并将完成的实施日志从 `PLAN.md` 迁入历史文档。

## CI 与测试基线

审查时的验证结果：

- `cargo fmt --all --check`：通过。
- `cargo clippy --workspace --all-features --all-targets -- -D warnings`：失败，暴露 library test、RAR4 test 和 CLI interop test 中的 lint；现有 CI 未使用 `--all-targets`。
- `cargo test --workspace --all-features`：在 300 秒限制内未完成；超时前显示的测试均通过，不能据此宣称全套通过。
- 手工复现：update 纯新增丢失、成员后缀误匹配、裸密码未加密。
- 官方互操作测试在工具/环境变量缺失时跳过，因此普通 CI 绿色不等于互操作门禁通过。
- `fuzz/` 为独立 workspace，普通 workspace CI 不检查其编译状态。

推荐门禁：

1. PR：fmt、all-targets clippy、default feature、all features、fuzz `cargo check`、核心快速测试。
2. 定时：完整测试、短时 fuzz、官方 UnRAR 互操作、Windows 文件系统测试。
3. Release：所有目标 addon load + 最小 roundtrip、版本一致性、许可证、SBOM、SHA-256、artifact attestation。

## 本轮整改范围

本轮优先完成：

- 已复现 CLI 缺陷及共享 selector。
- 核心解析长度、公开状态 API 和明显资源限制问题。
- N-API 数值验证、错误映射、WASI 参数/路径契约和分卷排序。
- CI all-targets/fuzz check、Release 收集和校验修复、包元数据与陈旧文档。
- 与上述修改直接相关的渐进式边界整理和测试。

以下内容延期到独立破坏性版本：

- crate `rar5` 重命名。
- `ArchiveVersion` 类型拆分。
- 默认隐藏全部低层格式 API。
- codec 热路径的大规模目录迁移。
- 完整的跨平台多卷事务协议。
