#!/usr/bin/env bash
# Fetch the official RAR console tools the interop suites drive.
#
# Two releases, matching the CI interop job: 7.23's UnRAR validates what we
# emit, while the RAR writer must be 6.23 because 7.23 dropped RAR4 creation
# (`rar a -ma4` is rejected). Layout: ~/rarlab/{rar623/rar, rar723/unrar,
# rar623/default.sfx} -- exactly what SA_OFFICIAL_* expect.
#
# Usage: bash scripts/wsl/fetch-rarlab.sh
set -euo pipefail

ROOT=${RARLAB:-$HOME/rarlab}
mkdir -p "$ROOT"
cd "$ROOT"

for version in 723 623; do
  tarball="rar$version.tar.gz"
  [ -f "$tarball" ] || curl -4 -fsSL --retry 3 -o "$tarball" \
    "https://www.rarlab.com/rar/rarlinux-x64-$version.tar.gz"
  if [ ! -d "rar$version" ]; then
    tar -xzf "$tarball"          # both tarballs unpack a `rar/` directory
    mv rar "rar$version"
  fi
done

for tool in "$ROOT/rar623/rar" "$ROOT/rar723/unrar" "$ROOT/rar623/default.sfx"; do
  if [ ! -x "$tool" ]; then
    echo "missing $tool" >&2
    exit 1
  fi
done
echo "tools in $ROOT:"
"$ROOT/rar623/rar" | head -1
"$ROOT/rar723/unrar" | head -1
