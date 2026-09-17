#!/usr/bin/env bash
# Install Node 24 (with the Chinese npm mirror) for the rar-rs-napi tests.
#
# Aliyun is the only mirror of this box's `nodejs-release` that carries v24
# (TUNA/USTC/NJU 404, Huawei 401); see docs/testing.md, "Running the Linux half
# under WSL2". The tarball is verified against the mirror's copy of the
# upstream SHASUMS256.txt. npm itself then talks to registry.npmmirror.com.
#
# Idempotent, needs no root: Node lands in ~/.local/node.
#
# Usage: bash scripts/wsl/setup-node.sh
set -euo pipefail

NODE_VERSION=${NODE_VERSION:-v24.21.0}
MIRROR=${MIRROR:-https://mirrors.aliyun.com/nodejs-release}
PREFIX=${PREFIX:-$HOME/.local}
TARBALL="node-$NODE_VERSION-linux-x64.tar.xz"
DIR="node-$NODE_VERSION-linux-x64"

mkdir -p "$PREFIX"
cd "$PREFIX"

if [ ! -d "$DIR" ]; then
  echo "downloading $MIRROR/$NODE_VERSION/$TARBALL"
  curl -4 -fsSL --retry 3 -o "$TARBALL" "$MIRROR/$NODE_VERSION/$TARBALL"
  curl -4 -fsSL --retry 3 -o SHASUMS256.txt "$MIRROR/$NODE_VERSION/SHASUMS256.txt"
  expected=$(awk -v f="$TARBALL" '$2 == f { print $1 }' SHASUMS256.txt)
  actual=$(sha256sum "$TARBALL" | awk '{print $1}')
  if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    echo "SHA256 mismatch: expected '$expected', got '$actual'" >&2
    exit 1
  fi
  echo "sha256 ok: $actual"
  tar -xf "$TARBALL"
  rm -f "$TARBALL" SHASUMS256.txt
else
  echo "$DIR already installed"
fi

ln -sfn "$PREFIX/$DIR" "$PREFIX/node"

marker="# rar-rs linux experiments: node (scripts/wsl/setup-node.sh)"
for rc in "$HOME/.bashrc" "$HOME/.profile"; do
  touch "$rc"
  if ! grep -qF "$marker" "$rc"; then
    printf '\n%s\nexport PATH="%s/node/bin:$PATH"\n' "$marker" "$PREFIX" >> "$rc"
    echo "PATH -> $rc"
  fi
done
export PATH="$PREFIX/node/bin:$PATH"

cat > "$HOME/.npmrc" <<'EOF'
# rar-rs linux experiments: Chinese npm mirror.
registry=https://registry.npmmirror.com/
EOF
echo "wrote $HOME/.npmrc"

node --version
npm --version
echo "npm registry: $(npm config get registry)"
