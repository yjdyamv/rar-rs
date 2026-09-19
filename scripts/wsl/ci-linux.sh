#!/usr/bin/env bash
# Reproduce .github/workflows/CI.yml's Linux jobs locally (WSL2):
#
#     1-13  lint job    fmt (workspace + fuzz), host-path guard, cargo check
#                       (workspace / no-default / wasm -D warnings / fuzz /
#                       fuzz+fuzzing), clippy -D warnings (workspace, then
#                       every rar-rs feature combination), the dependency
#                       gate (cargo deny: advisories/licenses/sources), the
#                       full workspace test suite (stale `rarfiles.lst`
#                       removed first, failing test names echoed the way CI
#                       annotates them), rustdoc -D warnings
#       14  interop job official rar 6.23 + unrar 7.23 suites, driven with
#                       SA_REQUIRE_OFFICIAL=1 so a missing tool fails loudly
#  15-19  binding job   npm ci, native addon build + test, wasm addon build,
#                       WASI loader patch + WASI-forced test
#       20  heavy only  fuzz smoke loop (CI runs it on tags/schedule only)
#
# Not covered: the cross-target binding matrix (needs zig/cargo-zigbuild), the
# Windows-only CLI job (run it on Windows), and CI's log-only plumbing.
#
# Usage: scripts/wsl/ci-linux.sh [FROM] [TO]      (defaults: 1 19)
# Prereqs: scripts/wsl/setup-{apt,rust,node}.sh, a repo clone (REPO=~ by
#          default at ~/rar-rs), scripts/wsl/fetch-rarlab.sh for step 14, and
#          cargo-deny (`cargo install cargo-deny --locked`) for step 12.
#          Mirror rationale: docs/testing.md, "Running the Linux half under
#          WSL2".
set -uo pipefail

REPO=${REPO:-$HOME/rar-rs}
RARLAB=${RARLAB:-$HOME/rarlab}
FROM=${1:-1}
TO=${2:-19}

cd "$REPO" 2>/dev/null || {
  echo "no repo at $REPO (set REPO=...); clone one first, e.g." >&2
  echo "  git clone /mnt/c/Users/yuan/Desktop/rar-rs ~/rar-rs" >&2
  exit 1
}

# A plain `bash script.sh` is not a login shell, so pick the toolchains up.
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="${NODE_BIN:-$HOME/.local/node/bin}:$PATH"

# The interop suites and the binding's official-unrar check read these; with
# them set the JS suite runs 59/59 instead of skipping one test.
if [ -x "$RARLAB/rar723/unrar" ]; then
  export SA_OFFICIAL_UNRAR=${SA_OFFICIAL_UNRAR:-$RARLAB/rar723/unrar}
  export SA_OFFICIAL_RAR=${SA_OFFICIAL_RAR:-$RARLAB/rar623/rar}
  export SA_OFFICIAL_SFX=${SA_OFFICIAL_SFX:-$RARLAB/rar623/default.sfx}
fi

step() { printf '\n========== %s ==========\n' "$*"; }
run() {
  if "$@"; then return 0; fi
  echo "!!! FAILED: $*" >&2
  exit 1
}
want() { [ "$1" -ge "$FROM" ] && [ "$1" -le "$TO" ]; }

if want 1; then
  step "1/20 cargo fmt (workspace)"
  run cargo fmt --all --check
fi

if want 2; then
  step "2/20 static guards (host paths, workspace versions)"
  printf "ends_with('%s')\nstarts_with('%s')\n" '\\' '\\' > /tmp/host-sep-patterns
  if grep -rnFf /tmp/host-sep-patterns --include='*.rs' crates/rar-cli/src crates/rar/src; then
    echo "!!! host paths must use std::path::is_separator, not a literal backslash" >&2
    exit 1
  fi
  # Mirrors the workflow's "Workspace versions agree": the release tag is only
  # compared against the binding, so the library and CLI drifted once.
  version() {
    sed -n '/^\[package\]/,/^\[/{s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p}' "$1" | head -1
  }
  lib=$(version crates/rar/Cargo.toml)
  cli=$(version crates/rar-cli/Cargo.toml)
  napi=$(version crates/rar-napi/Cargo.toml)
  pkg=$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' \
    crates/rar-napi/package.json | head -1)
  printf '  rar-rs=%s rar-cli=%s rar-rs-napi=%s package.json=%s\n' \
    "$lib" "$cli" "$napi" "$pkg"
  if [ -z "$lib" ] || [ "$lib" != "$cli" ] || [ "$lib" != "$napi" ] \
    || [ "$lib" != "$pkg" ]; then
    echo "!!! library, CLI, binding and package.json versions must match" >&2
    exit 1
  fi
fi

if want 3; then
  step "3/20 cargo check (workspace, all targets)"
  run cargo check --workspace --all-targets --locked
fi

if want 4; then
  step "4/20 cargo check (rar-rs, no default features)"
  run cargo check -p rar-rs --no-default-features --locked
fi

if want 5; then
  step "5/20 cargo check (rar-rs, wasm target, -D warnings)"
  RUSTFLAGS="-D warnings" run cargo check -p rar-rs --all-features \
    --target wasm32-wasip1-threads --locked
fi

if want 6; then
  step "6/20 cargo check (fuzz workspace)"
  run cargo check --manifest-path fuzz/Cargo.toml --all-targets --locked
fi

if want 7; then
  step "7/20 cargo fmt (fuzz workspace)"
  run cargo fmt --manifest-path fuzz/Cargo.toml --check
fi

if want 8; then
  step "8/20 cargo check (fuzz workspace, libFuzzer feature)"
  RUSTFLAGS=--cfg\ fuzzing run cargo check --manifest-path fuzz/Cargo.toml \
    --all-targets --features fuzzing --locked
fi

if want 9; then
  step "9/20 clippy (workspace, all features and targets, -D warnings)"
  run cargo clippy --workspace --all-features --all-targets --locked -- -D warnings
fi

if want 10; then
  step "10/20 clippy (rar-rs, every feature combination, -D warnings)"
  # The workspace build unifies features across crates, which hides targets
  # that use a gated API without declaring `required-features` for it.
  for features in "" parallel simd parallel,simd; do
    echo "--- features='$features'"
    run cargo clippy -p rar-rs --no-default-features --features "$features" \
      --all-targets --locked -- -D warnings
  done
fi

if want 11; then
  step "11/20 cargo test (workspace, all features)"
  # A stale `rarfiles.lst` next to the binary would reorder the solid tests.
  rm -f target/debug/rarfiles.lst
  set -o pipefail
  if cargo test --workspace --all-features --locked --no-fail-fast 2>&1 \
      | tee target/workspace-tests.log; then
    grep -hE '^test result:' target/workspace-tests.log | tail -3
  else
    echo "!!! failing tests:" >&2
    grep -hE '^test .* \.\.\. FAILED$' target/workspace-tests.log >&2 || true
    exit 1
  fi
fi

if want 12; then
  step "12/20 dependency gate (cargo deny: advisories/licenses/sources)"
  run cargo deny check --all-features --locked
fi

if want 13; then
  step "13/20 rustdoc (-D warnings)"
  RUSTDOCFLAGS="-D warnings" run cargo doc --workspace --no-deps
fi

if want 14; then
  step "14/20 official rar/unrar interop (rar 6.23 + unrar 7.23)"
  for tool in "$RARLAB/rar623/rar" "$RARLAB/rar723/unrar" "$RARLAB/rar623/default.sfx"; do
    if [ ! -x "$tool" ]; then
      echo "!!! missing $tool -- run scripts/wsl/fetch-rarlab.sh first" >&2
      exit 1
    fi
  done
  SA_REQUIRE_OFFICIAL=1 run cargo test -p rar-rs --locked \
    --test official_interop --test rewrite_tests --test rar4_solid_store_fallback \
    -- --nocapture
fi

if want 15; then
  step "15/20 binding: npm ci"
  ( cd crates/rar-napi && run npm ci ) 2>&1 | tail -3
fi

if want 16; then
  step "16/20 binding: native addon build"
  ( cd crates/rar-napi && run npx napi build --platform --release ) 2>&1 | tail -4
fi

if want 17; then
  step "17/20 binding: native test"
  ( cd crates/rar-napi && run npm test ) 2>&1 | grep -E '^ℹ (tests|pass|fail|skipped)'
fi

if want 18; then
  step "18/20 binding: wasm addon build"
  ( cd crates/rar-napi && run npx napi build --platform --release \
      --target wasm32-wasip1-threads ) 2>&1 | tail -4
fi

if want 19; then
  step "19/20 binding: WASI loader patch + WASI-forced test"
  ( cd crates/rar-napi && run node scripts/patch-wasi-loader.mjs ) 2>&1 | tail -2
  ( cd crates/rar-napi && NAPI_RS_FORCE_WASI=error run npm test ) 2>&1 \
    | grep -E '^ℹ (tests|pass|fail|skipped)'
fi

if want 20; then
  step "20/20 fuzz smoke (CI: tags/schedule only)"
  # The smoke run exists to catch panics, so force the arithmetic/assert checks
  # back on; the seed is printed so a failure can be replayed.
  export CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true
  export CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true
  export FUZZ_SEED=${FUZZ_SEED:-$RANDOM}
  echo "FUZZ_SEED=$FUZZ_SEED"
  for target in parse crypto recovery rev legacy; do
    FUZZ_ITERATIONS=5000 run cargo run --release --locked \
      --manifest-path fuzz/Cargo.toml --bin "$target"
  done
  for target in write rewrite; do
    FUZZ_ITERATIONS=500 run cargo run --release --locked \
      --manifest-path fuzz/Cargo.toml --bin "$target"
  done
fi

step "done (steps $FROM..$TO)"
