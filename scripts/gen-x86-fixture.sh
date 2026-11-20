#!/usr/bin/env bash
# Regenerate `crates/rar/tests/data/dense-x86-prefix.bin`.
#
# The file is the first 129,400 bytes of the PE produced by compiling
# `scripts/fixtures/dense-x86-corpus.rs`: section headers, DOS stub and about
# 128 KiB of compiler-generated register-register code with disp32 operands.
# The regression in `crates/rar/tests/rar50_roundtrip.rs` needs code *shaped
# like that* — the multi-chunk match-finder corruption only appeared when a
# small dictionary slid over a stream that repeats instruction forms at many
# distances.
#
# The bytes are ours (this repository compiles the source), so the inventory
# entry under `## Test data` in `THIRD_PARTY_LICENSES.md` only has to record
# how they are produced. Re-running this script is only necessary when the
# shape of the corpus needs to change; the committed bytes are the fixture,
# and the test does not depend on the compiler version.
#
# Usage: scripts/gen-x86-fixture.sh [output-path]
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

out=${1:-crates/rar/tests/data/dense-x86-prefix.bin}
size=129400
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# `-Brepro` asks link.exe for reproducible output. Without it the image carries a
# build timestamp and a debug-directory GUID that differed between two runs of this
# script, so "regenerate the fixture" would rewrite a committed file for no reason.
# `-DEBUG:NONE` keeps the PDB path (which contains the temp directory) out of the
# image. Bytes are reproducible per toolchain, not across MSVC versions, which is
# the contract that matters: the fixture is committed, and this script exists to
# change its shape on purpose, not to be run by CI. The flags use the dash form
# because Git Bash rewrites a leading `/` into a Windows path (`/Brepro` arrives
# at link.exe as `C:/Program Files/Git/Brepro`).
rustc --edition 2024 -O -C debuginfo=0 -C panic=abort \
  -C link-arg=-Brepro -C link-arg=-DEBUG:NONE -o "$work/corpus.exe" \
  scripts/fixtures/dense-x86-corpus.rs

head -c "$size" "$work/corpus.exe" > "$out"
echo "wrote $out ($(wc -c < "$out") bytes, from $(basename "$work")/corpus.exe)"