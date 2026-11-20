#!/usr/bin/env bash
# Install the toolchain the two static-musl cross builds need: `zig` and
# `cargo-zigbuild`.
#
# Why this exists: `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`
# are built with `napi build -x`, i.e. cargo-zigbuild, which drives `zig` — so
# the two halves are only ever needed together, from the same two matrix rows.
# They used to be two scripts and two workflow steps; one step that installs
# whichever half its version variable names is both shorter and testable
# (`scripts/ci/test-install-zig.sh` exercises the zig half alone, with
# ZIG_VERSION/ZIG_SHA256 and no CARGO_ZIGBUILD_VERSION).
#
#   ZIG_VERSION / ZIG_SHA256      install `zig` (see below)
#   CARGO_ZIGBUILD_VERSION        install `cargo-zigbuild` (compiled from source
#                                 on a cache miss; `actions/cache` keeps
#                                 `~/.cargo/bin/cargo-zigbuild`, so a warm cache
#                                 skips a multi-minute build)
#
# Set at least one; each half is skipped when its variable is empty. The
# integrity rule is the same for both: nothing is installed that we did not pin.
#
# Usage:
#   ZIG_VERSION=0.14.0 ZIG_SHA256=<hash> CARGO_ZIGBUILD_VERSION=0.23.4 \
#     bash scripts/ci/install-musl-toolchain.sh
set -euo pipefail

if [ -z "${ZIG_VERSION:-}" ] && [ -z "${CARGO_ZIGBUILD_VERSION:-}" ]; then
  echo "nothing to install: set ZIG_VERSION (+ ZIG_SHA256) and/or CARGO_ZIGBUILD_VERSION" >&2
  exit 1
fi

# Install a pinned zig, for the static-musl cross builds in CI.
#
# Zig's own guidance is that CI should not depend on ziglang.org (it has no
# uptime or speed guarantees; see
# https://ziglang.org/download/community-mirrors/), which is exactly how the
# v0.8.0 release lost ~20 minutes and a later run failed outright.
#
# Integrity comes from the SHA-256 published by ziglang.org and pinned in the
# workflow, not from the host we happen to fetch from: every candidate is
# verified before use, so a mirror can only ever supply the same bytes (or be
# skipped). That is what makes fetching from the community mirrors safe, and
# they are raced in parallel so a slow-but-alive mirror cannot hold the job:
# the first candidate that delivers a verified tarball wins.
#
# Inputs (env):
#   ZIG_VERSION   required, e.g. 0.14.0
#   ZIG_SHA256    required, from https://ziglang.org/download/index.json
#   ZIG_HOME      where to unpack (default /tmp/zig)
#   ZIG_BASES     candidate bases, space separated (defaults to the list below:
#                 the community mirrors Zig publishes, then ziglang.org itself)
#   ZIG_TARBALLS  release file names to try, in order (defaults to both the
#                 post-0.15 and pre-0.15 spellings)
#   ZIG_RACE      how many candidates to run at once (default 4)
#   ZIG_DEADLINE  give up after this many seconds (default 1200)
install_zig() {
  case "$ZIG_SHA256" in
    *[!0-9a-fA-F]*) echo "ZIG_SHA256 must be a hex digest, got '$ZIG_SHA256'" >&2; exit 1 ;;
  esac
  [ "${#ZIG_SHA256}" = 64 ] || { echo "ZIG_SHA256 must be 64 hex characters" >&2; exit 1; }

  local ZIG_HOME=${ZIG_HOME:-/tmp/zig}
  local ZIG_RACE=${ZIG_RACE:-4}
  local ZIG_DEADLINE=${ZIG_DEADLINE:-1200}
  local ZIG_BASES=${ZIG_BASES:-$(sed 's/^[[:space:]]*//; s/[[:space:]]*$//' <<'BASES'
https://pkg.hexops.org/zig
https://zigmirror.hryx.net/zig
https://zig.linus.dev/zig
https://zig.squirl.dev
https://zig.mirror.mschae23.de/zig
https://ziglang.freetls.fastly.net
https://zig.tilok.dev
https://zig-mirror.tsimnet.eu/zig
https://zig.karearl.com/zig
https://pkg.earth/zig
https://fs.liujiacai.net/zigbuilds
https://zigmirror.com
https://zig.chainsafe.dev
https://zig.savalione.com
https://zig.bcr.ist
https://zig.vortan.dev/zig
https://ziglang.org/download
BASES
)}

  # The release runners are x86_64; keep the arch explicit so a different runner
  # fails loudly instead of fetching a wrong-arch binary.
  local arch
  arch=$(uname -m)
  if [ "$arch" != "x86_64" ]; then
    echo "unsupported runner architecture: $arch (only x86_64 is pinned)" >&2
    exit 1
  fi

  # Zig has to be on PATH for the *later* steps (`napi build -x`); forgetting
  # this is what made the build step fail after the toolchain had installed
  # fine.
  if [ -n "${GITHUB_PATH:-}" ]; then
    echo "$ZIG_HOME" >> "$GITHUB_PATH"
    echo "added $ZIG_HOME to GITHUB_PATH"
  fi

  if [ -x "$ZIG_HOME/zig" ]; then
    echo "zig already installed in $ZIG_HOME"
    "$ZIG_HOME/zig" version
    return 0
  fi

  local tarball_names=${ZIG_TARBALLS:-"zig-x86_64-linux-$ZIG_VERSION.tar.xz zig-linux-x86_64-$ZIG_VERSION.tar.xz"}
  local work winner
  work=$(mktemp -d)
  winner="$work/winner.tar.xz"
  # shellcheck disable=SC2064 # expand $work now: the trap must not outlive it
  trap "rm -rf '$work'" EXIT

  # Fetch one candidate and hand it over if it verifies. Resumes across attempts
  # (`-C -`) and aborts a transfer that stops making progress, so a dead mirror
  # costs seconds and a stalling one a minute, not the whole job. Both tarball
  # names are tried: zig 0.15 renamed them from `zig-linux-x86_64-<v>` to
  # `zig-x86_64-linux-<v>`, and the SHA-256 check means a wrong pick simply
  # fails over to the other name instead of installing the wrong thing.
  local base out name url _
  try_candidate() {
    base=${1%/} out=$2
    for name in $tarball_names; do
      url="$base/$ZIG_VERSION/$name"
      for _ in 1 2 3; do
        if curl -fL -C - --connect-timeout 15 --max-time 900 \
            --speed-limit 4096 --speed-time 60 -o "$out" "$url" 2>/dev/null; then
          if echo "$ZIG_SHA256  $out" | sha256sum -c - >/dev/null 2>&1; then
            mv -f "$out" "$winner"
            echo "verified $name from $base"
            return 0
          fi
          echo "checksum mismatch for $name from $base" >&2
          rm -f "$out"
          break
        fi
        sleep 5
      done
    done
    return 1
  }

  echo "== zig $ZIG_VERSION: racing up to $ZIG_RACE candidates of:"
  for base in $ZIG_BASES; do echo "   $base"; done

  local pids=() started=0 deadline pid alive
  for base in $ZIG_BASES; do
    started=$((started + 1))
    [ "$started" -gt "$ZIG_RACE" ] && break
    try_candidate "$base" "$work/c$started.tar.xz" &
    pids+=("$!")
  done

  deadline=$((SECONDS + ZIG_DEADLINE))
  while [ ! -f "$winner" ] && [ "$SECONDS" -lt "$deadline" ]; do
    alive=0
    for pid in "${pids[@]}"; do
      kill -0 "$pid" 2>/dev/null && alive=1
    done
    [ "$alive" = 0 ] && break
    sleep 2
  done
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true

  if [ ! -f "$winner" ]; then
    echo "no candidate delivered a verified zig $ZIG_VERSION tarball" >&2
    exit 1
  fi

  mkdir -p "$ZIG_HOME"
  tar -xJf "$winner" -C "$ZIG_HOME" --strip-components=1
  "$ZIG_HOME/zig" version
  echo "zig $ZIG_VERSION installed in $ZIG_HOME"
}

# Install the cargo-zigbuild version CI cross-builds the musl addons with.
install_cargo_zigbuild() {
  local bin
  bin="${CARGO_HOME:-$HOME/.cargo}/bin/cargo-zigbuild"
  if [ ! -x "$bin" ]; then
    cargo install cargo-zigbuild --version "$CARGO_ZIGBUILD_VERSION" --locked
  fi

  echo "cargo-zigbuild: $("$bin" --version 2>/dev/null || echo installed)"
  if [ -n "${GITHUB_PATH:-}" ]; then
    echo "${CARGO_HOME:-$HOME/.cargo}/bin" >> "$GITHUB_PATH"
  fi
}

# ZIG_SHA256 without ZIG_VERSION is a broken call, not a request to skip.
if [ -n "${ZIG_VERSION:-}" ] || [ -n "${ZIG_SHA256:-}" ]; then
  : "${ZIG_VERSION:?set ZIG_VERSION (e.g. 0.14.0)}"
  : "${ZIG_SHA256:?set ZIG_SHA256 (from ziglang.org/download/index.json)}"
  install_zig
fi

if [ -n "${CARGO_ZIGBUILD_VERSION:-}" ]; then
  install_cargo_zigbuild
fi
