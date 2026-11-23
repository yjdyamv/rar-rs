#!/usr/bin/env bash
# Run the CI static gate locally, so a push does not carry a failure CI would
# catch. The workflow is authoritative (`.github/workflows/CI.yml`, jobs `check`
# and `test`); the three share the checks that used to be hand-copied, so only
# the orchestration below can drift:
#
#   scripts/ci/run-actionlint.sh           workflow-file lint (opt-in locally)
#                                          plus shellcheck over the shell scripts
#   scripts/ci/check-host-paths.sh          host-path separator guard
#   scripts/ci/check-crate-licenses.sh      the legal texts each crate ships
#   scripts/ci/check-versions.sh            the five version-bearing places
#   scripts/ci/clippy-feature-matrix.sh     rar-rs, the feature combinations the
#                                          workspace build cannot cover
#   scripts/ci/test-release-scripts.sh     the release asset rules + notes
#                                          grouping, against fixtures
#   scripts/ci/test-generated-corpus.sh     the generated legacy-corpus suites
#
# It exists because the documented local gate (`cargo test --workspace
# --all-features`, docs/testing.md) does not run the static checks — and a
# `#[cfg(not(feature = "parallel"))]`-only dead code slipped past it once,
# failing the clippy feature-matrix step.
#
# Covered: actionlint over the workflow files, shellcheck over them and the
# shell scripts, the release payload's fixture test, fmt (workspace + fuzz), the
# three guards below, the wasm-target `cargo check`, clippy (workspace, then the
# rar-rs feature combinations), the fuzz `cargo check`s, rustdoc, the dependency
# gate (cargo deny) and the hermetic JS path-mapping test — plus the same test
# targets `test` runs.
#
# Not covered: the Windows-only `check-windows`/`test-windows` jobs (run them on Windows), the
# 32-bit `test-i686` job (needs an `i686-unknown-linux-gnu` toolchain plus
# `gcc-multilib`), the binding build / `tsc` matrix and the official-tool interop
# suites. Markdown formatting is the one optional extra, under `dprint` below.
# `scripts/wsl/ci-linux.sh` is the full Linux gate under WSL2; this script is
# the portable static gate that also runs on Windows Git Bash.
#
# Usage: scripts/ci-lint.sh [--no-tests]
#   --no-tests   run only the static checks (fast; skips the Rust test suites)
#
# Expects: cargo with the rustfmt + clippy components and the
# `wasm32-wasip1-threads` target (`rustup component add clippy rustfmt`;
# `rustup target add wasm32-wasip1-threads`). `node` and `cargo-deny` are
# optional — their steps skip with a notice when absent.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

with_tests=1
for arg in "$@"; do
  case "$arg" in
    --no-tests) with_tests=0 ;;
    -h | --help)
      sed -n '2,35p' "$0"
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

step() { printf '\n===== %s =====\n' "$*"; }
run() {
  if "$@"; then return 0; fi
  echo "!!! FAILED: $*" >&2
  exit 1
}

step "workflow files (actionlint)"
# The one check `check` runs first and this script used to skip. It is opt-in
# here because the pinned download in scripts/ci/run-actionlint.sh is a Linux
# tarball: with `actionlint` on PATH (or ACTIONLINT_BIN set) it runs, otherwise
# it says so and moves on — the checks that follow do not depend on it.
if [ -n "${ACTIONLINT_BIN:-}" ] || command -v actionlint >/dev/null 2>&1; then
  if [ -z "${ACTIONLINT_BIN:-}" ]; then ACTIONLINT_BIN=$(command -v actionlint); fi
  export ACTIONLINT_BIN
  run bash scripts/ci/run-actionlint.sh
else
  echo "SKIP: actionlint not on PATH (set ACTIONLINT_BIN, or let CI run it)"
fi

step "shell scripts (shellcheck)"
# The scripts the workflows call. `-S warning` matches the CI step: at the
# default style level shellcheck also flags deliberate constructs.
if command -v shellcheck >/dev/null 2>&1; then
  run shellcheck -S warning scripts/*.sh scripts/ci/*.sh scripts/wsl/*.sh
else
  echo "SKIP: shellcheck not installed"
fi

step "release payload (fixtures)"
# The same fixture test `check` runs: the release asset rules and the notes
# grouping, against temp directories and a scratch git repo. Seconds, no
# toolchain — and it is the only local way to exercise release-only logic
# without cutting a tag.
run bash scripts/ci/test-release-scripts.sh

step "markdown (dprint, optional)"
# No CI job runs this. It is a local habit, not a gate: `dprint` needs the npm
# package and `plugins.dprint.dev` at check time, and its markdown plugin wraps
# on whitespace, so Chinese prose (no spaces) yields nothing to wrap and English
# prose reflows on every edit. Run it when you want a diff to stay narrow.
if command -v dprint >/dev/null 2>&1; then
  run dprint check
else
  echo "SKIP: dprint not on PATH (npx dprint@0.50.2 check)"
fi

step "cargo fmt (workspace)"
run cargo fmt --all --check

step "cargo fmt (fuzz)"
run cargo fmt --manifest-path fuzz/Cargo.toml --check

step "host-path separator guard"
run bash scripts/ci/check-host-paths.sh

step "workspace versions agree"
run bash scripts/ci/check-versions.sh

step "crate license texts match the root copies"
run bash scripts/ci/check-crate-licenses.sh

step "cargo check (rar-rs, wasm target, -D warnings)"
RUSTFLAGS="-D warnings" run cargo check -p rar-rs --all-features \
  --target wasm32-wasip1-threads --locked

step "clippy (workspace, all features and targets, -D warnings)"
run cargo clippy --workspace --all-features --all-targets --locked -- -D warnings

step "clippy (rar-rs, the combinations the workspace build misses, -D warnings)"
run bash scripts/ci/clippy-feature-matrix.sh

step "cargo check (fuzz workspace)"
run cargo check --manifest-path fuzz/Cargo.toml --all-targets --locked

step "cargo check (fuzz workspace, libFuzzer feature)"
RUSTFLAGS="--cfg fuzzing" run cargo check --manifest-path fuzz/Cargo.toml \
  --all-targets --features fuzzing --locked

step "rustdoc (-D warnings)"
RUSTDOCFLAGS="-D warnings" run cargo doc --workspace --no-deps --locked

step "dependency gate (cargo deny)"
if cargo deny --version >/dev/null 2>&1; then
  run cargo deny check --all-features --locked
else
  echo "  SKIP: cargo-deny not installed (cargo install cargo-deny --locked)"
fi

step "binding path-mapping test (node --test)"
if command -v node >/dev/null 2>&1; then
  run node --test crates/rar-napi/__test__/wasi-path-map.test.mjs
else
  echo "  SKIP: node not on PATH"
fi

if [ "$with_tests" -eq 1 ]; then
  step "cargo test (rar-rs, all features)"
  run cargo test -p rar-rs --all-features --locked

  step "cargo test (rar-cli bins + hermetic CLI suites)"
  run cargo test -p rar-cli --locked --bins --test cli_behavior --test volume_bare_name

  step "cargo test (rar-rs-napi lib)"
  run cargo test -p rar-rs-napi --lib --locked

  step "cargo test (generated legacy corpus; skips without the official tools)"
  run bash scripts/ci/test-generated-corpus.sh
fi

step "done"
