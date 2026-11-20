#!/usr/bin/env bash
# Publish the three workspace crates to crates.io, in dependency order.
#
# `rar-rs` comes first: `rar-cli` and `rar-rs-napi` depend on it, and
# `cargo publish` resolves that dependency through the registry, not the
# workspace. The index is polled between crates so the next publish never races
# the upload that just finished.
#
# Idempotent on purpose: a version already visible in the sparse index is
# skipped, so re-running a tag (`workflow_dispatch` with `release_tag`) only
# fills the gaps instead of failing on the first `already uploaded`.
#
# Authentication is the caller's business: `CARGO_REGISTRY_TOKEN` must be set
# (the `publish` job mints a short-lived one through OIDC; locally, use
# `cargo login` or a token with `publish-new` + `publish-update`).
#
# Usage: scripts/ci/publish-crates.sh [--dry-run]
#        scripts/ci/publish-crates.sh --index-path <crate-name>
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

INDEX_BASE="${CRATES_INDEX_BASE:-https://index.crates.io}"
# Dependency order. Keep in sync with `crates/*/Cargo.toml`.
CRATES=(rar-rs rar-cli rar-rs-napi)

# The sparse index's layout: 1/ and 2/ keep one and two character names
# directly, three character names sit under their first character, and
# everything longer splits the first two and the next two into directories.
# `rar-rs` therefore lives at `ra/r-/rar-rs`.
index_path() {
  local name=$1
  case ${#name} in
    1) printf '1/%s' "$name" ;;
    2) printf '2/%s' "$name" ;;
    3) printf '3/%s/%s' "${name:0:1}" "$name" ;;
    *) printf '%s/%s/%s' "${name:0:2}" "${name:2:2}" "$name" ;;
  esac
}

if [ "${1:-}" = "--index-path" ]; then
  index_path "$2"
  exit 0
fi

dry_run=0
if [ "${1:-}" = "--dry-run" ]; then
  dry_run=1
fi

# GitHub renders `::error::` as an annotation; everywhere else it is noise.
fail() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*"; fi
  echo "!! $*" >&2
  exit 1
}

version() {
  sed -n '/^\[package\]/,/^\[/{s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p}' \
    crates/rar/Cargo.toml | head -1
}

# published <crate> <version>: is that exact version in the index already?
published() {
  local crate=$1 want=$2
  curl -fsS --max-time 20 "$INDEX_BASE/$(index_path "$crate")" 2>/dev/null |
    grep -qF "\"vers\":\"$want\""
}

current=$(version)
[ -n "$current" ] || fail "no version in crates/rar/Cargo.toml"

if [ "$dry_run" = 0 ] && [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
  # `cargo login`'s stored credentials work too; this only catches the case
  # where CI forgot to hand a token to the job at all.
  if [ -z "${CARGO_HOME:-$HOME/.cargo}" ] || [ ! -f "${CARGO_HOME:-$HOME/.cargo}/credentials.toml" ]; then
    fail "CARGO_REGISTRY_TOKEN is not set and there is no credentials.toml"
  fi
fi

echo "publishing v$current (${CRATES[*]})"
for crate in "${CRATES[@]}"; do
  if published "$crate" "$current"; then
    echo "skip  $crate $current — already on the index"
    continue
  fi
  if [ "$dry_run" = 1 ]; then
    echo "would publish $crate $current"
    continue
  fi
  echo "publish $crate $current"
  cargo publish -p "$crate" --locked
  # The upload is asynchronous: the next crate's dependency resolution reads
  # the index, so wait for this one to be visible rather than sleeping blindly.
  for _ in $(seq 1 60); do
    published "$crate" "$current" && break
    sleep 5
  done
  published "$crate" "$current" ||
    fail "$crate $current never showed up in the index at $INDEX_BASE"
done

echo "done: v$current"