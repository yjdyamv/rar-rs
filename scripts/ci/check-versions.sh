#!/usr/bin/env bash
# Assert that every place the workspace carries a version agrees.
#
# One tree ships four artifacts — `rar-rs`, `rar-cli`, `rar-rs-napi` and the
# `rar-rs-napi` npm package — so five fields must match: the three crates'
# `[package] version`, `crates/rar-napi/package.json`, and the `rar-rs` entry
# in the root workspace dependencies (which `cargo package` refuses to let
# drift from `crates/rar`).
#
# This lived inline in the `lint` job, again (with different parsing) in the
# `release` job, and a third time in `scripts/ci-lint.sh`; the library and CLI
# had already drifted to 0.1.2 against the binding's 0.7.0 once. One script,
# three callers.
#
# Usage: scripts/ci/check-versions.sh [expected-version]
#   expected-version  also require every field to equal it; `release` passes
#                     the git tag (minus its leading `v`).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

# GitHub renders `::error::` as an annotation; everywhere else it is noise.
fail() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*"; fi
  echo "!! $*" >&2
  exit 1
}

version() {
  sed -n '/^\[package\]/,/^\[/{s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p}' \
    "$1" | head -1
}

lib=$(version crates/rar/Cargo.toml)
cli=$(version crates/rar-cli/Cargo.toml)
napi=$(version crates/rar-napi/Cargo.toml)
pkg=$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' \
  crates/rar-napi/package.json | head -1)
dep=$(sed -n 's/^rar-rs = { version = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)

printf 'rar-rs=%s rar-cli=%s rar-rs-napi=%s package.json=%s workspace-dep=%s\n' \
  "$lib" "$cli" "$napi" "$pkg" "$dep"

if [ -z "$lib" ] || [ "$lib" != "$cli" ] || [ "$lib" != "$napi" ] \
  || [ "$lib" != "$pkg" ] || [ "$lib" != "$dep" ]; then
  fail "library, CLI, binding, package.json and workspace-dependency versions must match"
fi

if [ "$#" -gt 0 ]; then
  expected="${1#v}"
  if [ -z "$expected" ] || [ "$expected" != "$lib" ]; then
    fail "tag v$expected does not match the workspace version $lib"
  fi
  echo "releasing v$lib"
fi
