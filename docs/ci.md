# CI: what runs, why it is shaped like this

`.github/workflows/CI.yml` is the only workflow and the only gate. This page is
the map: the job graph, what each job is allowed to prove, what it costs, and
what was deliberately left out. The workflow file stays the source of truth —
when the two disagree, fix one of them in the same commit.

## The graph

```mermaid
flowchart TD
  subgraph trig[Triggers]
    pr[push to a PR / pull_request]
    main[push to main]
    tag["push of a tag v*"]
    disp["workflow_dispatch (± release_tag)"]
    cron[schedule, Mon 04:00 UTC]
  end

  subgraph linux[Linux, every trigger]
    check["check — static gate<br/>actionlint · shellcheck · fmt ·<br/>guards · wasm check · clippy ×4 · deny<br/>(+ rustdoc on heavy runs)"]
    test["test — behaviour gate<br/>unit + CLI + napi lib · JS path map<br/>· zig installer<br/>(scalar corpus on schedule/tag)"]
    msrv["msrv — declared floor<br/>rust-version from Cargo.toml"]
  end

  win["lint-windows — the Windows<br/>compiler gate (not on schedule;<br/>corpus on tags/dispatches)"]

  sel["build-matrix — pick targets from<br/>scripts/ci/build-matrix.js:<br/>9 on heavy runs, 1 on a PR"]

  subgraph heavy[Addon work: tags, dispatches, the weekly run, every PR]
    build["build — the chosen addon targets<br/>(napi build, + the tsc type gate once)"]
    wasm["build-wasm — wasi bundle + loader patch"]
  end

  rel["release — tag (or dispatch naming one)<br/>version check · assets · SHA-256SUMS<br/>· grouped notes · idempotent upload"]

  pub["publish — crates.io<br/>Trusted Publishing (OIDC) · rar-rs first<br/>· index polled · versions already up skipped"]

  pr --> check & test & msrv & win
  main --> check & test & msrv & win
  cron --> check & test & msrv
  tag --> check & test & msrv & win
  disp --> check & test & msrv & win
  pr --> sel & wasm
  cron --> sel & wasm
  tag --> sel & wasm
  disp --> sel & wasm
  sel --> build
  tag --> rel
  disp --> rel

  check --> rel
  test --> rel
  msrv --> rel
  win --> rel
  build -->|"one addon artifact per target"| rel
  wasm -->|"bindings-wasm32-wasip1-threads"| rel
  rel -->|"only after the Release exists"| pub
```

Two properties of the shape are worth reading off the diagram:

- **Nothing is upstream of `check`/`test`/`msrv`/`lint-windows`/`build-matrix`.
  They start together on every trigger, so a PR gets its answer in the time of
  the slowest job instead of the sum. The price is that a formatting mistake
  also burns the test minutes; paid on purpose — a red `check` names the
  problem, and a serialised pipeline would give one failure at a time.
- **`release` and `publish` are the only jobs with `needs`.** Everything else is
  independent, so a broken job can never hide behind an earlier failure, and
  `release` cannot assemble a partial artifact set. `build` needs `build-matrix`
  only because it reads the target list from it. `publish` needs `release`
  because crates.io versions are permanent (yankable, never replaceable): the
  version must only reach the registry after the tag has produced its Release.

## What each job is allowed to prove

| Job                 | Proves                                                                                                                                                                                                                                                                                                     | Not merged into a neighbour because                                                                                                                                                                                                                                             |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `check`             | The tree compiles, is formatted, is lint-clean, its manifest invariants hold, its dependencies are legal, its docs build.                                                                                                                                                                                  | Runs no test suite: a red `check` and a red `test` mean different things, and the log you open first should be the right one.                                                                                                                                                   |
| `test`              | Behaviour: library unit tests, the CLI suites, the binding's Rust tests, the JS path-mapping test, the zig installer's failure modes — plus the 10 generated legacy-corpus targets, which the first step already runs under `--all-features`; the second, scalar-build run of them is `schedule`/tag only. | Same reason, mirrored. It also installs no clippy/rustfmt and no wasm target — `-D warnings` on the test profile would report a warning that `check` already fails on.                                                                                                          |
| `msrv`              | The `rust-version` in `Cargo.toml` compiles, using exactly that toolchain with `--all-features`.                                                                                                                                                                                                           | `check`/`test` run whatever `stable` is today, so without this job `rust-version` is a comment. Kept separate so the failure reads "the floor broke", not "the tree broke".                                                                                                     |
| `lint-windows`      | Everything under `#[cfg(windows)]`: the CLI switch contract, separator hazards, the binding's `error.rs`/`guard.rs` tests; on tags and dispatches, also the generated corpus against the official Windows builds (`SA_REQUIRE_WINRAR=1`).                                                                  | Windows minutes are ~4× Linux; splitting it into a static and a test job would double the runner cost for a log-clarity gain that the Linux split already provides. The WinRAR fetch is what makes the corpus step expensive, so it follows the corpus and runs release-shaped. |
| `build-matrix`      | Which addon targets this event builds: the whole list on tags, dispatches and the weekly run; the one `smoke` entry on a pull request.                                                                                                                                                                     | The list is data (`scripts/ci/build-matrix.js`), not a `matrix:` literal, because two events need two different subsets of the same list, and a duplicated literal would let the PR smoke target drift away from the target the release builds.                                 |
| `build` (`×1`–`×9`) | A loadable `.node` exists for each target the selector chose, plus the `tsc` type gate once, on the entry that generated `binding.d.ts`.                                                                                                                                                                   | One job per target so `fail-fast: false` reports every broken platform in one run instead of one per push. On a PR one target runs, so the napi pipeline and the type gate are exercised before a tag (see below).                                                              |
| `build-wasm`        | The wasi bundle: 8 files, loader patched for Windows host paths.                                                                                                                                                                                                                                           | Cheap and independent; a red wasm run should not make 9 addon jobs look broken.                                                                                                                                                                                                 |
| `release`           | The tag, the version match, the exact asset set, the checksums, the notes; and that re-running it updates instead of failing.                                                                                                                                                                              | —                                                                                                                                                                                                                                                                               |
| `publish`           | That all three crate versions on the index match the tag, in dependency order, through a short-lived OIDC token rather than a stored secret; already-published versions are skipped.                                                                                                                       | Publishing cannot be undone on crates.io, so it is the one job that must not run until every other gate has passed — `needs: release` is that ordering, and it makes a re-release dispatch a gap-filler instead of a failure.                                                   |

## The release path is rehearsed before the tag

A green pull request used to say nothing about a release: the addon build, the
`tsc` type gate, the wasi bundle and the asset contract all ran for the first
time on the tag. Five changes close that gap — two cost seconds, three cost
minutes:

| Release-only step                                                              | Where it now runs on a PR                        | What it costs          |
| ------------------------------------------------------------------------------ | ------------------------------------------------ | ---------------------- |
| asset contract (9 addons, 8 WASI files, identical loaders, license flattening) | `check`, as `scripts/ci/test-release-scripts.sh` | seconds                |
| release-notes grouping and the previous-tag range                              | same fixture test, in `check`                    | seconds                |
| `napi build` + `tsc` type gate                                                 | `build`, on the one `smoke` matrix entry         | one addon build        |
| wasi bundle + loader patch                                                     | `build-wasm`                                     | ~4 min                 |
| the eight cross targets (musl, arm, darwin, win32)                             | weekly schedule, full matrix, no publish         | weekly                 |
| the corpus on Windows, against the official WinRAR builds                      | tag and dispatch (same runs that publish)        | minutes, 4×            |
| crate publish order, index polling, skip-if-already-published                  | `check`, in the same fixture test (stub index)   | 2 curl-less assertions |

No rehearsal gaps are left in that table, but three rows are release-shaped
rather than per-PR on purpose: a second _library configuration_ of a suite that
already runs on every push, and a Windows corpus whose setup downloads four
official releases. Both run on every tag before `release` consumes the
artifacts, which is the run that matters; the weekly job covers the first, so a
break is found within seven days instead of at the tag. The third is the token
exchange: a `release_tag` dispatch naming a tag whose versions are all on the
index runs `publish` end to end and publishes nothing, which is the rehearsal.

The rules that used to be shell inside `release` are now
`scripts/ci/collect-release-assets.sh` and `scripts/ci/release-notes.sh`, which
is what makes the fixture test possible at all. The fixture test also asserts
that the build matrix's addon names equal the ones the release demands, so the
two lists cannot disagree silently.

What a PR still cannot prove, by construction: that `gh release` accepts the
upload, and that the eight cross-target binaries load on their platforms. The
first is covered by idempotence (a re-run converges instead of failing) and by
`workflow_dispatch` with `release_tag` (same code path, an existing tag); the
second by the weekly full-matrix run. Deliberately not bought: nine addon builds
on every PR — that is the runner bill this shape exists to avoid.

## Cost

Rough runner-minutes per event (Linux ≈ 1×, Windows ≈ 4× billed):

| Event                      | Jobs | Notes                                                                                                                                                |
| -------------------------- | ---- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| PR                         | 7    | ~2 min faster than a run that also re-ran the corpus against a scalar build; the static gates answer in ~2 min, the single addon build sets the tail |
| push to main               | 4    | no addon work — the pull request already built that tree                                                                                             |
| push of a tag              | 17   | dominated by the 9-target build matrix; `release` then `publish`, the latter usually skipping every crate                                            |
| weekly schedule            | 14   | the full matrix + wasi bundle, so the heavy path is proven without publishing; also the advisory DB and "does today's stable build it"               |
| dispatch, no `release_tag` | 15   | the smoke path for workflow changes: builds the matrix against the default branch, publishes nothing                                                 |
| dispatch, `release_tag`    | 17   | `release` re-uploads the assets, `publish` proves the OIDC exchange and skips every version already on the index                                     |

Caches that keep this down: `Swatinem/rust-cache` with one `linux` key shared by
`check`/`test` (same toolchain and lock, so one dependency cache serves both), a
separate `msrv` key (different toolchain), a per-target `cargo` key for the
build matrix, and the zig/cargo-zigbuild and WinRAR-download caches.

## Decisions in this shape

| Decision                                                  | Why                                                                                                                                                                                                                                                                                                                                                                                                                   |
| --------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| No `paths-ignore`                                         | It skipped a tag whose diff happened to be markdown-only (silent non-release) and skipped the required checks on docs-only PRs (permanent "waiting"). A docs push is worth the minutes.                                                                                                                                                                                                                               |
| `workflow_dispatch.inputs.release_tag`                    | A release that died mid-upload used to be retryable only by deleting and re-pushing the tag.                                                                                                                                                                                                                                                                                                                          |
| `release` is idempotent                                   | `gh release view` → `edit --notes-file` + `upload --clobber`; else `create --verify-tag`. Re-running publishes the same assets instead of a second tag, so a run that died mid-upload converges. The consumer pins digests, so a re-run that changes a byte is a re-pin on that side — Windows addons are not reproducible, Linux and macOS are.                                                                      |
| Notes are generated from commits                          | `--generate-notes` renders once and never refreshes on a re-release; grouping `feat`/`fix`/`perf` from `git log --no-merges` also makes the notes reviewable in the diff of this file.                                                                                                                                                                                                                                |
| `cancel-in-progress` only for PRs                         | Cancelling a `main` or tag run can leave a release half-published; a PR run only wastes minutes.                                                                                                                                                                                                                                                                                                                      |
| actionlint + shellcheck are steps, actionlint is verified | The workflows are the one part of the repo nothing compiled. `scripts/ci/run-actionlint.sh` downloads a pinned tarball and checks it against the SHA-256 from actionlint's own release index. shellcheck runs at `-S warning` — at the default style level its info findings fire on deliberate constructs.                                                                                                           |
| `msrv` reads `Cargo.toml`                                 | The version lives in one place; a job that hard-codes it would let the manifest drift.                                                                                                                                                                                                                                                                                                                                |
| `publish` uses Trusted Publishing, not a secret           | crates.io mints a token per run through `rust-lang/crates-io-auth-action@v1` and `id-token: write`, so a stolen repository secret cannot publish a version; the trust is per crate (`rar-rs`, `rar-cli`, `rar-rs-napi`) and names this workflow file. It also waits on `needs: release`, because a version that reached the registry cannot be replaced — only yanked.                                                |
| `publish` skips what is already on the index              | A `release_tag` dispatch exists to repair a half-published release; failing on the versions that did make it would defeat that. The skip also makes a re-pointed tag a no-op instead of a second permanent version.                                                                                                                                                                                                   |
| Dependabot, grouped                                       | The repo had already drifted (`actions/cache@v4` beside `v6`). Dependabot does not know `rust-version`, so a bump that raises the floor surfaces as a red `msrv` job.                                                                                                                                                                                                                                                 |
| `retention-days: 7` on the artifacts                      | `release` consumes them in the same run; the 90-day default stores nine addons for three months.                                                                                                                                                                                                                                                                                                                      |
| The addon matrix is data, in `scripts/ci/build-matrix.js` | Two events need two subsets of one list — nine entries on a tag, one on a PR. A `matrix:` literal per job would duplicate the targets, and a duplicated list is how the smoke target silently stops being the target that ships.                                                                                                                                                                                      |
| A PR builds one addon, not nine                           | Rehearses the napi pipeline and the `tsc` type gate (which otherwise first ran on a tag) for ~8 runner-minutes in parallel, so wall clock does not move; nine would be the bill this shape avoids.                                                                                                                                                                                                                    |
| A PR builds the wasi bundle                               | ~4 minutes, in parallel, and the only thing that patches the loader for Windows host paths — a break there is a release break, not a Linux-gate break.                                                                                                                                                                                                                                                                |
| The weekly run builds the full matrix                     | The eight cross targets are unreachable on a PR without wasting minutes; a schedule proves them without publishing, so "the tag failed on armv7" stops being the first signal.                                                                                                                                                                                                                                        |
| clippy ×4, not ×5                                         | `crates/rar` has `default = []` and exactly two features, so `--features parallel,simd` _is_ `--all-features`, which the workspace step already runs. The ungated build and the two single-feature builds are what the matrix still adds — each one is a configuration the workspace build cannot see.                                                                                                                |
| The scalar-build corpus run is `schedule`/tag only        | The `--all-features` step above it already executes those suites on every push, so what this step adds is the same assertions against a `default = []` library — worth one extra compile of the crate plus every test binary, not worth it on every push. `check` keeps the scalar configuration compiling on every push.                                                                                             |
| The Windows corpus is release-shaped                      | Its setup is four official WinRAR releases plus minutes of download at 4× billing, and the corpus is the only consumer. The `#[cfg(windows)]` unit tests and clippy stay on every PR: those are the parts a PR is actually at risk of breaking.                                                                                                                                                                       |
| No markdown gate                                          | `dprint` left CI (2026-11): the check pulled the npm package and `plugins.dprint.dev` on every run, so a network hiccup was a red build unrelated to the change, and its markdown plugin wraps on whitespace only — it cannot break Chinese prose, so on this repo's docs it mostly re-flowed text into meaningless breaks. It stays in `dprint.json` and in `scripts/ci-lint.sh` for whoever has the binary on PATH. |
| One installer for zig and cargo-zigbuild                  | Only the two musl rows need either, and they need both (`napi build -x` is cargo-zigbuild, which drives `zig`). The script installs whichever half its version variable names, which is also what lets `scripts/ci/test-install-zig.sh` exercise the zig half alone.                                                                                                                                                  |

## Deliberately absent

- **`cargo publish --dry-run` / `cargo package` gate.** The owner deferred it
  ("p0 3不做"); when it lands it belongs in `release` before the upload, not in
  `check`, because it validates the packaged crate rather than the tree.
- **A macOS job on PRs.** There is no `cfg(target_os = "macos")` in the tree
  (the platform-specific code is `unix` 66 sites and `windows` 102), so macOS
  adds no compile coverage on a PR; the `aarch64-apple-darwin` matrix entry
  exists to publish an addon. Adding a macOS job would buy runner minutes and
  nothing else.
- **Splitting `lint-windows`.** See the table above.
- **SHA-pinning third-party actions.** They are pinned by major tag and tracked
  by Dependabot; the trade-off is a supply-chain surface in exchange for
  readable versions. Revisit if the repo ever gets a release key worth stealing
  (it currently publishes with `GITHUB_TOKEN` and no registry tokens).
- **The JS/WASI binding suites in CI.** Owner's call (2026-09): they were the
  one flaky gate; they stay local. `docs/testing.md` is where the local covers
  them.

## Changing it safely

1. Edit `CI.yml` (or a script it calls; the scripts under `scripts/ci/` exist so
   the same check can run locally — see `scripts/ci-lint.sh`).
2. `scripts/ci-lint.sh --no-tests` — actionlint, shellcheck, the release
   fixtures, fmt, the guards, clippy, rustdoc. This is the same set `check`
   runs.
3. `bash scripts/ci/test-release-scripts.sh` on its own when editing the release
   scripts (`collect-release-assets.sh`, `release-notes.sh`,
   `publish-crates.sh`) or the build matrix (it is part of step 2 as well).
4. `workflow_dispatch` with an empty `release_tag` — builds the whole matrix
   against the default branch and stops before `release`. That is the only way
   to test the heavy half without cutting a tag.
5. To re-publish an existing tag: dispatch with `release_tag` set to it. If its
   versions are already on crates.io, `publish` prints a `skip` for each and the
   run is the rehearsal of the OIDC exchange.
6. A **new** crate name cannot be published through OIDC: crates.io needs the
   crate to exist before a Trusted Publisher can be attached to it, so the first
   upload is a manual `cargo publish` with `publish-new`.
