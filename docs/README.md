# rar-rs 文档索引

入口是仓库根 [`README.md`](../README.md)。本目录只放**长期有效**的文档；
一次性审查、实施与调试过程不入库——结论归入长期文档，过程留在 git 提交信息。

## 阅读顺序

1. [`../README.md`](../README.md) — 是什么、构建与快速上手。
2. [`CLI.md`](CLI.md) — `rar` / `unrar` 命令、全量开关与退出码。
3. [`ARCHITECTURE.md`](ARCHITECTURE.md) —
   模块地图、分层、设计不变量、CLI/测试布局。
4. [`../CONTEXT.md`](../CONTEXT.md) — 领域词汇（先查术语）。
5. [`../PLAN.md`](../PLAN.md) — 下一步、开放议题与当前判断。
6. [`PITFALLS.md`](PITFALLS.md) —
   长期规则与「别改回去」的地雷，**改行为之前先读**。
7. [`ci.md`](ci.md) — CI 的 job 图、每个 job 被允许证明什么、成本模型，以及
   「刻意没有做什么」。改 `.github/workflows/` 之前先读。
8. [`FORMAT_RAR5_RAR7.html`](FORMAT_RAR5_RAR7.html) — 字节级格式参考。

## 单一来源（谁负责什么）

每个事实只在一个文件维护，其它文档只**链接**、不复制，避免双份漂移：

| 内容                                          | 归属                                                                                                                   |
| --------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| 领域词汇 / 术语                               | [`../CONTEXT.md`](../CONTEXT.md)                                                                                       |
| 模块地图 / 分层 / 设计不变量 / CLI 与测试布局 | [`ARCHITECTURE.md`](ARCHITECTURE.md)                                                                                   |
| CLI 命令、开关与退出码                        | [`CLI.md`](CLI.md)                                                                                                     |
| **长期规则、契约与地雷（含公开 API 契约）**   | [`PITFALLS.md`](PITFALLS.md)                                                                                           |
| 下一步 / 现状 / 开放议题 / 已知小差异         | [`../PLAN.md`](../PLAN.md)                                                                                             |
| 磁盘格式（字节级）                            | [`FORMAT_RAR5_RAR7.html`](FORMAT_RAR5_RAR7.html)                                                                       |
| 架构决策                                      | [`adr/`](adr/)                                                                                                         |
| 压缩性能议题、契约与已否决方向                | [`../PLAN.md`](../PLAN.md)「性能」段 + [`issues/compression-perf/`](issues/compression-perf/)                          |
| 测试怎么跑 / 耗时                             | [`testing.md`](testing.md)                                                                                             |
| fuzz 目标                                     | [`../fuzz/README.md`](../fuzz/README.md)                                                                               |
| 法务与许可                                    | [`../NOTICE`](../NOTICE) · [`../THIRD_PARTY_LICENSES.md`](../THIRD_PARTY_LICENSES.md) · [`../LICENSES/`](../LICENSES/) |

CI 的 job 图、每个 job 的职责与成本见 [`ci.md`](ci.md)（`.github/workflows/`
的单一来源）。

## 决策记录与议题

- **ADR**（`adr/`，当时点的决策）：[0001 RAR4 创建架构](adr/0001-rar4-creation-architecture.md)
  · [0002 格式中立模型与 API v2](adr/0002-format-neutral-model-and-api-v2.md) ·
  [0003 破坏性发布范围](adr/0003-breaking-release-scope.md) ·
  [0004 单一 `ArchiveVersion` 表](adr/0004-single-archive-version-table.md) ·
  [0005 RAR4 编辑架构](adr/0005-rar4-edit-architecture.md) ·
  [0006 公开面收敛到角色门面](adr/0006-public-api-convergence.md) ·
  [0007 删除 `raw` feature](adr/0007-raw-feature-retired.md) ·
  [0008 单向的家族分层与 `Engine` 接缝](adr/0008-one-way-family-layering.md) ·
  [0009 匹配器内核替换（已否决）](adr/0009-match-finder-kernel.md) ·
  [0010 哈希环匹配器（已否决）](adr/0010-hash-ring-finder.md) ·
  [0011 按官方形状重做匹配器（已否决）](adr/0011-official-shaped-matcher.md) ·
  [0012 两段式解析：距离/rep 裁决后置（提议）](adr/0012-deferred-distance-adjudication.md)
- **议题**（`docs/issues/<feature>/`，只留未关闭的）：判决留在议题文件里；议题**关闭时**把结论并进
  [`../PLAN.md`](../PLAN.md)（下一步 / 已知小差异）或
  [`PITFALLS.md`](PITFALLS.md)（长期规则）并删文件。
  [`compression-perf/`](issues/compression-perf/)（未关闭：04、09、15）、
  [`rar5-decode/`](issues/rar5-decode/)（未关闭：`FUN_14007ab9c` 的槽位解析）。
- **规格**：[`rar4-creation-spec.md`](rar4-creation-spec.md) — RAR4
  创建行为与格式。

## 文档格式约定

- **正文尽量每行 ≤ 80 列**，便于按列对照 review。代码块、表格、标题、HTML
  块、链接定义与不可断的长 token（长
  URL、长行内代码、长路径/标识符串）不参与换行。
- **没有 CI 门禁**（2026-11 决定）。`dprint` 曾在独立的 `Docs` workflow 里做
  `check`，已移除：它每次要拉 npm 包与 `plugins.dprint.dev`
  的插件，任一抖动就是与代码正确性无关的红；它的 markdown 插件按空白折行、不实现
  CJK 断行（见下），对中文段落基本只制造噪声。配置仍在
  [`dprint.json`](../dprint.json)（markdown 插件
  0.19.0、`lineWidth: 80`、`textWrap: always`、`newLineKind: lf`），`npx dprint@0.50.2 fmt`
  想跑就跑；`scripts/ci-lint.sh` 只在 PATH 上有 `dprint` 时顺手 `check`。
- **中文段落不要手工折行**：折行处的换行在渲染时是空格，会造成中文之间的假空格。需要窄
  diff 时用 `dprint` 自己折。
- **已知限制**：dprint 的 markdown 插件按空白折行，不实现 CJK/UAX #14 断行（见
  [dprint-plugin-markdown #127][md127]），所以不含空白的纯中文长句只能靠作者在语义边界留空格；少量密集引用行可能超
  80 列，属允许的例外。

## 文档真实性与时效

每条实现性陈述都必须对得上源码或实测。**不加强制校验脚本**（项目决定）；由作者
与评审按下述办法人工核对并留痕。

- **留痕**：面向实现的文档（`README` / `CONTEXT` / `ARCHITECTURE` / `CLI` /
  `testing` / `PITFALLS` / `rar4-creation-spec`）在标题下标注
  `最后核对：YYYY-MM-DD @ <短 commit>`。**改行为必须同步改对应 owner
  文档并刷新日期；只改代码不改文档视为未完成。**
- **怎么核对**（改文档或评审时逐项过）：
  - 反引号里的仓库路径与代码符号 → 在源码里搜到；搜不到的必须**显式标注**历史名
    / 已删除 / 被否方案 / 上游 rars 路径（写明「旧名」「已删」「未采用」）。
  - CLI 开关与语义 → 对 `crates/rar-cli/src/bin/rar/args.rs` 的 clap 面与
    `crates/rar-cli/tests/` 的断言。
  - 常量与数值 → grep 源码常量定义，不凭记忆。
  - 行为差异 / 退出码 → 用官方 WinRAR 7.23 实测。
  - 基准 / 性能数字 → 是特定机器上的**快照**，必须给出可复现命令，不视为契约。
- **时点记录不同**：`adr/` 与 `issues/` 是当时点的决策 /
  实验记录，允许保留旧名与已回退方案，但必须写清状态（accepted /
  superseded、废弃 / 负例）。

## 其它约定

- `.scratch/` 是**本地忽略的临时区**（临时脚本、一次性 dump），不承载任何计划或
  issue——被 `PLAN.md` 引用的东西必须在 `docs/` 下。
- 格式细节以 `FORMAT_RAR5_RAR7.html` 为权威；与 bitplane/rars、rar-research
  冲突处以本实现为准。

[md127]: https://github.com/dprint/dprint-plugin-markdown/issues/127
