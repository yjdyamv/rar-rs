#!/usr/bin/env bash
# Guard: every crate directory carries the repository's legal texts verbatim.
#
# `cargo package` only ships files under the package directory, so a crate that
# relied on the root copies would publish a tarball with no `LICENSE`, no
# `NOTICE` and no third-party inventory — while `license = "BSD-2-Clause"` and
# the `rars` ports (MIT OR Apache-2.0) both assume the texts travel with the
# source. The first `cargo publish` would have shipped exactly that, so the
# files are copied into each crate and this guard keeps the copies honest.
#
# Copied, not symlinked: cargo does not follow symlinks out of the package
# directory, and a symlink checked out as a regular file on Windows would defeat
# the byte comparison the guard depends on.
#
# Used by the `check` job and by `scripts/ci-lint.sh`.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

# The texts a redistributor of any of the three crates must receive: the
# project's own license, the attribution notice, the inventory of third-party
# portions, and the two upstream license texts `NOTICE` points at.
files=(
  LICENSE
  NOTICE
  THIRD_PARTY_LICENSES.md
  LICENSES/Apache-2.0.txt
  LICENSES/MIT.txt
)

fail() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*"; fi
  echo "!! $*" >&2
  exit 1
}

for crate in crates/rar crates/rar-cli crates/rar-napi; do
  for f in "${files[@]}"; do
    if [ ! -f "$crate/$f" ]; then
      fail "$crate/$f is missing: cargo package cannot reach the root copy"
    fi
    if ! cmp -s "$f" "$crate/$f"; then
      fail "$crate/$f differs from $f: copy it again after editing the root one"
    fi
  done
done

echo "license texts are identical in crates/rar, crates/rar-cli and crates/rar-napi"