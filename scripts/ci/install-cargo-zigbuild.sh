#!/usr/bin/env bash
# Install the cargo-zigbuild version CI cross-builds the musl addons with.
#
# Compiled from source on a cache miss (`actions/cache` keeps
# `~/.cargo/bin/cargo-zigbuild`), so a warm cache skips a multi-minute build.
#
# Inputs (env):
#   CARGO_ZIGBUILD_VERSION  required, e.g. 0.23.4
#
# Usage: CARGO_ZIGBUILD_VERSION=0.23.4 bash scripts/ci/install-cargo-zigbuild.sh
set -euo pipefail

: "${CARGO_ZIGBUILD_VERSION:?set CARGO_ZIGBUILD_VERSION (e.g. 0.23.4)}"

bin="${CARGO_HOME:-$HOME/.cargo}/bin/cargo-zigbuild"
if [ ! -x "$bin" ]; then
  cargo install cargo-zigbuild --version "$CARGO_ZIGBUILD_VERSION" --locked
fi

echo "cargo-zigbuild: $("$bin" --version 2>/dev/null || echo installed)"
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "${CARGO_HOME:-$HOME/.cargo}/bin" >> "$GITHUB_PATH"
fi
