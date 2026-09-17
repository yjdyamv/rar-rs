#!/usr/bin/env bash
# Install the Rust toolchain for the Linux experiments.
#
# Layout chosen after measuring this box (docs/testing.md, "Running the Linux
# half under WSL2"):
#   * toolchain tree from Tencent: its manifest is the current stable AND it
#     honors RUSTUP_DIST_SERVER. Aliyun is faster but its rustup mirror is
#     stale and rewrites manifest URLs to itself, so it also decides what a
#     `rustup target add` can fetch -- and it lacks wasm32-wasip1-threads.
#   * crates.io from Aliyun (fastest working sparse index).
#   * rustup-init itself from USTC: Aliyun mirrors the dist tree, not the
#     bootstrap binary.
#
# Idempotent, needs no root: everything lands in $HOME. The login shell gets
# the mirror environment (`RUSTUP_DIST_SERVER` / `RUSTUP_UPDATE_ROOT`) and
# `~/.cargo/env`.
#
# Usage: bash scripts/wsl/setup-rust.sh
set -euo pipefail

DIST_SERVER=${DIST_SERVER:-https://mirrors.cloud.tencent.com/rustup}
UPDATE_ROOT=${UPDATE_ROOT:-https://mirrors.cloud.tencent.com/rustup/rustup}
CRATES_REGISTRY=${CRATES_REGISTRY:-sparse+https://mirrors.aliyun.com/crates.io-index/}
BOOTSTRAP=${BOOTSTRAP:-https://mirrors.ustc.edu.cn/rust-static/rustup/dist/x86_64-unknown-linux-gnu/rustup-init}
WASM_TARGET=${WASM_TARGET:-wasm32-wasip1-threads}

start_marker="# >>> rust mirrors (scripts/wsl/setup-rust.sh) >>>"
end_marker="# <<< rust mirrors (scripts/wsl/setup-rust.sh) <<<"

for rc in "$HOME/.bashrc" "$HOME/.profile"; do
  touch "$rc"
  if grep -qF "$start_marker" "$rc"; then
    sed -i "\|$start_marker|,\|$end_marker|d" "$rc"
  fi
  {
    printf '%s\n' "$start_marker"
    printf 'export RUSTUP_DIST_SERVER=%s\n' "$DIST_SERVER"
    printf 'export RUSTUP_UPDATE_ROOT=%s\n' "$UPDATE_ROOT"
    printf '%s\n' "$end_marker"
  } >> "$rc"
done
echo "mirror exports -> $HOME/.bashrc, $HOME/.profile"

export RUSTUP_DIST_SERVER="$DIST_SERVER"
export RUSTUP_UPDATE_ROOT="$UPDATE_ROOT"

mkdir -p "$HOME/.cargo"
cat > "$HOME/.cargo/config.toml" <<EOF
# crates.io through the Aliyun mirror (sparse index), for this box only.
[source.crates-io]
replace-with = "aliyun"

[source.aliyun]
registry = "$CRATES_REGISTRY"

[net]
git-fetch-with-cli = true
EOF
echo "wrote $HOME/.cargo/config.toml"

if ! command -v rustup >/dev/null 2>&1; then
  tmp=$(mktemp -d)
  echo "bootstrap rustup-init <- $BOOTSTRAP"
  curl -4 -fsSL --retry 3 -o "$tmp/rustup-init" "$BOOTSTRAP"
  chmod +x "$tmp/rustup-init"
  "$tmp/rustup-init" -y --profile default --default-toolchain none 2>&1 | tail -3
  rm -rf "$tmp"
fi

# shellcheck disable=SC1091
. "$HOME/.cargo/env"

rustup toolchain install stable --profile default 2>&1 | tail -3
rustup default stable >/dev/null
rustup component add clippy rustfmt 2>&1 | tail -2
rustup target add "$WASM_TARGET" 2>&1 | tail -2

echo "--- toolchain ---"
rustup show active-toolchain
rustup target list --installed
cargo --version
rustc --version
cargo clippy --version
echo "--- mirrors ---"
echo "RUSTUP_DIST_SERVER=$RUSTUP_DIST_SERVER"
grep -E 'registry|replace-with' "$HOME/.cargo/config.toml"
