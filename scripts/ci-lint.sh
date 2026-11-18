#!/usr/bin/env bash
# Run the CI `lint` job locally, so a push does not carry a failure CI would
# catch. This is a *mirror*, not a second source of truth — the workflow is
# authoritative (`.github/workflows/CI.yml`, job `lint`); update both together.
#
# It exists because the documented local gate (`cargo test --workspace
# --all-features`, docs/testing.md) does not run the static checks — and a
# `#[cfg(not(feature = "parallel"))]`-only dead code slipped past it once,
# failing the "clippy (rar-rs, every feature combination)" step.
#
# Covered: fmt (workspace + fuzz), the host-path separator guard, workspace
# version agreement, the wasm-target `cargo check`, clippy (workspace, then
# every rar-rs feature combination), the fuzz `cargo check`s, rustdoc, the
# dependency gate (cargo deny) and the hermetic JS path-mapping test — plus the
# same test targets the job runs.
#
# Not covered: the Windows-only `lint-windows` job (run it on Windows), the
# binding build / `tsc` matrix and the official-tool interop suites.
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
      sed -n '2,30p' "$0"
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

step "cargo fmt (workspace)"
run cargo fmt --all --check

step "cargo fmt (fuzz)"
run cargo fmt --manifest-path fuzz/Cargo.toml --check

step "host-path separator guard"
# Host paths must test separators through `std::path::is_separator`, which is
# platform-correct; a literal backslash check is a Windows-only assumption.
patterns=$(mktemp)
printf "ends_with('%s')\nstarts_with('%s')\n" '\\' '\\' > "$patterns"
if grep -rnFf "$patterns" --include='*.rs' crates/rar-cli/src crates/rar/src; then
  rm -f "$patterns"
  echo "!!! host paths must use std::path::is_separator, not a literal backslash check" >&2
  exit 1
fi
rm -f "$patterns"

step "workspace versions agree"
version() {
  sed -n '/^\[package\]/,/^\[/{s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p}' "$1" | head -1
}
lib=$(version crates/rar/Cargo.toml)
cli=$(version crates/rar-cli/Cargo.toml)
napi=$(version crates/rar-napi/Cargo.toml)
pkg=$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' \
  crates/rar-napi/package.json | head -1)
dep=$(sed -n 's/^rar-rs = { version = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)
printf '  rar-rs=%s rar-cli=%s rar-rs-napi=%s package.json=%s workspace-dep=%s\n' \
  "$lib" "$cli" "$napi" "$pkg" "$dep"
if [ -z "$lib" ] || [ "$lib" != "$cli" ] || [ "$lib" != "$napi" ] \
  || [ "$lib" != "$pkg" ] || [ "$lib" != "$dep" ]; then
  echo "!! library, CLI, binding, package.json and workspace-dependency versions must match" >&2
  exit 1
fi

step "cargo check (rar-rs, wasm target, -D warnings)"
RUSTFLAGS="-D warnings" run cargo check -p rar-rs --all-features \
  --target wasm32-wasip1-threads --locked

step "clippy (workspace, all features and targets, -D warnings)"
run cargo clippy --workspace --all-features --all-targets --locked -- -D warnings

step "clippy (rar-rs, every feature combination, -D warnings)"
# The workspace build unifies features across crates, which hides targets that
# use a gated API without declaring `required-features` for it.
for features in "" parallel simd parallel,simd; do
  echo "--- features='$features'"
  run cargo clippy -p rar-rs --no-default-features --features "$features" \
    --all-targets --locked -- -D warnings
done

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
  run cargo test -p rar-rs --locked \
    --test rar4_read --test rar13_read --test rar4_rev3 \
    --test rar4_edge_cases --test rar_vm_filters \
    --test streaming_edge_cases --test structured_recovery_mutations \
    --test archive_editor --test winrar_generation --test rar50_roundtrip
fi

step "done"
