#!/usr/bin/env bash
# Cross-platform check of the legacy streaming work against the official tools:
#
#   Stage 1: RAR 2.x windowed multi-block compressed streaming (>= 64 MiB)
#   Stage 2: per-generation streamed member encryption (v15/v20/v29)
#
# Every archive is validated by the official rarlab unrar 7.23 Linux binary:
# `t` (CRC) must succeed and `x` must reproduce the input byte-for-byte. This is
# the Linux half of the matrix in PLAN.md; the Windows half uses WinRAR 7.23.
#
# Usage: scripts/wsl/stage12-linux.sh      (env: REPO, RARLAB, WORK, SIZE_MB)
# Prereqs: build with `cargo build --release -p rar-cli` (the script does it)
#          and scripts/wsl/fetch-rarlab.sh for the official unrar.
set -uo pipefail

REPO=${REPO:-$HOME/rar-rs}
RARLAB=${RARLAB:-$HOME/rarlab}
UNRAR=${UNRAR:-$RARLAB/rar723/unrar}
RAR=${RAR:-$REPO/target/release/rar}
WORK=${WORK:-$HOME/stage12}
SIZE_MB=${SIZE_MB:-66}
BYTES=$((SIZE_MB * 1024 * 1024))

cd "$REPO" 2>/dev/null || { echo "no repo at $REPO (set REPO=...)" >&2; exit 1; }
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi

echo "=== build release CLI ==="
cargo build --release -p rar-cli --locked 2>&1 | tail -2
[ -x "$RAR" ] || { echo "missing $RAR" >&2; exit 1; }
[ -x "$UNRAR" ] || { echo "missing $UNRAR -- run scripts/wsl/fetch-rarlab.sh" >&2; exit 1; }
"$UNRAR" | head -1

rm -rf "$WORK"; mkdir -p "$WORK"; cd "$WORK" || exit 1

echo "=== fixtures ($SIZE_MB MiB) ==="
yes "the quick brown fox jumps over the lazy dog 0123456789" | head -c "$BYTES" > big.txt
head -c "$BYTES" /dev/urandom > rand.bin
ls -l big.txt rand.bin | awk '{print "  " $9, $5}'

fail=0

check() { # label archive source [password]
  local label=$1 arc=$2 src=$3 pw=${4:-}
  local args=(-idq -y)
  [ -n "$pw" ] && args+=("-p$pw")
  local t x b
  "$UNRAR" t "${args[@]}" "$arc" >/dev/null 2>&1; t=$?
  rm -rf out; mkdir out
  "$UNRAR" x "${args[@]}" "$arc" out/ >/dev/null 2>&1; x=$?
  if cmp -s "$src" "out/$(basename "$src")"; then b=same; else b=DIFF; fi
  printf "  %-30s unrar_t=%s unrar_x=%s bytes=%s  size=%s\n" \
    "$label" "$t" "$x" "$b" "$(stat -c%s "$arc")"
  [ "$t" = 0 ] && [ "$x" = 0 ] && [ "$b" = same ] || fail=1
}

echo "=== Stage 1: windowed compressed streaming (-ma2, $SIZE_MB MiB) ==="
for m in 1 2 3 4 5; do
  rm -f "m$m.rar"
  "$RAR" a -ma2 "-m$m" "m$m.rar" big.txt
  check "-ma2 -m$m" "m$m.rar" big.txt
done

echo "=== Stage 1: multi-volume split of the windowed stream ==="
rm -f vol.rar vol.r[0-9][0-9]
"$RAR" a -ma2 -m5 -v100k vol.rar big.txt
echo "  continuation volumes: $(ls vol.r[0-9][0-9] 2>/dev/null | wc -l)"
check "-ma2 -m5 -v100k" vol.rar big.txt

echo "=== Stage 1: incompressible probe -> STORE (bounded memory) ==="
rm -f rnd.rar
"$RAR" a -ma2 -m5 rnd.rar rand.bin
check "-ma2 -m5 random" rnd.rar rand.bin

echo "=== Stage 2: streamed encryption per generation ==="
for ma in -ma2 -ma15 -ma4; do
  arc="enc${ma#-ma}.rar"
  rm -f "$arc" "enc${ma#-ma}.r"[0-9][0-9]
  "$RAR" a "$ma" -m5 -ppw -v100k "$arc" big.txt
  echo "  continuation volumes: $(ls "enc${ma#-ma}.r"[0-9][0-9] 2>/dev/null | wc -l)"
  check "$ma -p -v100k" "$arc" big.txt pw
done

echo "=== RAR 1.5 large member still streams as STORE ==="
rm -f t15.rar
"$RAR" a -ma15 -m5 t15.rar big.txt
check "-ma15 -m5 (STORE stream)" t15.rar big.txt

echo
if [ "$fail" = 0 ]; then
  echo "ALL LINUX LEGACY-STREAMING CHECKS PASSED"
else
  echo "FAILURES PRESENT"
fi
exit "$fail"
