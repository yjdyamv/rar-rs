# Testing

> Last verified: 2026-09-19 @ `c2db547`; the timing table is a host-specific
> snapshot, not a contract.

How the suite is organized, what it costs, and the traps to know before changing
it.

## Running

```sh
cargo test --workspace --all-features     # everything
cargo test --package rar-rs --lib         # library unit tests only
cargo test --package rar-rs --lib -- archive::   # one module
```

`cargo nextest run --workspace --all-features` also works, is roughly twice as
fast (it parallelizes across test binaries instead of running them one after
another) and prints a per-test timing report, which is the easiest way to find
what got slow. It is a local convenience only: CI stays on plain `cargo test`,
and read the `rarfiles.lst` trap below before trusting it.

## Checking the Linux half from Windows

CI's `lint` job runs on Linux, and `#[cfg(windows)]` / `cfg!(windows)` branches
mean a green local run can still fail there (a `let mut` that is only pushed on
Windows trips `unused_mut` under `-D warnings`, and tests asserting Windows-only
path hazards fail when POSIX keeps the name). Compile the Linux half locally
with the cross target (clippy/check do not link, so no cross toolchain is
needed):

```sh
rustup target add x86_64-unknown-linux-gnu    # once
cargo clippy --workspace --all-features --all-targets --locked \
  --target x86_64-unknown-linux-gnu -- -D warnings
```

The cfg-gated test branches can still only _run_ on Linux; assert what POSIX
actually does (see `extract_rejects_unsafe_entry_names`) instead of assuming a
Windows-only hazard.

The `wasm32-wasip1-threads` half is a third such surface, and the quietest: it
is neither unix nor windows, so helpers those two branches use are dead there.
The `lint` job lints it (a `-D warnings` check) and locally:

```sh
rustup target add wasm32-wasip1-threads    # once
RUSTFLAGS="-D warnings" cargo check -p rar-rs --all-features \
  --target wasm32-wasip1-threads --locked
```

The binding crate cannot be checked that way (`napi-build` needs the
`EMNAPI_LINK_DIR` that `napi build` injects); build it with
`npx napi build --platform --release --target wasm32-wasip1-threads` instead.

## Dependency gate (`cargo deny`)

The `lint` job also runs `cargo deny check --all-features --locked`, which
judges the _dependency graph_ rather than this tree: RustSec advisories, the
license allow-list and the registry sources. Configuration and the rationale for
each allowed license family live in the root [`deny.toml`](../deny.toml).
Locally:

```sh
cargo install cargo-deny --locked    # once
cargo deny check --all-features --locked
```

Adding a dependency whose license is not in the allow-list fails the check on
purpose: the license decision is made when the dependency is added, not at
release time. In-tree `rars` ports are _not_ covered by this file — their
provenance is recorded in
[`THIRD_PARTY_LICENSES.md`](../THIRD_PARTY_LICENSES.md) and
[`NOTICE`](../NOTICE).

## Running the Linux half under WSL2

WSL2 runs the job for real, including the official-tool interop suites. The
`scripts/wsl/` helpers set the box up and drive it; mirrors were picked from
measurements on the author's box (2026-09-17: 8 MiB ranges of the real `rustc`
tarball, a real crate download, a real `Packages.gz` — re-measure before
trusting them elsewhere):

| mirror  | toolchain | crate     | ubuntu    |
| ------- | --------- | --------- | --------- |
| aliyun  | 2.41 MB/s | 2.08 MB/s | 2.78 MB/s |
| tencent | 2.50 MB/s | —         | 2.69 MB/s |
| nju     | 2.31 MB/s | 403       | 1.70 MB/s |
| ustc    | 0.89 MB/s | 0.13 MB/s | 0.87 MB/s |
| tuna    | 0.20 MB/s | 0.05 MB/s | 0.36 MB/s |
| sustech | —         | —         | 1.96 MB/s |

Caveats behind those columns: Aliyun's rustup manifest was months stale (1.96.0
vs the then-current 1.98.1) and rewrites component URLs to itself, which is why
`rustup target add wasm32-wasip1-threads` 404s there, and it does not mirror
`rustup-init`; NJU refuses the crate download (`dl` URL 403); TUNA, USTC,
Tencent and NJU keep the manifests pointing at `static.rust-lang.org`, so they
honor `RUSTUP_DIST_SERVER`; SUSTech has no Rust mirror at all.

The scripts therefore use: apt and crates.io from Aliyun, the toolchain from
Tencent, `rustup-init` from USTC, Node 24 from Aliyun (the only mirror that
carries it here) and npm from `registry.npmmirror.com`.

```sh
sudo bash scripts/wsl/setup-apt.sh     # mirror + build-essential (root)
bash scripts/wsl/setup-rust.sh         # toolchain, cargo/crates mirrors
bash scripts/wsl/setup-node.sh         # Node 24 + npm mirror
git clone /mnt/c/path/to/rar-rs ~/rar-rs   # ext4 target/ beats building there
bash scripts/wsl/fetch-rarlab.sh       # official rar 6.23 / unrar 7.23
cd ~/rar-rs
bash scripts/wsl/ci-linux.sh           # CI's lint + interop + binding steps
bash scripts/wsl/stage12-linux.sh      # legacy streaming vs official unrar
```

`ci-linux.sh` takes a step range: `1-13` the lint job (its step 12 is the
`cargo deny` dependency gate, step 13 the workspace tests), `14` official
interop, `15-19` the binding job, `20` the heavy fuzz smoke (CI runs that on
tags and the weekly schedule only). It follows the workflow step for step,
except that CI's log-only plumbing becomes plain output. When the rarlab tools
are present it exports `SA_OFFICIAL_*`, so the interop suites run instead of
skipping — including the binding test that otherwise reports
`SA_OFFICIAL_UNRAR is not set` (59 passed, 0 skipped, instead of 58 + 1).

## 本地官方工具与互操作套件

Windows 上不装 WinRAR 也能跑互操作：`.cache/winrar/`（被 `.gitignore` 忽略的本地
参考工具）下按版本放控制台工具（`5-91/`、`6-23/`、`7-23/`）。分工是有原因的：
**7.23 的 `Rar.exe` 既不创建（`-ma4`）也不修复 RAR4 归档**，所以 RAR4
写入/修复的 参考必须是 6.23；读取侧以 7.23 为准（`scripts/wsl/ci-linux.sh` 就是
`SA_OFFICIAL_RAR=rar623` + `SA_OFFICIAL_UNRAR=rar723`）。缓存不存在时用
`scripts/wsl/fetch-rarlab.sh` 拉，或装 WinRAR 后靠默认安装路径。

`SA_WINRAR_DIR` 选整套工具目录（`rar-cli` 的 `winrar_interop` 套件）；
`SA_OFFICIAL_RAR` / `SA_OFFICIAL_UNRAR` 直接指可执行文件（`rar-rs` 的
`official_interop` 套件）。从仓库根运行，路径要给 Windows 形式 （Git Bash
下：`cygpath -w "$PWD/.cache/winrar"`）：

```sh
SA_WINRAR_DIR='C:\path\to\rar-rs\.cache\winrar\6-23' \
  cargo test -p rar-cli --test winrar_interop

SA_OFFICIAL_RAR='C:\path\to\rar-rs\.cache\winrar\6-23\Rar.exe' \
SA_OFFICIAL_UNRAR='C:\path\to\rar-rs\.cache\winrar\7-23\UnRAR.exe' \
  cargo test -p rar-rs --test official_interop
```

工具缺失时套件打印 `SKIPPED` 并跳过；`SA_REQUIRE_WINRAR=1` /
`SA_REQUIRE_OFFICIAL=1` 把缺工具变成硬失败。**注意**：把 `SA_OFFICIAL_UNRAR`
指向 6.23 时，`v70::*`（RAR7 归档）与 `om_mark_of_the_web_matches_winrar` 会失败
——6.23 早于 RAR7、MOTW 行为也不同，是版本差异而非回归（已在 `1fbfaba` 上确认
同样失败）。

## Why test targets are optimized

The root `Cargo.toml` sets:

```toml
[profile.test]
opt-level = 2
```

The compression paths (match finding, PPMd, AES) are pure CPU work and ran an
order of magnitude slower unoptimized — the full suite took ~9 minutes and now
takes ~2. Only test targets are affected; `cargo build` keeps the debug profile.
`debug-assertions` and `overflow-checks` are still inherited from `dev`, so
arithmetic panics and `debug_assert!` fire exactly as before.

## Where the time goes

Measured on a 16-core host with `--all-features`, after the optimization above
(per-test times from `cargo nextest run`):

The numbers below are a snapshot measured on one host, not a contract. They are
here to tell you _which_ tests dominate, so you can filter — re-measure with
`cargo nextest run` instead of updating this table when they drift.

| Test                                                                        | Time  | What it does                                                                                         |
| --------------------------------------------------------------------------- | ----- | ---------------------------------------------------------------------------------------------------- |
| `codec::modern::lzss_huff::mt_tests::matchless_fast_path_is_byte_identical` | ~89 s | 7 corpora (~89 MiB) × 9 (level, dictionary, variant) combos × 2 (fast path on/off): ~1.6 GiB encoded |
| `...::mt_tests::cli_like_external_chunking_serial_chain`                    | ~20 s | three 14 MiB members, chunked the way `add_file` does                                                |
| `...::mt_tests::sequential_solid_chain_random_shared_blocks`                | ~18 s | three 13 MiB members against a 16 MiB dictionary                                                     |
| `rar50_roundtrip` (3 tests)                                                 | ~22 s | large-file batch, parallel extraction, per-archive thread counts                                     |
| everything else                                                             | ~40 s |                                                                                                      |

About 80% of the wall clock is three tests. They are **not** shrunk and **not**
marked `#[ignore]`, on purpose: each one guards a regression that already
shipped once — the persistent match-finder tree corrupting output across chunk
grows, the solid chain losing its shared window past the second member, and the
incompressible-data fast path diverging from the full pricing passes. The cost
is wall clock; the coverage is the point.

If you want a faster local loop, filter rather than change the tests:

```sh
cargo test --package rar-rs --lib -- archive::
cargo nextest run -E 'not test(/mt_tests::/)'
```

## Traps

- **`rarfiles_lst_lock()` is process-local.**
  `crates/rar-cli/tests/cli_behavior/` guards the tests that read `rarfiles.lst`
  with a `static OnceLock<Mutex<()>>` in its `support` module. That works under
  `cargo test` (one process per test binary) but not under `cargo nextest` (one
  process per test), where those tests can race and fail intermittently. It is a
  test-isolation artifact, not a product defect — but it is why CI does not use
  nextest.

- **Concurrency is not free.** Compression tests build Rayon pools sized from
  `available_parallelism()`, so running every test at once can oversubscribe the
  machine. On a 16-core host the library's own test binary went from 104 s at 16
  concurrent tests to 6.5 s at 8 when built without the `parallel` feature; with
  it enabled the difference is only ~8%. If a run looks pathologically slow, try
  `RUST_TEST_THREADS=8 cargo test …`. There is no committed default — CI hosts
  are small enough that the default is fine.
