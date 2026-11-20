#!/usr/bin/env bash
# Assemble the release payload from the `build` / `build-wasm` artifacts.
#
# This decides whether a release is *complete*: every platform addon present
# exactly once, every WASI file present, the generated loader identical across
# targets, and no collision when the `LICENSES/` tree is flattened into one
# directory. It lived inline in the `release` job, where the only way to test it
# was to publish; it is a script so `scripts/ci/test-release-scripts.sh` can run
# it against fixtures on every push (and so a failed release can be reproduced
# locally with the downloaded artifacts).
#
# Usage: collect-release-assets.sh [BINDINGS_DIR] [WASM_DIR] [OUT_DIR] [ROOT]
#
# Defaults are the paths the workflow uses: bindings/, wasm/, release/, `.`.
# `ROOT` is where LICENSE, NOTICE, THIRD_PARTY_LICENSES.md and LICENSES/ live.
#
# Needs bash 4 (`mapfile`) and coreutils sha256sum; i.e. the CI image, Git Bash
# and Linux — not macOS's bash 3.2.
set -euo pipefail
export LC_ALL=C

bindings=${1:-bindings}
wasm=${2:-wasm}
out=${3:-release}
root=${4:-.}

fail() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*" >&2; else echo "!! $*" >&2; fi
  exit 1
}

# The release contract: one addon per published platform, no more, no less.
EXPECTED_ADDONS=(
  rar-rs-napi.darwin-arm64.node
  rar-rs-napi.win32-x64-msvc.node
  rar-rs-napi.win32-ia32-msvc.node
  rar-rs-napi.win32-arm64-msvc.node
  rar-rs-napi.linux-x64-gnu.node
  rar-rs-napi.linux-x64-musl.node
  rar-rs-napi.linux-arm64-gnu.node
  rar-rs-napi.linux-arm64-musl.node
  rar-rs-napi.linux-arm-gnueabihf.node
)
# Every listed WASI file is part of the same contract.
WASI_FILES=(
  rar-rs-napi.wasm32-wasi.wasm
  rar-rs-napi.wasm32-wasi.debug.wasm
  rar-rs-napi.wasi.cjs
  rar-rs-napi.wasi.d.cts
  rar-rs-napi.wasi-browser.js
  wasi-path-map.cjs
  wasi-worker.mjs
  wasi-worker-browser.mjs
)

mkdir -p "$out"

mapfile -d '' -t node_files < <(find "$bindings" -type f -name '*.node' -print0 2>/dev/null | sort -z)
if (( ${#node_files[@]} != ${#EXPECTED_ADDONS[@]} )); then
  printf 'found: %s\n' "${node_files[@]:-<none>}" >&2
  fail "expected ${#EXPECTED_ADDONS[@]} native addons, found ${#node_files[@]}"
fi
for addon in "${EXPECTED_ADDONS[@]}"; do
  mapfile -d '' -t matches < <(find "$bindings" -type f -name "$addon" -print0 2>/dev/null | sort -z)
  if (( ${#matches[@]} != 1 )); then
    fail "expected exactly one $addon artifact, found ${#matches[@]}"
  fi
  cp "${matches[0]}" "$out/"
done

for name in "${WASI_FILES[@]}"; do
  if [ ! -f "$wasm/$name" ]; then
    fail "missing WASI file $wasm/$name"
  fi
done
cp "${WASI_FILES[@]/#/$wasm/}" "$out/"

# The generated loader and its types are byte-identical on every target, so the
# release ships one copy of each — and a difference means the build matrix was
# not reproducible, which is worth failing on.
copy_identical_generated_file() {
  local name="$1"
  local source
  mapfile -d '' -t files < <(find "$bindings" -type f -name "$name" -print0 2>/dev/null | sort -z)
  if (( ${#files[@]} == 0 )); then
    fail "no $name artifact was downloaded"
  fi
  source="${files[0]}"
  for candidate in "${files[@]:1}"; do
    if ! cmp -s "$source" "$candidate"; then
      fail "$name differs between $source and $candidate"
    fi
  done
  cp "$source" "$out/$name"
}
copy_identical_generated_file binding.js
copy_identical_generated_file binding.d.ts

for legal in LICENSE NOTICE THIRD_PARTY_LICENSES.md; do
  if [ ! -f "$root/$legal" ]; then
    fail "missing $root/$legal"
  fi
done
cp "$root/LICENSE" "$root/NOTICE" "$root/THIRD_PARTY_LICENSES.md" "$out/"

mapfile -d '' -t license_files < <(find "$root/LICENSES" -type f -print0 2>/dev/null | sort -z)
if (( ${#license_files[@]} == 0 )); then
  fail "no identified license-family texts found under $root/LICENSES/"
fi
for file in "${license_files[@]}"; do
  relative="${file#"$root"/LICENSES/}"
  flattened="${relative//\//__}"
  destination="$out/THIRD_PARTY_LICENSE-$flattened"
  if [[ -e "$destination" ]]; then
    fail "flattened license filename collision: $destination"
  fi
  cp "$file" "$destination"
done

# Written outside `out` so SHA-256SUMS can never hash itself.
sums=$(mktemp)
(cd "$out" && sha256sum -- *) > "$sums"
cp "$sums" "$out/SHA-256SUMS"
rm -f "$sums"

printf 'assembled %s files in %s\n' "$(find "$out" -maxdepth 1 -type f | wc -l | tr -d ' ')" "$out"
