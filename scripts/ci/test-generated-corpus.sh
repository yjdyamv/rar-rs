#!/usr/bin/env bash
# The generated legacy-corpus suites, in the order CI runs them.
#
# These are the RAR 1.3-4.x, `.rev`, VM-filter and recovery-record suites whose
# archives are built at run time by the official tool (see
# `crates/rar/tests/support/generate.rs`); they self-skip when no tool is
# present. Three places used to repeat this target list — the `lint` job, the
# `lint-windows` job and `scripts/ci-lint.sh` — so a new suite could be added
# to one and silently stay out of the others.
#
# `lint` runs this against rarlab's Linux builds without `SA_REQUIRE_WINRAR`
# (2.90/3.00 have no Linux tarball and must keep skipping); `lint-windows` sets
# `SA_REQUIRE_WINRAR=1` so a gap fails instead.
#
# Usage: scripts/ci/test-generated-corpus.sh [extra cargo test args]
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

targets=(
  rar4_read
  rar13_read
  rar4_rev3
  rar4_edge_cases
  rar_vm_filters
  streaming_edge_cases
  structured_recovery_mutations
  archive_editor
  winrar_generation
  rar50_roundtrip
)

args=()
for target in "${targets[@]}"; do
  args+=(--test "$target")
done

exec cargo test -p rar-rs --locked "${args[@]}" "$@"
