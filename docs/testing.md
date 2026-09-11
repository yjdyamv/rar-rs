# Testing

How the suite is organized, what it costs, and the traps to know before
changing it.

## Running

```sh
cargo test --workspace --all-features     # everything
cargo test --package rar-rs --lib         # library unit tests only
cargo test --package rar-rs --lib -- archive::   # one module
```

`cargo nextest run --workspace --all-features` also works, is roughly twice as
fast (it parallelizes across test binaries instead of running them one after
another) and prints a per-test timing report, which is the easiest way to find
what got slow. It is a local convenience only: CI stays on plain
`cargo test`, and read the `rarfiles.lst` trap below before trusting it.

## Why test targets are optimized

The root `Cargo.toml` sets:

```toml
[profile.test]
opt-level = 2
```

The compression paths (match finding, PPMd, AES) are pure CPU work and ran an
order of magnitude slower unoptimized — the full suite took ~9 minutes and now
takes ~2. Only test targets are affected; `cargo build` keeps the debug
profile. `debug-assertions` and `overflow-checks` are still inherited from
`dev`, so arithmetic panics and `debug_assert!` fire exactly as before.

## Where the time goes

Measured on a 16-core host with `--all-features`, after the optimization above
(per-test times from `cargo nextest run`):

| Test | Time | What it does |
|---|---|---|
| `codec::modern::lzss_huff::mt_tests::matchless_fast_path_is_byte_identical` | ~109 s | 7 corpora (~89 MiB) × 9 (level, dictionary, variant) combos × 2 (fast path on/off): ~1.6 GiB encoded |
| `...::mt_tests::cli_like_external_chunking_serial_chain` | ~22 s | three 14 MiB members, chunked the way `add_file` does |
| `...::mt_tests::sequential_solid_chain_random_shared_blocks` | ~20 s | three 13 MiB members against a 16 MiB dictionary |
| `rar50_roundtrip` (3 tests) | ~22 s | large-file batch, parallel extraction, per-archive thread counts |
| everything else (≈585 tests) | ~40 s | |

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

- **`rarfiles_lst_lock()` is process-local.** `crates/rar-cli/tests/cli_behavior.rs`
  guards the tests that read `rarfiles.lst` with a `static OnceLock<Mutex<()>>`.
  That works under `cargo test` (one process per test binary) but not under
  `cargo nextest` (one process per test), where those tests can race and fail
  intermittently. It is a test-isolation artifact, not a product defect — but
  it is why CI does not use nextest.

- **Concurrency is not free.** Compression tests build Rayon pools sized from
  `available_parallelism()`, so running every test at once can oversubscribe
  the machine. On a 16-core host the library's own test binary went from 104 s
  at 16 concurrent tests to 6.5 s at 8 when built without the `parallel`
  feature; with it enabled the difference is only ~8%. If a run looks
  pathologically slow, try `RUST_TEST_THREADS=8 cargo test …`. There is no
  committed default — CI hosts are small enough that the default is fine.
