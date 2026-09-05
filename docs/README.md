# rar-rs 文档索引

本目录汇集 rar-rs 的全部文档。**入口从仓库根 `README.md` 开始**；领域词汇集中在 `CONTEXT.md`；工程状态与差距清单见 `PLAN.md`。下面按"读者"分组，新读者从对应入口读起即可。

## 起点

| 文档 | 内容 | 何时读 |
|---|---|---|
| [`../README.md`](../README.md) | 项目概览：构建、快速上手、特性与限制 | 第一次接触 / 想知道"这是什么" |
| [`CONTEXT.md`](../CONTEXT.md) | 领域词汇（Archive/Member/Volume/Chunk/…）+ 分层结构 + 项目事实 | 遇到新术语、做架构审查、写 skill 前查这里 |
| [`PLAN.md`](../PLAN.md) | 工程计划：现状、待办、已完成里程碑、一致拒绝、已知差异、改代码前必读 | 计划工作量 / 对齐进度 / 改代码前 |
| [`../CHANGELOG.md`](../CHANGELOG.md) | 面向发布的变更摘要 | 准备发布 / 查询版本变化 |
| [`../SECURITY.md`](../SECURITY.md) | 漏洞报告渠道、范围与披露建议 | 发现安全问题 / 处理恶意归档 |

## 审查与治理

| 文档 | 内容 | 何时读 |
|---|---|---|
| [`CODE_AUDIT_2026-09-05.md`](CODE_AUDIT_2026-09-05.md) | 代码、API、CI、Release 与许可证整改基线 | 排整改优先级 / 验收审查问题 |
| [`CODE_REVIEW_2026-09-05.md`](CODE_REVIEW_2026-09-05.md) | origin/main..HEAD 21-commit 重构双轴审查（Standards/Spec）+ 可执行修复事项表 | 处理审查发现 / 验收重构系列 |
| [`../NOTICE`](../NOTICE) | 项目归属、上游部分和商标说明 | 发布 / 核对来源边界 |
| [`../THIRD_PARTY_LICENSES.md`](../THIRD_PARTY_LICENSES.md) | 第三方来源清单；不是法律结论或完整许可证判定 | 来源审计 / 准备发行材料 |

## 格式与架构（深入）

| 文档 | 内容 | 何时读 |
|---|---|---|
| [`FORMAT_RAR5_RAR7.html`](FORMAT_RAR5_RAR7.html) | 磁盘格式权威参考：块流、逐字节拆解、Extra、压缩/加密/多卷/恢复/Solid/RAR7、差异清单、常量速查 | 需要格式细节 / 校验字节布局 / 对照实现 |
| [`ARCHITECTURE.html`](ARCHITECTURE.html) | 库 crate 模块地图、workspace、设计笔记（有界内存/安全提取/solid 链）、特性矩阵 | 理解代码分层 / 定位模块 |
| [`CLI.md`](CLI.md) | 命令行工具 `rar` / `unrar` 全量开关与命令参考 | 用 CLI / 实现新开关 |

## ADR 与规格

| 文档 | 内容 | 何时读 |
|---|---|---|
| [`adr/0001-rar4-creation-architecture.md`](adr/0001-rar4-creation-architecture.md) | RAR4 创建架构决策记录 | 修改 RAR4 写管线 / 理解决策约束 |
| [`adr/0002-format-neutral-model-and-api-v2.md`](adr/0002-format-neutral-model-and-api-v2.md) | 格式中立模型、依赖方向与 API v2 兼容策略 | 修改核心模型 / 设计公共 API |
| [`plan-architecture-api-v2.md`](plan-architecture-api-v2.md) | 架构与 API v2 分阶段实施清单 | 推进 model、reader/writer/editor 迁移 |
| [`rar4-creation-spec.md`](rar4-creation-spec.md) | RAR4 创建行为与格式规格 | 实现或验证 RAR4 创建 |
| [`plan-rar29-encoder-port.md`](plan-rar29-encoder-port.md) | RAR29 encoder port 实施计划与历史 | 追溯 legacy encoder 迁移 |

## 过程与工具

| 文档 | 内容 | 何时读 |
|---|---|---|
| [`agents/issue-tracker.md`](agents/issue-tracker.md) | 本地 markdown issue 跟踪约定（`.scratch/<feature>/`） | 用 triage/to-tickets/to-spec / 跟踪工作 |
| [`../fuzz/README.md`](../fuzz/README.md) | fuzz 五目标（parse/crypto/recovery/write/rewrite）与双模式运行 | 跑 fuzz / 加模糊目标 |
| [`../crates/rar/tests/fixtures/rar50/README.md`](../crates/rar/tests/fixtures/rar50/README.md) | 真实 WinRAR fixture 的来源与用途 | 理解互操作测试数据 |

## 术语速查

- 写侧把 LZSS 符号流切成**发射块（emitted block）** ≤ 4 MiB（自适应早闭）；解析预算上限 128 KiB（`MAX_BLOCK_SIZE`）。两者解耦。
- 跨卷成员在某卷中的数据段叫 **Chunk（分块）**；连续压缩成员共享的 LZ 窗口叫 **solid 链**。
- 见 [`CONTEXT.md`](../CONTEXT.md) 完整词汇。

> 约定：格式细节以 **`FORMAT_RAR5_RAR7.html`** 为权威；与 bitplane/rars 及 rar-research 冲突处以 rar-rs 实现为准。
