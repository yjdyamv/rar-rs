#!/usr/bin/env bash
# Point apt at the Aliyun mirror and install the Linux build prerequisites.
#
# Must run as root: it rewrites /etc/apt/sources.list.d/ubuntu.sources (the
# stock file is kept as ubuntu.sources.orig) and installs the packages the
# workspace tests need (a C compiler for any -sys crate, plus fetch/unpack
# tools and pkg-config).
#
# Mirror choice and the measurements behind it: docs/testing.md,
# "Running the Linux half under WSL2".
#
# Usage: sudo bash scripts/wsl/setup-apt.sh
set -euo pipefail

if [ "$(id -u)" != 0 ]; then
  echo "run me with sudo: it writes /etc/apt and installs packages" >&2
  exit 1
fi

# The suite comes from the running system, so this works on any Ubuntu release.
# shellcheck disable=SC1091
. /etc/os-release
SUITE=${SUITE:-${VERSION_CODENAME:?no VERSION_CODENAME in /etc/os-release}}
MIRROR=${MIRROR:-https://mirrors.aliyun.com/ubuntu}
SRC=/etc/apt/sources.list.d/ubuntu.sources

[ -f "$SRC.orig" ] || cp -a "$SRC" "$SRC.orig"
cat > "$SRC" <<EOF
# Ubuntu package mirror: Aliyun, set by scripts/wsl/setup-apt.sh.
# Original stock file: $SRC.orig
Types: deb
URIs: $MIRROR/
Suites: $SUITE $SUITE-updates $SUITE-backports
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

# Ubuntu security updates, served by the same mirror.
Types: deb
URIs: $MIRROR/
Suites: $SUITE-security
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
echo "wrote $SRC (suite $SUITE, mirror $MIRROR)"

export DEBIAN_FRONTEND=noninteractive
# IPv6 is often advertised but dead under WSL2, which makes apt hang on it.
apt-get -o Acquire::ForceIPv4=true -o Acquire::Retries=3 update
apt-get -o Acquire::ForceIPv4=true -o Acquire::Retries=3 install -y \
  --no-install-recommends \
  build-essential pkg-config ca-certificates curl tar gzip xz-utils file unzip

cc --version | head -1
make --version | head -1
