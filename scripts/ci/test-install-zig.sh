#!/usr/bin/env bash
# Regression tests for scripts/ci/install-zig.sh. A fixture tarball served over
# HTTP stands in for the real one, so every path runs without a 47 MiB download.
# Mirror layout matches the real thing: <base>/<version>/<tarball>, and both
# release spellings are covered (zig 0.15 renamed `zig-linux-x86_64-<v>` to
# `zig-x86_64-linux-<v>`, which silently 404'd the pinned 0.14.0 download).
# The GITHUB_PATH case guards the bug that made CI's build step fail: zig was
# installed but never put on PATH for the later steps.
#
# Usage: bash scripts/ci/test-install-zig.sh
set -uo pipefail

PORT=${PORT:-8823}
VER=9.9.9
NEW="zig-x86_64-linux-$VER.tar.xz"
OLD="zig-linux-x86_64-$VER.tar.xz"
BASE="http://127.0.0.1:$PORT"

cd "${REPO:-$HOME/rar-rs}" || exit 1
script=scripts/ci/install-zig.sh

rm -rf /tmp/srv /tmp/fx /tmp/zt* /tmp/ghpath
mkdir -p "/tmp/srv/new/$VER" "/tmp/srv/old/$VER" "/tmp/srv/bad/$VER" "/tmp/fx/zig-x86_64-linux-$VER"
printf '#!/bin/sh\necho %s-fake\n' "$VER" > "/tmp/fx/zig-x86_64-linux-$VER/zig"
chmod +x "/tmp/fx/zig-x86_64-linux-$VER/zig"
tar -cJf "/tmp/srv/new/$VER/$NEW" -C /tmp/fx "zig-x86_64-linux-$VER"
cp "/tmp/srv/new/$VER/$NEW" "/tmp/srv/old/$VER/$OLD"
printf 'garbage' | xz > "/tmp/srv/bad/$VER/$NEW"
sha=$(sha256sum "/tmp/srv/new/$VER/$NEW" | cut -d' ' -f1)
echo "fixture: new=$NEW old=$OLD sha256=$sha"

PORT="$PORT" node -e '
const http = require("http"), fs = require("fs");
http.createServer((q, s) => {
  const p = "/tmp/srv" + q.url.split("?")[0];
  if (!fs.existsSync(p) || fs.statSync(p).isDirectory()) { s.writeHead(404); return s.end(); }
  const st = fs.statSync(p), r = q.headers.range;
  if (r) { const start = +r.replace(/[^0-9].*/, ""); s.writeHead(206, { "Content-Length": st.size - start }); fs.createReadStream(p, { start }).pipe(s); }
  else { s.writeHead(200, { "Content-Length": st.size }); fs.createReadStream(p).pipe(s); }
}).listen(process.env.PORT)
' &
srv=$!
sleep 1
trap 'kill $srv 2>/dev/null' EXIT

pass=0
fail=0
check() { # label expected-substring want-rc -- cmd...
  local label=$1 want=$2 wantrc=$3
  shift 3
  local out rc
  out=$("$@" 2>&1)
  rc=$?
  if [ "$rc" = "$wantrc" ] && printf '%s' "$out" | grep -q "$want"; then
    printf 'ok   %-36s rc=%s\n' "$label" "$rc"
    pass=$((pass + 1))
  else
    printf 'FAIL %-36s rc=%s (want %s, "%s")\n%s\n' "$label" "$rc" "$wantrc" "$want" "$out"
    fail=$((fail + 1))
  fi
}

check "new naming (>=0.15 style)" "$VER-fake" 0 \
  env ZIG_VERSION=$VER ZIG_SHA256="$sha" ZIG_BASES="$BASE/new" ZIG_HOME=/tmp/zt1 bash "$script"
check "cache hit (no download)" "already installed" 0 \
  env ZIG_VERSION=$VER ZIG_SHA256="$sha" ZIG_BASES="$BASE/new" ZIG_HOME=/tmp/zt1 GITHUB_PATH=/tmp/ghpath-cache bash "$script"
check "old naming only (<0.15 style)" "$VER-fake" 0 \
  env ZIG_VERSION=$VER ZIG_SHA256="$sha" ZIG_BASES="$BASE/old" ZIG_HOME=/tmp/zt2 bash "$script"
check "bad mirror then good (race)" "$VER-fake" 0 \
  env ZIG_VERSION=$VER ZIG_SHA256="$sha" ZIG_RACE=3 ZIG_BASES="$BASE/bad $BASE/new" ZIG_HOME=/tmp/zt3 bash "$script"
check "all candidates bad" "no candidate delivered" 1 \
  env ZIG_VERSION=$VER ZIG_SHA256="$sha" ZIG_RACE=1 ZIG_BASES="$BASE/bad" ZIG_DEADLINE=20 ZIG_HOME=/tmp/zt4 bash "$script"
check "malformed checksum" "must be a hex digest" 1 \
  env ZIG_VERSION=$VER ZIG_SHA256=zzzz ZIG_BASES="$BASE/new" ZIG_HOME=/tmp/zt5 bash "$script"
check "short checksum" "must be 64 hex" 1 \
  env ZIG_VERSION=$VER ZIG_SHA256=deadbeef ZIG_BASES="$BASE/new" ZIG_HOME=/tmp/zt6 bash "$script"
check "adds ZIG_HOME to GITHUB_PATH" "added /tmp/zt7 to GITHUB_PATH" 0 \
  env ZIG_VERSION=$VER ZIG_SHA256="$sha" ZIG_BASES="$BASE/new" ZIG_HOME=/tmp/zt7 GITHUB_PATH=/tmp/ghpath bash "$script"
check "missing version" "set ZIG_VERSION" 1 \
  env -u ZIG_VERSION ZIG_SHA256="$sha" bash "$script"

# Both the install and the cache-hit path have to publish the PATH entry, or the
# build step that follows cannot find `zig`.
check_file() { # label file expected-line
  if grep -qx "$3" "$2" 2>/dev/null; then
    printf 'ok   %-36s %s\n' "$1" "$(tr '\n' ' ' < "$2")"
    pass=$((pass + 1))
  else
    printf 'FAIL %-36s %s\n' "$1" "$(cat "$2" 2>/dev/null)"
    fail=$((fail + 1))
  fi
}
check_file "GITHUB_PATH after install" /tmp/ghpath /tmp/zt7
check_file "GITHUB_PATH on cache hit" /tmp/ghpath-cache /tmp/zt1

echo "passed=$pass failed=$fail"
[ "$fail" = 0 ]
