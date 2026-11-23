#!/usr/bin/env bash
# Download a pinned actionlint and run it over the workflow files.
#
# Why this exists: the workflows are the one part of this repo that nothing
# compiles or lints. A stale `actions/cache@v4` sat in the Windows job while the
# build matrix had moved to `v6` — no CI step could have noticed, because no CI
# step reads the workflow files. actionlint does (unknown action inputs, broken
# expressions, missing `needs`, shell problems inside `run:` blocks).
#
# Integrity comes from the SHA-256 published in actionlint's own release
# checksums, pinned in the workflow, not from the host: a substituted tarball
# is a hard error. Same rule as scripts/ci/install-musl-toolchain.sh.
#
# Inputs (env):
#   ACTIONLINT_BIN      use this binary instead of downloading (local runs)
#   ACTIONLINT_VERSION  required unless ACTIONLINT_BIN is set, e.g. 1.7.12
#   ACTIONLINT_SHA256   required unless ACTIONLINT_BIN is set, from
#                       https://github.com/rhysd/actionlint/releases/download/v<version>/actionlint_<version>_checksums.txt
#   ACTIONLINT_DEST     where to unpack (default $RUNNER_TEMP/actionlint)
#
# Usage: bash scripts/ci/run-actionlint.sh [file ...]
#
# With no arguments every `.github/workflows/*.yml` is checked; actionlint
# takes files (not directories), so the glob is expanded here. The download is
# the Linux x86_64 release, i.e. that path is CI-only; `ACTIONLINT_BIN` is how
# a local run on another OS uses its own binary.
set -euo pipefail

if [ "$#" -eq 0 ]; then
  set -- .github/workflows/*.yml
fi

bin=${ACTIONLINT_BIN:-}
if [ -z "$bin" ]; then
  : "${ACTIONLINT_VERSION:?set ACTIONLINT_VERSION (e.g. 1.7.12), or ACTIONLINT_BIN}"
  : "${ACTIONLINT_SHA256:?set ACTIONLINT_SHA256 (from the release checksums), or ACTIONLINT_BIN}"
  case "$ACTIONLINT_SHA256" in
    *[!0-9a-fA-F]*) echo "ACTIONLINT_SHA256 must be a hex digest, got '$ACTIONLINT_SHA256'" >&2; exit 1 ;;
  esac
  [ "${#ACTIONLINT_SHA256}" = 64 ] ||
    { echo "ACTIONLINT_SHA256 must be 64 hex characters" >&2; exit 1; }

  dest=${ACTIONLINT_DEST:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/actionlint}
  bin="$dest/actionlint"
  if [ ! -x "$bin" ]; then
    archive="$dest.tar.gz"
    url="https://github.com/rhysd/actionlint/releases/download/v$ACTIONLINT_VERSION/actionlint_${ACTIONLINT_VERSION}_linux_amd64.tar.gz"
    mkdir -p "$dest"
    echo "fetching $url"
    if ! curl -fsSL --retry 3 --retry-delay 2 -o "$archive" "$url"; then
      echo "::error::could not download $url" >&2
      exit 1
    fi
    actual="$(sha256sum "$archive" | cut -d' ' -f1)"
    if [ "$actual" != "$ACTIONLINT_SHA256" ]; then
      rm -f "$archive"
      echo "::error::actionlint $ACTIONLINT_VERSION checksum mismatch: expected $ACTIONLINT_SHA256, got $actual" >&2
      exit 1
    fi
    tar -xzf "$archive" -C "$dest" actionlint
    rm -f "$archive"
  fi
fi

exec "$bin" -no-color "$@"
