#!/usr/bin/env bash
# Install a pinned zig, for the static-musl cross builds in CI.
#
# Why not an action: the two musl targets (`x86_64-unknown-linux-musl`,
# `aarch64-unknown-linux-musl`) are built with `napi build -x`, i.e.
# cargo-zigbuild, which needs `zig` on PATH. The `setup-zig` action pinned
# ziglang.org, cached through a broken cache entry and hung the job when the
# download stalled, so this does the few things it needs to do explicitly:
#
#   * download from ziglang.org only (no third-party mirror),
#   * verify the official SHA-256 before unpacking,
#   * resume a stalled transfer (`-C -`) and abort one that stops making
#     progress (`--speed-limit`/`--speed-time`) instead of hanging,
#   * skip everything when the unpacked toolchain is already there, so CI can
#     cache `$ZIG_HOME`.
#
# Inputs (env):
#   ZIG_VERSION  required, e.g. 0.16.0
#   ZIG_SHA256   required, from https://ziglang.org/download/index.json
#   ZIG_HOME     where to unpack (default /tmp/zig)
#   ZIG_URL      override for the tarball URL (tests; default is the official
#                https://ziglang.org/download/<version>/zig-<arch>-linux-<version>.tar.xz)
#
# Usage: ZIG_VERSION=0.16.0 ZIG_SHA256=<hash> bash scripts/ci/install-zig.sh
set -euo pipefail

: "${ZIG_VERSION:?set ZIG_VERSION (e.g. 0.16.0)}"
: "${ZIG_SHA256:?set ZIG_SHA256 (from ziglang.org/download/index.json)}"
case "$ZIG_SHA256" in
  *[!0-9a-fA-F]*) echo "ZIG_SHA256 must be a hex digest, got '$ZIG_SHA256'" >&2; exit 1 ;;
esac
[ "${#ZIG_SHA256}" = 64 ] || { echo "ZIG_SHA256 must be 64 hex characters" >&2; exit 1; }
ZIG_HOME=${ZIG_HOME:-/tmp/zig}

# The release runners are x86_64; keep the arch explicit so a different runner
# fails loudly instead of fetching a wrong-arch binary.
arch=$(uname -m)
if [ "$arch" != "x86_64" ]; then
  echo "unsupported runner architecture: $arch (only x86_64 is pinned)" >&2
  exit 1
fi

if [ -x "$ZIG_HOME/zig" ]; then
  echo "zig already installed in $ZIG_HOME"
  "$ZIG_HOME/zig" version
  exit 0
fi

tarball="zig-x86_64-linux-$ZIG_VERSION.tar.xz"
url=${ZIG_URL:-"https://ziglang.org/download/$ZIG_VERSION/$tarball"}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for attempt in 1 2 3 4 5 6; do
  echo "== zig download attempt $attempt: $url"
  if curl -fL -C - --connect-timeout 15 \
      --speed-limit 2048 --speed-time 90 \
      -o "$work/$tarball" "$url"; then
    break
  fi
  [ "$attempt" = 6 ] && { echo "zig download failed 6 times" >&2; exit 1; }
  sleep 10
done

echo "$ZIG_SHA256  $work/$tarball" | sha256sum -c -
mkdir -p "$ZIG_HOME"
tar -xJf "$work/$tarball" -C "$ZIG_HOME" --strip-components=1

"$ZIG_HOME/zig" version
echo "zig installed in $ZIG_HOME"
