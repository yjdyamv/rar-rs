#!/usr/bin/env bash
# Clippy over every `rar-rs` feature combination the workspace build cannot
# cover, plus the ungated build.
#
# A workspace-wide build unifies features across crates (`rar-rs-napi` pulls in
# `parallel` + `simd`), which hides targets that use a gated API without
# declaring `required-features` for it — the `mtwin` example failed
# `cargo check -p rar-rs --all-targets` for exactly that reason while the
# workspace check stayed green. That is also why the combination with *both*
# features on is not listed here: `crates/rar` has `default = []` and exactly
# two features, so `--features parallel,simd` is byte-for-byte what
# `cargo clippy --workspace --all-features` already runs, in the step above.
#
# What is left to prove is the ungated build (where `cfg`-gated code is
# genuinely absent, so dead code and unused imports surface) and the two
# single-feature builds (where a `simd`-gated item that needs a
# `parallel`-gated one fails to resolve).
#
# Shared by the `check` job and `scripts/ci-lint.sh`; `scripts/wsl/ci-linux.sh`
# keeps its own copy because it interleaves the loop with its step numbering.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

for features in "" parallel simd; do
  printf '\n--- rar-rs features=%s\n' "${features:-<none>}"
  # Word splitting is the point: "" must stay a single empty `--features`.
  # shellcheck disable=SC2086
  cargo clippy -p rar-rs --no-default-features --features "$features" \
    --all-targets --locked -- -D warnings
done
