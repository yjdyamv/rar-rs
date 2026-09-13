# rar-rs-fuzz

Fuzz targets for the `rar-rs` library, covering the three attack surfaces:

| target | surface |
|---|---|
| `parse` | RAR5/RAR7 block envelope, vints, headers, extra records, solid chains, encryption-header scan, extraction (bounded) |
| `crypto` | key derivation (bounded strength), encryption-parameter parsing, AES-256-CBC round trips |
| `recovery` | inline `{RB}` chunk build/parse/repair, structured plan/geometry/shard mutations with the CRC64-XZ recomputed, GF(2^16) parity + reconstruct, CRC64-XZ, `.rev` serialization |
| `rev` | streaming `repair_archive_path`, fabricated REV5 sets mutated at the header (CRC32 recomputed) and rev3 sets built with the public API and mutated at the trailer — both driven through `rebuild_missing_volumes` / `collect_recovery_volumes` |
| `legacy` | RAR 1.4 / RAR 2.0 / RAR 3.0 / RAR4 block envelopes with mutated headers (16-bit header CRC / RAR13 rolling member checksum recomputed) through the full read path and the legacy recovery-record scan |
| `write` | create from fuzzed options/members: single/multi-volume, solid, encrypted, header-encrypted, quick-open, BLAKE2sp, inline RR, `.rev`; round-trip byte checks + rv/rc rebuild |
| `rewrite` | create then delete/rename/append/comment/lock mutations with byte-for-byte survivor verification |

Seed corpus embeds genuine WinRAR output
(`crates/rar/tests/fixtures/rar50/winrar5_multiple_files.rar`) and the
tail-match regression input, so mutations reach deep parser paths that
raw random bytes almost never touch. The recovery targets additionally
embed a genuine WinRAR `-rr5%` archive
(`fixtures/rar50/winrar5_with_recovery_rr5.rar`) and five small legacy
fixtures (RAR 1.4, RAR 2.0, RAR 3.0, WinRAR 5.91 STORE, RAR 2.5
`PROTECT_HEAD`), and `crates/rar/tests/support/structured.rs` (shared with
the regression test via `#[path]`) derives valid-before-mutation
records from them: plan/geometry fields and shard states are edited with
the CRC64-XZ / CRC32 / 16-bit-header checksums recomputed, so the shard
arithmetic, Reed-Solomon solve and `.rev` naming/layout code are actually
reached instead of being stopped at the checksum gate.

> The standalone harness embeds its seeds (`include_bytes!`), so the
> gitignored `fuzz/corpus/` directory is only used by libFuzzer. Seed it
> from the same fixtures before a libFuzzer run (see below).
>
> The structured mutator deliberately keeps one parser-valid geometry out
> of the default loop: a reversed shard range
> (`(data_shards-1) * group_count > prefix_len`) currently panics in
> `repair_inline_recovery_prefix` (found by this target, 2026-09; the
> library fix is out of scope for the fuzzing change).
> `crates/rar/tests/structured_recovery_mutations.rs` carries the
> deterministic repro as an `#[ignore]`d test.

> The targets import the wire-level surface through `rar_rs::wire` (the
> former `raw` feature was retired in ADR 0007); CI runs a bounded standalone
> smoke (5k iterations for parse/crypto/recovery, 500 for write/rewrite) in
> addition to the check.

## Standalone (stable Rust, no extra toolchain)

Each target is a `fn(&[u8])` runner driven by a deterministic mutation
loop. A panic saves the crashing input to `fuzz/crashes/` and exits
non-zero — usable both as a quick local smoke and as CI:

```sh
cargo run --release --bin parse      # 200k iterations
cargo run --release --bin crypto
cargo run --release --bin recovery
cargo run --release --bin rev        # 20k iterations (real file I/O each)
cargo run --release --bin legacy     # 20k iterations (real file I/O each)
cargo run --release --bin write      # 20k iterations (real file I/O each)
cargo run --release --bin rewrite    # 20k iterations (real file I/O each)

FUZZ_ITERATIONS=50000 cargo run --release --bin parse   # override count
FUZZ_SEED=0xC0FFEE cargo run --release --bin recovery   # override seed
```

The write-side targets (`write`, `rewrite`) create and rewrite archives
on disk every iteration, so they default to 20k iterations (Windows file
churn makes them slow there; Linux is ~10x faster) — raise the count
with `FUZZ_ITERATIONS` for longer runs. `rev` and `legacy` also touch
disk per iteration and default to 20k for the same reason.

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
mkdir -p fuzz/corpus/parse fuzz/corpus/rev fuzz/corpus/legacy
cp crates/rar/tests/fixtures/rar50/winrar5_multiple_files.rar fuzz/corpus/parse/
cp crates/rar/tests/fixtures/rar50/tail-match-362.bin fuzz/corpus/parse/
cp crates/rar/tests/fixtures/rar50/winrar5_with_recovery_rr5.rar fuzz/corpus/recovery/
cp crates/rar/tests/fixtures/rar40/rev3/rev_newstyle.part1.rar \
   crates/rar/tests/fixtures/rar40/rev3/rev_newstyle.part2.rar \
   crates/rar/tests/fixtures/rar40/rev3/rev_newstyle.part1.rev fuzz/corpus/rev/
cp crates/rar/tests/fixtures/rar13/MULTIFIL.RAR \
   crates/rar/tests/fixtures/rar40/rar2/rar20.rar \
   crates/rar/tests/fixtures/rar40/winrar591_store_m0.rar \
   crates/rar/tests/fixtures/rar40/rar300/compressed_text_rar300.rar \
   crates/rar/tests/fixtures/rar40/repair/rar250_protect_head_rr1.rar fuzz/corpus/legacy/
```

The corpus layout found on disk at the time of writing is
`fuzz/corpus/{parse,crypto,recovery}` with `parse/winrar5.rar`,
`parse/tail-match.bin`, `crypto/sig.bin` and `recovery/sig.bin`; the
`rev`/`legacy` directories above are additions for this target pair (the
standalone loop does not read them — it embeds the same bytes).

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
