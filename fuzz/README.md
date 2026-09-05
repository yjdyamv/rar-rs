# rar-rs-fuzz

Fuzz targets for the `rar-rs` library, covering the three attack surfaces:

| target | surface |
|---|---|
| `parse` | RAR5/RAR7 block envelope, vints, headers, extra records, solid chains, encryption-header scan, extraction (bounded) |
| `crypto` | key derivation (bounded strength), encryption-parameter parsing, AES-256-CBC round trips |
| `recovery` | inline `{RB}` chunk build/parse/repair, GF(2^16) parity + reconstruct, CRC64-XZ, `.rev` serialization |
| `write` | create from fuzzed options/members: single/multi-volume, solid, encrypted, header-encrypted, quick-open, BLAKE2sp, inline RR, `.rev`; round-trip byte checks + rv/rc rebuild |
| `rewrite` | create then delete/rename/append/comment/lock mutations with byte-for-byte survivor verification |

Seed corpus embeds genuine WinRAR output
(`crates/rar/tests/fixtures/rar50/winrar5_multiple_files.rar`) and the
tail-match regression input, so mutations reach deep parser paths that
raw random bytes almost never touch.

## Standalone (stable Rust, no extra toolchain)

Each target is a `fn(&[u8])` runner driven by a deterministic mutation
loop. A panic saves the crashing input to `fuzz/crashes/` and exits
non-zero — usable both as a quick local smoke and as CI:

```sh
cargo run --release --bin parse      # 200k iterations
cargo run --release --bin crypto
cargo run --release --bin recovery
cargo run --release --bin write      # 20k iterations (real file I/O each)
cargo run --release --bin rewrite    # 20k iterations (real file I/O each)

FUZZ_ITERATIONS=50000 cargo run --release --bin parse   # override count
FUZZ_SEED=0xC0FFEE cargo run --release --bin recovery   # override seed
```

The write-side targets (`write`, `rewrite`) create and rewrite archives
on disk every iteration, so they default to 20k iterations (Windows file
churn makes them slow there; Linux is ~10x faster) — raise the count
with `FUZZ_ITERATIONS` for longer runs.

## libFuzzer (nightly + clang, e.g. Linux CI)

```sh
cargo install cargo-fuzz
rustup toolchain install nightly
cd fuzz
cargo +nightly fuzz run parse --features fuzzing
cargo +nightly fuzz run crypto --features fuzzing
cargo +nightly fuzz run recovery --features fuzzing
```

The `fuzzing` feature pulls in `libfuzzer-sys`; the same `fn(&[u8])`
runners are reused so both modes fuzz identical code. `fuzz/corpus/` is
**gitignored** (libFuzzer grows it with new inputs); seed it from the
vendored fixtures before the first run:

```sh
mkdir -p fuzz/corpus/parse
cp crates/rar/tests/fixtures/rar50/winrar5_multiple_files.rar fuzz/corpus/parse/
cp crates/rar/tests/fixtures/rar50/tail-match-362.bin fuzz/corpus/parse/
```

Nightly is required here — cargo-fuzz passes `-Z sanitizer` (ASAN/UBSAN)
which stable cannot provide; the standalone loop only observes panics,
so run libFuzzer before relying on parser robustness against hostile
input.

## Notes

- The parse target bounds extraction (`max_unpacked_bytes`,
  `max_total_unpacked_bytes`, `max_dict_size`) so decompression bombs
  can't exhaust memory; the parser's own 2 MiB header cap and 4 GiB
  dictionary ceiling bound the rest.
- The crypto target masks the KDF strength byte to ≤ 2^14 iterations so
  hostile inputs can't turn the fuzzer itself into a CPU bomb. Do not
  add encrypted archive fixtures to the parse corpus without a
  strength cap — a mutated `-hp` fixture could otherwise trigger
  2^24-iteration KDF runs.
- `fuzz/` is deliberately **not** a workspace member: `cargo test
  --workspace --all-features` (CI) must not enable the `fuzzing`
  feature, which would drag in the C++ libFuzzer runtime.
