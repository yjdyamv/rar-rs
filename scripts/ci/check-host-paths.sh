#!/usr/bin/env bash
# Guard: host paths must test separators through `std::path::is_separator`.
#
# `is_separator` is platform-correct — `/` everywhere, `\` only on Windows. A
# literal `ends_with('\\')` encodes a Windows-only assumption; it once made
# `rar x p.rar dest\` create a literal `dest\` directory on Linux.
#
# Archive-member names may still normalize `\` -> `/` (RAR stores
# backslashes), so only the method-on-char-literal shape is guarded.
#
# Used by the `lint` job and by `scripts/ci-lint.sh`, which used to each carry
# their own copy of the grep.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

patterns=$(mktemp)
trap 'rm -f "$patterns"' EXIT
printf "ends_with('%s')\nstarts_with('%s')\n" '\\' '\\' > "$patterns"

if grep -rnFf "$patterns" --include='*.rs' crates/rar-cli/src crates/rar/src; then
  msg="host paths must use std::path::is_separator, not a literal backslash check"
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$msg"; fi
  echo "!! $msg" >&2
  exit 1
fi
