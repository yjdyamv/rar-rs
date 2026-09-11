# rar-rs 文档索引

入口在仓库根 [`README.md`](../README.md)。本目录只放**长期有效**的文档：领域词汇在
[`CONTEXT.md`](../CONTEXT.md)，工程状态与差距在 [`PLAN.md`](../PLAN.md)，一次性审查/实施
计划不入库（结论归入前两者，过程在 git 历史）。

## 起点

| 文档 | 内容 | 何时读 |
|---|---|---|
| [`../README.md`](../README.md) | 项目概览：构建、快速上手、特性与限制 | 第一次接触 |
| [`CONTEXT.md`](../CONTEXT.md) | 领域词汇（Archive/Member/Volume/Chunk/…）+ 分层结构 + 项目事实 | 遇到新术语 / 做架构审查 |
| [`PLAN.md`](../PLAN.md) | 现状、待办、技术债、加固记录、一致拒绝、已知差异 | 计划工作量 / 改代码前 |
| [`../SECURITY.md`](../SECURITY.md) | 漏洞报告渠道、范围与披露建议 | 发现安全问题 / 处理恶意归档 |

## 格式与架构

| 文档 | 内容 | 何时读 |
|---|---|---|
| [`FORMAT_RAR5_RAR7.html`](FORMAT_RAR5_RAR7.html) | 磁盘格式**权威参考**：块流、逐字节拆解、Extra、压缩/加密/多卷/恢复/Solid/RAR7、常量速查 | 需要格式细节 / 校验字节布局 |
| [`ARCHITECTURE.html`](ARCHITECTURE.html) | 库 crate 模块地图、workspace、设计笔记（有界内存/安全提取/solid 链）、特性矩阵 | 理解分层 / 定位模块 |
| [`CLI.md`](CLI.md) | `rar` / `unrar` 全量开关与命令参考 | 用 CLI / 实现新开关 |

## 决策记录（ADR 与规格）

| 文档 | 内容 |
|---|---|
| [`adr/0001-rar4-creation-architecture.md`](adr/0001-rar4-creation-architecture.md) | RAR4 创建架构决策 |
| [`adr/0002-format-neutral-model-and-api-v2.md`](adr/0002-format-neutral-model-and-api-v2.md) | 格式中立模型、依赖方向、API v2 兼容策略（版本轴部分已被 0004 取代） |
| [`adr/0003-breaking-release-scope.md`](adr/0003-breaking-release-scope.md) | 公开面收敛范围：`raw` 门控、破坏性发布清单 |
| [`adr/0004-single-archive-version-table.md`](adr/0004-single-archive-version-table.md) | 单一 `ArchiveVersion` 表（v15–v70），废弃容器轴与 `CompressionVersion` |
| [`adr/0005-rar4-edit-architecture.md`](adr/0005-rar4-edit-architecture.md) | RAR4 编辑架构（对齐官方、solid 整档 repack、分卷/-hp 边界） |
| [`rar4-creation-spec.md`](rar4-creation-spec.md) | RAR4 创建行为与格式规格 |

## 过程与工具

| 文档 | 内容 | 何时读 |
|---|---|---|
| [`testing.md`](testing.md) | 怎么跑测试、耗时花在哪、`[profile.test]` 为何开优化、nextest 的坑 | 跑测试 / 觉得套件慢 / 改测试 |
| [`issues/compression-perf/map.md`](issues/compression-perf/map.md) | 压缩性能议题地图：结论、已关闭议题判决、Open frontier、与 WinRAR 7.23 的头对头 | 排压缩性能工作 |
| [`issues/compression-perf/issues/`](issues/compression-perf/issues/) | 仅存**未关闭**议题（04、09）；已关闭的结论见 map.md 表格 | 接手未关闭的性能议题 |
| [`../fuzz/README.md`](../fuzz/README.md) | fuzz 五目标与双模式运行 | 跑 fuzz / 加模糊目标 |
| [`../crates/rar/tests/fixtures/rar50/README.md`](../crates/rar/tests/fixtures/rar50/README.md) | 真实 WinRAR fixture 的来源与用途 | 理解互操作测试数据 |

## 约定

- **本地 issue**写在 `docs/issues/<feature>/`，只保留未关闭的；关闭后结论并入 `map.md` 并删文件。
- `.scratch/` 是**本地忽略的临时区**（临时脚本、一次性 dump），不承载任何计划或 issue ——
  被 `PLAN.md` 引用的东西必须在 `docs/` 下。
- 格式细节以 `FORMAT_RAR5_RAR7.html` 为权威；与 bitplane/rars、rar-research 冲突处以本实现为准。
- 领域词汇：写侧 LZSS 符号流切成**发射块** ≤ 4 MiB（自适应早闭），解析预算上限 128 KiB
  （`MAX_BLOCK_SIZE`），两者解耦；跨卷数据段叫 **Chunk**，共享 LZ 窗口叫 **solid 链**。完整表见 `CONTEXT.md`。
