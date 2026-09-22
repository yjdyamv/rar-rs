# rar-rs 文档索引

入口是仓库根 [`README.md`](../README.md)。本目录只放**长期有效**的文档；
一次性审查、实施与调试过程不入库——结论归入长期文档，过程留在 git 提交信息。

## 阅读顺序

1. [`../README.md`](../README.md) — 是什么、构建与快速上手。
2. [`CLI.md`](CLI.md) — `rar` / `unrar` 命令与全量开关。
3. [`ARCHITECTURE.md`](ARCHITECTURE.md) —
   模块地图、分层、设计不变量、CLI/测试布局。
4. [`../CONTEXT.md`](../CONTEXT.md) — 领域词汇（先查术语）。
5. [`../PLAN.md`](../PLAN.md) — 现状、下一步、限制与已接受的差异。
6. [`FORMAT_RAR5_RAR7.html`](FORMAT_RAR5_RAR7.html) — 字节级格式参考。

## 单一来源（谁负责什么）

每个事实只在一个文件维护，其它文档只**链接**、不复制，避免双份漂移：

| 内容                                          | 归属                                                                                                                   |
| --------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| 领域词汇 / 术语                               | [`../CONTEXT.md`](../CONTEXT.md)                                                                                       |
| 模块地图 / 分层 / 设计不变量 / CLI 与测试布局 | [`ARCHITECTURE.md`](ARCHITECTURE.md)                                                                                   |
| CLI 命令与开关                                | [`CLI.md`](CLI.md)                                                                                                     |
| 现状 / 下一步 / 限制 / 一致拒绝 / 已知小差异  | [`../PLAN.md`](../PLAN.md)                                                                                             |
| 磁盘格式（字节级）                            | [`FORMAT_RAR5_RAR7.html`](FORMAT_RAR5_RAR7.html)                                                                       |
| 架构决策                                      | [`adr/`](adr/)                                                                                                         |
| 压缩性能议题与契约                            | [`issues/compression-perf/`](issues/compression-perf/) + [`../PLAN.md`](../PLAN.md)「性能」段                          |
| 测试怎么跑 / 耗时                             | [`testing.md`](testing.md)                                                                                             |
| fuzz 目标                                     | [`../fuzz/README.md`](../fuzz/README.md)                                                                               |
| 法务与许可                                    | [`../NOTICE`](../NOTICE) · [`../THIRD_PARTY_LICENSES.md`](../THIRD_PARTY_LICENSES.md) · [`../LICENSES/`](../LICENSES/) |

## 决策记录与议题

- **ADR**（`adr/`，当时点的决策）：[0001 RAR4 创建架构](adr/0001-rar4-creation-architecture.md)
  · [0002 格式中立模型与 API v2](adr/0002-format-neutral-model-and-api-v2.md) ·
  [0003 破坏性发布范围](adr/0003-breaking-release-scope.md) ·
  [0004 单一 `ArchiveVersion` 表](adr/0004-single-archive-version-table.md) ·
  [0005 RAR4 编辑架构](adr/0005-rar4-edit-architecture.md) ·
  [0006 公开面收敛到角色门面](adr/0006-public-api-convergence.md) ·
  [0007 删除 `raw` feature](adr/0007-raw-feature-retired.md) ·
  [0008 单向的家族分层与 `Engine` 接缝](adr/0008-one-way-family-layering.md)
- **议题**（`docs/issues/<feature>/`，只留未关闭的；关闭后**判决并入
  [`../PLAN.md`](../PLAN.md)「性能」段**并删文件）：
  [`compression-perf/`](issues/compression-perf/)（未关闭议题 04、09、15）
- **规格**：[`rar4-creation-spec.md`](rar4-creation-spec.md) — RAR4
  创建行为与格式。

## 文档格式约定

- **每个 `.md` 必须能被 `dprint` 幂等格式化**：正文尽量每行 ≤ **80 列**。
  代码块、表格、标题、HTML 块、链接定义与不可断的长 token（长 URL、长行内代码、
  长路径/标识符串）不参与换行。
- 配置在 [`dprint.json`](../dprint.json)：markdown 插件
  0.19.0、`lineWidth: 80`、 `textWrap: always`、`newLineKind: lf`。写作后跑
  `npx dprint@0.50.2 fmt`；CI 的 `Docs` workflow 跑 `check`，未格式化会让 CI
  变红。
- **中文段落不要手工折行**：dprint
  把折行处的换行当空格，会造成中文之间的假空格。 写完跑 `fmt`，由它按空白折行。
- **已知限制**：dprint 的 markdown 插件按空白折行，不实现 CJK/UAX #14 断行（见
  [dprint-plugin-markdown #127][md127]），所以不含空白的纯中文长句只能靠作者在
  语义边界留空格；少量密集引用行仍可能超 80 列，属允许的例外。

## 文档真实性与时效

每条实现性陈述都必须对得上源码或实测。**不加强制校验脚本**（项目决定）；由作者
与评审按下述办法人工核对并留痕。

- **留痕**：面向实现的文档（`README` / `CONTEXT` / `ARCHITECTURE` / `CLI` /
  `testing` / `rar4-creation-spec`）在标题下标注
  `最后核对：YYYY-MM-DD @ <短 commit>`。**改行为必须同步改对应 owner 文档并刷新
  日期；只改代码不改文档视为未完成。**
- **怎么核对**（改文档或评审时逐项过）：
  - 反引号里的仓库路径与代码符号 → 在源码里搜到；搜不到的必须**显式标注**历史名
    / 已删除 / 被否方案 / 上游 rars 路径（写明「旧名」「已删」「未采用」）。
  - CLI 开关与语义 → 对 `crates/rar-cli/src/bin/rar/args.rs` 的 clap 面与
    `crates/rar-cli/tests/` 的断言。
  - 常量与数值 → grep 源码常量定义，不凭记忆。
  - 行为差异 / 退出码 → 用官方 WinRAR 7.23 实测。
  - 基准 / 性能数字 → 是特定机器上的**快照**，必须给出可复现命令，不视为契约。
- **时点记录不同**：`adr/` 与 `issues/` 是当时点的决策 /
  实验记录，允许保留旧名与 已回退方案，但必须写清状态（accepted /
  superseded、废弃 / 负例）。

## 其它约定

- `.scratch/` 是**本地忽略的临时区**（临时脚本、一次性 dump），不承载任何计划或
  issue——被 `PLAN.md` 引用的东西必须在 `docs/` 下。
- 格式细节以 `FORMAT_RAR5_RAR7.html` 为权威；与 bitplane/rars、rar-research
  冲突处 以本实现为准。

[md127]: https://github.com/dprint/dprint-plugin-markdown/issues/127
