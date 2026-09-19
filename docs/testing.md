# Testing

> Last verified: 2026-09-19 @ `c2db547`; every timing number here is a
> host-specific snapshot, not a contract.

How the suite is organized, what it costs, and the traps to know before changing
it.

## Running

```sh
cargo test --workspace --all-features       # everything
cargo test --package rar-rs --lib           # library unit tests only
cargo test --package rar-rs --lib -- archive::   # one module
```

`cargo nextest run --workspace --all-features` also works, is roughly twice as
fast (it parallelizes across test binaries instead of running them one after
another) and prints a per-test timing report — the easiest way to find what got
slow. It is a local convenience only: CI stays on plain `cargo test`, and read
the `rarfiles_lst_lock` trap below before trusting it.

## The other two platforms

`lint` runs on Linux, and a green Windows run says nothing about it. Both extra
surfaces are check-only (`clippy`/`check` do not link, so no cross toolchain is
needed):

```sh
rustup target add x86_64-unknown-linux-gnu    # once
cargo clippy --workspace --all-features --all-targets --locked \
  --target x86_64-unknown-linux-gnu -- -D warnings

rustup target add wasm32-wasip1-threads       # once
RUSTFLAGS="-D warnings" cargo check -p rar-rs --all-features \
  --target wasm32-wasip1-threads --locked
```

- **Linux**: a `let mut` only pushed under `cfg(windows)` trips `unused_mut`
  under `-D warnings`, and tests asserting a Windows-only path hazard fail when
  POSIX accepts the name. The cfg-gated branches can only _run_ on Linux, so
  assert what POSIX actually does (see `extract_rejects_unsafe_entry_names`)
  rather than assuming a Windows-only hazard.
- **wasm32-wasip1-threads** is the quietest: it is neither unix nor windows, so
  helpers those branches use are dead there. That is why the library gates such
  helpers on `any(unix, windows)`.
- The binding crate cannot be checked either way (`napi-build` needs the
  `EMNAPI_LINK_DIR` that `napi build` injects); build it with
  `npx napi build --platform --release --target wasm32-wasip1-threads`.

## Dependency gate

`lint` also runs `cargo deny check --all-features --locked`, which judges the
_dependency graph_ rather than this tree: RustSec advisories, the license
allow-list, the registry sources. Configuration and the rationale for each
allowed license family live in the root [`deny.toml`](../deny.toml); a
dependency under an unlisted license fails on purpose. In-tree `rars` ports are
_not_ covered by that file — see
[`THIRD_PARTY_LICENSES.md`](../THIRD_PARTY_LICENSES.md).

```sh
cargo install cargo-deny --locked    # once
cargo deny check --all-features --locked
```

## Running the Linux half under WSL2

WSL2 runs the job for real, including the official-tool interop suites.
`scripts/wsl/` sets the box up and drives it:

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

`ci-linux.sh [FROM] [TO]` mirrors the workflow step for step (default `1 19`),
with CI's log-only plumbing as plain output: `1-13` the lint job (step 12 is the
`cargo deny` gate, 13 the workspace tests), `14` official interop, `15-19` the
binding job, `20` the heavy fuzz smoke (tags/schedule only). With the rarlab
tools present it exports `SA_OFFICIAL_*`, so the interop suites run instead of
skipping — including the binding test that otherwise reports
`SA_OFFICIAL_UNRAR is not set` (59 passed, 0 skipped, instead of 58 + 1).

Mirrors were picked from measurements on the author's box; re-measure before
trusting them elsewhere. (2026-09-17: 8 MiB ranges of the real `rustc` tarball,
a real crate download, a real `Packages.gz`.)

| mirror  | toolchain | crate     | ubuntu    |
| ------- | --------- | --------- | --------- |
| aliyun  | 2.41 MB/s | 2.08 MB/s | 2.78 MB/s |
| tencent | 2.50 MB/s | —         | 2.69 MB/s |
| nju     | 2.31 MB/s | 403       | 1.70 MB/s |
| ustc    | 0.89 MB/s | 0.13 MB/s | 0.87 MB/s |
| tuna    | 0.20 MB/s | 0.05 MB/s | 0.36 MB/s |
| sustech | —         | —         | 1.96 MB/s |

The scripts therefore use apt and crates.io from Aliyun, the toolchain from
Tencent, `rustup-init` from USTC, Node 24 from Aliyun (the only mirror carrying
it) and npm from `registry.npmmirror.com`. Why the others lose: Aliyun's rustup
manifest is months stale and rewrites component URLs to itself (so
`rustup target add wasm32-wasip1-threads` 404s there) and it has no
`rustup-init`; NJU refuses crate downloads (403); TUNA/USTC/Tencent/NJU keep the
manifests pointing at `static.rust-lang.org` (so they honour
`RUSTUP_DIST_SERVER`); SUSTech has no Rust mirror.

## Official tools and the interop suites

Interop runs on Windows without installing WinRAR: `.cache/winrar/` (gitignored)
holds console tools by version (`5-91/`, `6-23/`, `7-23/`). The split is not
arbitrary — **7.23's `Rar.exe` neither creates `-ma4` nor repairs RAR4**, so
RAR4 write/repair must be validated against **6.23**, while the read side is
validated against **7.23** (`scripts/wsl/ci-linux.sh` uses exactly
`SA_OFFICIAL_RAR=rar623` + `SA_OFFICIAL_UNRAR=rar723`). Fetch the cache with
`scripts/wsl/fetch-rarlab.sh`, or rely on WinRAR's default install path.

- `SA_WINRAR_DIR` — whole tool directory, for `rar-cli`'s `winrar_interop`.
- `SA_OFFICIAL_RAR` / `SA_OFFICIAL_UNRAR` — explicit executables, for `rar-rs`'s
  `official_interop`.

Run from the repository root and pass **Windows** paths (in Git Bash:
`cygpath -w "$PWD/.cache/winrar"`):

```sh
SA_WINRAR_DIR='C:\path\to\rar-rs\.cache\winrar\6-23' \
  cargo test -p rar-cli --test winrar_interop

SA_OFFICIAL_RAR='C:\path\to\rar-rs\.cache\winrar\6-23\Rar.exe' \
SA_OFFICIAL_UNRAR='C:\path\to\rar-rs\.cache\winrar\7-23\UnRAR.exe' \
  cargo test -p rar-rs --test official_interop
```

A missing tool prints `SKIPPED` and the suite passes; `SA_REQUIRE_WINRAR=1` /
`SA_REQUIRE_OFFICIAL=1` turn that into a hard failure. **Pointing
`SA_OFFICIAL_UNRAR` at 6.23 fails `v70::*` and
`om_mark_of_the_web_matches_winrar`** — 6.23 predates RAR7 and its MOTW differs.
That is a version difference, not a regression (confirmed on `1fbfaba`).

## Why test targets are optimized

```toml
[profile.test]
opt-level = 2
```

The compression paths (match finding, PPMd, AES) are pure CPU work and ran an
order of magnitude slower unoptimized: the full suite took ~9 minutes, now ~2.
Only test targets are affected — `cargo build` keeps the debug profile — and
`debug-assertions`/`overflow-checks` are still inherited from `dev`, so
arithmetic panics and `debug_assert!` fire exactly as before.

## Where the time goes

Measured with `cargo nextest run` on a 16-core host, `--all-features`.
Re-measure rather than updating this table; it exists to tell you _which_ tests
dominate so you can filter them.

| Test                                                                        | Time  | What it does                                                                                         |
| --------------------------------------------------------------------------- | ----- | ---------------------------------------------------------------------------------------------------- |
| `codec::modern::lzss_huff::mt_tests::matchless_fast_path_is_byte_identical` | ~89 s | 7 corpora (~89 MiB) × 9 (level, dictionary, variant) combos × 2 (fast path on/off): ~1.6 GiB encoded |
| `...::mt_tests::cli_like_external_chunking_serial_chain`                    | ~20 s | three 14 MiB members, chunked the way `add_file` does                                                |
| `...::mt_tests::sequential_solid_chain_random_shared_blocks`                | ~18 s | three 13 MiB members against a 16 MiB dictionary                                                     |
| `rar50_roundtrip` (3 tests)                                                 | ~22 s | large-file batch, parallel extraction, per-archive thread counts                                     |
| everything else                                                             | ~40 s |                                                                                                      |

Three tests are ~80% of the wall clock. They are **not** shrunk and **not**
`#[ignore]`d on purpose: each guards a regression that already shipped once —
the persistent match-finder tree corrupting output across chunk grows, the solid
chain losing its shared window past the second member, and the
incompressible-data fast path diverging from the full pricing passes. The cost
is wall clock; the coverage is the point.

For a faster local loop, filter rather than change the tests:

```sh
cargo test --package rar-rs --lib -- archive::
cargo nextest run -E 'not test(/mt_tests::/)'
```

## Traps

- **`rarfiles_lst_lock()` is process-local.** `cli_behavior` guards the tests
  that read `rarfiles.lst` with a `static OnceLock<Mutex<()>>`. That works under
  `cargo test` (one process per test binary) but not under `cargo nextest` (one
  process per test), where those tests can race. It is a test-isolation
  artifact, not a product defect — and it is why CI does not use nextest.
- **Concurrency is not free.** Compression tests build Rayon pools sized from
  `available_parallelism()`, so running every test at once can oversubscribe the
  machine: on a 16-core host the library test binary went from 104 s at 16
  concurrent tests to 6.5 s at 8 built without `parallel`; with `parallel` the
  difference is only ~8%. If a run looks pathologically slow, try
  `RUST_TEST_THREADS=8 cargo test …`. There is no committed default — CI hosts
  are small enough.
