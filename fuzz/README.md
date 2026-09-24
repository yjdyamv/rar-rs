# rar-rs-fuzz

Seven targets covering the read and write attack surfaces of `rar-rs`:

- **`parse`** — RAR5/RAR7 block envelope, vints, headers, extra records, solid
  chains, encryption-header scan, bounded extraction.
- **`crypto`** — key derivation (strength capped), encryption-parameter parsing,
  AES-256-CBC round trips.
- **`recovery`** — inline `{RB}` chunk build/parse/repair, structured
  plan/geometry/shard mutations with CRC64-XZ recomputed, GF(2^16) parity +
  reconstruct, `.rev` serialization.
- **`rev`** — streaming `repair_archive_path`; REV5 sets mutated at the header
  (CRC32 recomputed) and rev3 sets built with the public API and mutated at the
  trailer, both through `rebuild_missing_volumes` / `collect_recovery_volumes`.
- **`legacy`** — RAR 1.4 / 2.0 / 3.0 / RAR4 block envelopes with mutated headers
  (16-bit header CRC or RAR13 rolling checksum recomputed) through the full read
  path and the legacy recovery scan.
- **`write`** — create from fuzzed options/members: single/multi-volume, solid,
  encrypted, header-encrypted, quick-open, BLAKE2sp, inline RR, `.rev`;
  round-trip byte checks plus rv/rc rebuild.
- **`rewrite`** — create, then delete/rename/append/comment/lock mutations with
  byte-for-byte survivor verification.

The target bytes steer the structured mutations directly, and the seeds are
genuine WinRAR output (`fixtures/rar50/winrar5_multiple_files.rar`, the
tail-match regression input, a `-rr5%` archive, and five small legacy fixtures),
so mutations reach deep parser paths that raw random bytes almost never touch.
`crates/rar/tests/support/structured.rs` (shared with the regression tests via
`#[path]`) derives valid-before-mutation records from them, with the CRC64-XZ /
CRC32 / 16-bit-header checksums recomputed — otherwise the shard arithmetic,
Reed-Solomon solve and `.rev` naming code would stop at the checksum gate.

Targets reach the wire-level surface through `rar_rs::wire` (the old `raw`
feature was retired in [ADR 0007](../docs/adr/0007-raw-feature-retired.md)).
GitHub CI only _compiles_ the fuzz workspace (`cargo check` / `cargo fmt`, with
and without the `fuzzing` feature); the bounded standalone smoke below runs in
the local Linux gate, [`scripts/wsl/ci-linux.sh`](../scripts/wsl/ci-linux.sh)
step 20/20 (5k iterations for parse/crypto/recovery/rev/legacy, 500 for
write/rewrite).

## Standalone (stable Rust, no extra toolchain)

Each target is a `fn(&[u8])` runner on a deterministic mutation loop. A panic
saves the crashing input to `fuzz/crashes/` and exits non-zero, so this doubles
as a quick local smoke (and as the `scripts/wsl/ci-linux.sh` fuzz step):

```sh
cargo run --release --bin parse      # 200k iterations
cargo run --release --bin crypto
cargo run --release --bin recovery
cargo run --release --bin rev        # 20k iterations (real file I/O each)
cargo run --release --bin legacy
cargo run --release --bin write
cargo run --release --bin rewrite

FUZZ_ITERATIONS=50000 cargo run --release --bin parse   # override count
FUZZ_SEED=0xC0FFEE cargo run --release --bin recovery   # override seed
```

The write-side targets create and rewrite archives on disk every iteration, so
they default to 20k iterations (Windows file churn makes them slow there; Linux
is ~10x faster) — raise it with `FUZZ_ITERATIONS`. `rev` and `legacy` also touch
disk per iteration and default to 20k for the same reason. `write` derives only
mutually compatible option combinations (quick-open is suppressed when a volume
size or header encryption is selected) and prints coverage counters after the
loop — created archives, multi-volume creations, rv/rc rebuilds — failing the
run when those paths were starved instead of passing with them dead.

## libFuzzer (nightly + clang, e.g. Linux CI)

```sh
cargo install cargo-fuzz
rustup toolchain install nightly
cd fuzz
cargo +nightly fuzz run <target> --features fuzzing   # each of the seven
```

The `fuzzing` feature pulls in `libfuzzer-sys`; the same `fn(&[u8])` runners are
reused, so both modes fuzz identical code. Nightly is required because
cargo-fuzz passes `-Z sanitizer` (ASAN/UBSAN), which stable cannot provide — and
the standalone loop only observes panics, so **run libFuzzer before relying on
parser robustness against hostile input**.

`fuzz/corpus/` is **gitignored**: `cargo fuzz run <target>` reads
`fuzz/corpus/<target>/` at startup and writes coverage-increasing mutations
back, so the directory is a libFuzzer work area, never committed. The standalone
loop never reads it — its seeds are `include_bytes!`-embedded in the harness.
Seed it from the vendored fixtures before a libFuzzer run:

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

`write` and `rewrite` have no fixture seeds (their archives are derived from the
fuzz input), so their corpus directories start empty.

## Notes

- The parse target bounds extraction (`max_unpacked_bytes`,
  `max_total_unpacked_bytes`, `max_dict_size`) so decompression bombs cannot
  exhaust memory; the parser's own 2 MiB header cap and 4 GiB dictionary ceiling
  bound the rest.
- The crypto target masks the KDF strength byte to ≤ 2^14 iterations so hostile
  input cannot turn the fuzzer itself into a CPU bomb. **Do not add encrypted
  archive fixtures to the parse corpus without a strength cap** — a mutated
  `-hp` fixture could otherwise trigger 2^24-iteration KDF runs.
- `fuzz/` is deliberately **not** a workspace member: CI's
  `cargo test --workspace --all-features` must not enable the `fuzzing` feature,
  which would drag in the C++ libFuzzer runtime.
