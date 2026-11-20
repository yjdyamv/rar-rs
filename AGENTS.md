# AGENTS.md

本仓库面向编码代理的规则。背景与理由见 [`docs/ci.md`](docs/ci.md)（CI 形态、每个
job 能证明什么）与 [`docs/testing.md`](docs/testing.md)（测试分层）。

## 提交消息

- **不得**在提交消息中加入 `Co-authored-by` 或任何同类署名
  trailer，大小写不限（`Co-Authored-By:`、`Co-authored-by:`
  …）；`Generated-by`、`Signed-off-by`
  之类的协作/生成工具署名行同样禁止。提交消息只写标题与正文。
- 标题用
  `area: imperative`（`ci: …`、`rar5: …`、`docs: …`）。这不是风格问题：release
  notes 由 `git log --no-merges`
  的前缀分组生成——`^(feat|perf)`、`^(fix)`、`^(docs|style|chore|test)`，其余一律落进
  "Everything else"。

## 推之前

- `bash scripts/ci-lint.sh`（`--no-tests` 只跑静态部分；完整跑与 CI 的 `test`
  目标相同）。
- 动了 `.github/workflows/` 再跑 `bash scripts/ci/run-actionlint.sh`；动了
  `scripts/ci/collect-release-assets.sh` 或 `release-notes.sh` 再跑
  `bash scripts/ci/test-release-scripts.sh`。actionlint/shellcheck 不在 PATH 时
  `ci-lint.sh` 打印 `SKIP:` ——**那不算通过**。
- 本地 `cargo test` 缺 WinRAR
  时，依赖官方工具的语料套件**静默跳过**，`test result: ok`
  不代表跑过。要硬失败设 `SA_REQUIRE_WINRAR=1`。CI 的 `test` job 先下载
  WinRAR，所以那里是真跑。

## 不能只改一处的东西

- **版本号五处**：`crates/rar/Cargo.toml`、`crates/rar-cli/Cargo.toml`、`crates/rar-napi/Cargo.toml`、`crates/rar-napi/package.json`、根
  `Cargo.toml` 里的 `rar-rs = { version = … }`。`scripts/ci/check-versions.sh`
  比对它们与 workspace 版本，`check` 和 `release` 都跑。
- **addon 名单纯数据**在 `scripts/ci/build-matrix.js`（9 条，一条标
  `smoke`），必须与 `scripts/ci/collect-release-assets.sh` 的 `EXPECTED_ADDONS`
  一致：`test-release-scripts.sh` 会断言两者相等，改一个就要改另一个。
- **MSRV 是 1.95**（`Cargo.toml` 的
  `rust-version`）：`crates/rar/src/archive/reader.rs` 用
  `AtomicU32::try_update`（1.95 稳定）。更旧的工具链会直接拒绝这个 crate。
- 构建产物不入库：`crates/rar-napi/*.node`、`binding.js`、`binding.d.ts` 由
  `napi build` 生成（`binding.js` 里还留着生成时的版本号，会过期）。
- **提交任何二进制/字节语料**（测试 fixture、样例、图片）必须同时在
  `THIRD_PARTY_LICENSES.md` 的 `## Test data` 表里写清来源、授权与生成方式。
  优先现场生成字节（`scripts/fixtures/` 下的源码 + 一个 gen 脚本），而不是提交
  一个来路不明的文件——仓库整体声明 `BSD-2-Clause`，一个没登记的第三方二进制会
  让这个声明失真（issue #2 就是这么来的：一段 `ntoskrnl.exe` 前缀在
  `crates/rar/tests/data/` 待了几个月没人问它从哪来）。

## CI 形态的硬约束

- job 级 `if:` 里**不能**用 `env` 与 `matrix` 上下文；按事件挑矩阵子集必须走
  `fromJSON(needs.build-matrix.outputs.settings)`。这就是 `build`/`build-wasm`
  的条件写得重复、矩阵放在数据文件里的原因。
- 文档没有 CI 门禁（`dprint`
  只是可选本地工具）。**中文段落不要手工折行**：折行处的换行在渲染时是空格，会造成中文之间的假空格。
