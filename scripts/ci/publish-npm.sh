#!/usr/bin/env bash
# Publish the napi-rs payload to npm as a single package.
#
# One package carrying every platform binary, rather than a root package plus
# one package per target. `napi`'s generated loaders make that safe: `binding.js`
# tries `./<binaryName>.<platform>.node` before it falls back to a per-platform
# package, and both WASI loaders resolve the `.wasm` next to themselves
# (`rar-rs-napi.wasi.cjs` before `require.resolve('rar-rs-wasm32-wasi')`,
# `rar-rs-napi.wasi-browser.js` via `new URL('./…', import.meta.url)`). What it
# costs is bytes: every install downloads the whole set (~14 MB packed) instead
# of the one binary it can load (~1.4 MB). What it buys is one name, no
# `optionalDependencies` to keep in sync, and one registry entry to configure.
#
# The *release payload* (`collect-release-assets.sh`) is the only source of
# bytes: it is the same set the `release` job checksums, so npm serves what the
# GitHub Release serves. Every target listed in `napi.targets` must have its
# artifact in the payload, or the run fails here instead of publishing a
# package that installs but cannot load on that platform.
#
# A version already on the registry is skipped — npm owns a version number
# forever, so a failed run has to be re-runnable without hand-edits.
#
# Usage: publish-npm.sh [--dry-run] [PAYLOAD_DIR]
#
#   PAYLOAD_DIR  flattened release payload (default: release/)
#
# Environment: NPM (default `npm`), ROOT (default `.`), NPM_PUBLISH_FLAGS
# (default `--access public`; the workflow adds `--provenance`). Credentials are
# either a token in `NODE_AUTH_TOKEN` (how npm reads an injected secret) or npm's
# own OIDC exchange, which the workflow gets from `id-token: write` — this script
# handles neither, it only checks that one of them is there.
set -euo pipefail
export LC_ALL=C

dry_run=0
positional=()
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    -h | --help)
      sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      positional+=("$arg")
      ;;
  esac
done

root=${ROOT:-.}
package_dir=$root/crates/rar-napi
payload=${positional[0]:-release}

# `json()` hands a path to `node -e`, and node needs a path it can `require`:
# a relative one only works with a `./` prefix, and Git Bash's `/c/...` / `/tmp/...`
# spellings do not resolve at all. Normalise both directories once.
abspath() {
  (cd "$1" 2>/dev/null && { pwd -W 2>/dev/null || pwd; }) || printf '%s' "$1"
}
package_dir=$(abspath "$package_dir")
payload=$(abspath "$payload")
npm_bin=${NPM:-npm}
publish_flags=${NPM_PUBLISH_FLAGS:---access public}

fail() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*" >&2; else echo "!! $*" >&2; fi
  exit 1
}

# `node -p` rather than sed: package.json is JSON, and every field this script
# needs (`files`, `napi.targets`) is one parse away.
json() {
  node -e "const p=require('$1');process.stdout.write(String(p$2 ?? ''))"
}

[ -f "$package_dir/package.json" ] || fail "no package.json at $package_dir"
[ -d "$payload" ] ||
  fail "no payload directory at $payload — run collect-release-assets.sh first"

# npm authenticates two ways here: a token in `NODE_AUTH_TOKEN` (what
# `actions/setup-node` turns into an `.npmrc` line), or its own OIDC exchange,
# available whenever the job granted `id-token: write` — Actions exports
# `ACTIONS_ID_TOKEN_REQUEST_URL` for exactly that, and CI relies on it so no
# secret is stored. Failing here names the missing credential; failing at
# `npm publish` would come after the Release and read as "npm is broken". A dry
# run needs neither.
has_oidc=0
[ -n "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ] && has_oidc=1
if ((!dry_run)) && [ -z "${NODE_AUTH_TOKEN:-}" ] && [ "$has_oidc" -eq 0 ] && ! grep -q _authToken "$HOME/.npmrc" 2>/dev/null; then
  fail "no npm credentials: set NODE_AUTH_TOKEN, or grant the job id-token: write so npm can exchange an OIDC token, or put an authToken in ~/.npmrc"
fi

name=$(json "$package_dir/package.json" .name)
version=$(json "$package_dir/package.json" .version)
binary=$(json "$package_dir/package.json" .napi.binaryName)

# The package is staged in a temporary directory: `binding.js`, the WASI
# loaders and `browser.js` are build outputs (gitignored, written by
# `napi build`), and publishing from the worktree would either ship a stale
# copy or leave publish-time edits behind.
if [ -n "${NPM_STAGE_DIR:-}" ]; then
  # A seam for `test-release-scripts.sh` and for reproducing a publish by hand:
  # the staged package is otherwise deleted on exit.
  stage=$NPM_STAGE_DIR
  rm -rf "$stage"
  mkdir -p "$stage"
else
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT
  # Git Bash hands out `/tmp/...`, which its own tools resolve but node.exe does
  # not — it would look for a literal C:\tmp. `pwd -W` is the Windows spelling;
  # on Linux it does not exist and the plain path is already correct.
  stage=$(cd "$stage" && { pwd -W 2>/dev/null || pwd; })
fi
stage_pkg=$stage/$name
mkdir -p "$stage_pkg"
cp "$package_dir/package.json" "$stage_pkg/package.json"
cp "$package_dir/LICENSE" "$stage_pkg/LICENSE"

# Every entry of `files` must resolve, and the payload decides what the bytes
# are. Three exceptions: the native artifacts are globbed by `files` and copied
# from `napi.targets` below, the README is the package's own
# (`crates/rar-napi/README.md`) when present and otherwise the repository root
# README shared with the crate and the CLI, and `browser.js` is the one-line
# re-export `napi build` writes only when it compiles a target — the publish job
# never does.
while IFS= read -r entry; do
  [ -n "$entry" ] || continue
  case "$entry" in
    README.md)
      if [ -e "$package_dir/README.md" ]; then
        cp "$package_dir/README.md" "$stage_pkg/README.md"
      else
        cp "$root/README.md" "$stage_pkg/README.md"
      fi
      ;;
    browser.js)
      # Bundlers reach the WASI build through this file, so it has to point at
      # the copy inside this package: the per-platform packages are gone.
      printf "export * from './%s.wasi-browser.js'\n" "$binary" >"$stage_pkg/browser.js"
      ;;
    '*.node' | *.wasm)
      : # native artifacts, handled from napi.targets below
      ;;
    *)
      if [ -e "$payload/$entry" ]; then
        cp -R "$payload/$entry" "$stage_pkg/$entry"
      elif [ -e "$package_dir/$entry" ]; then
        cp -R "$package_dir/$entry" "$stage_pkg/$entry"
      else
        fail "files lists $entry but neither $payload nor $package_dir has it"
      fi
      ;;
  esac
done < <(json "$package_dir/package.json" ".files.join('\n')"; echo)

# The target list in package.json is the single source of truth, and the count
# is what gets checked: napi names the artifacts after its own platform spelling
# (`win32-x64-msvc`), not the Rust triple in `napi.targets`, so a hand-kept
# triple-to-platform table would be one more place to forget. One `.node` per
# native target is the invariant that actually matters — a dropped addon fails
# here instead of publishing a package that installs but cannot load. The WASI
# target is named outright, because its artifact is a `.wasm` next to the two
# worker scripts the loader spawns (ordinary `files` entries — forgetting them
# is what makes a WASI package install and then fail to spawn).
targets=$(json "$package_dir/package.json" ".napi.targets.join('\n')"; echo)
native_targets=0
want_wasm=0
while IFS= read -r target; do
  [ -n "$target" ] || continue
  case "$target" in
    wasm32-*) want_wasm=1 ;;
    *) native_targets=$((native_targets + 1)) ;;
  esac
done < <(printf '%s\n' "$targets")
artifacts=()
for candidate in "$payload/$binary".*.node; do
  [ -f "$candidate" ] || continue
  artifacts+=("$(basename "$candidate")")
done
[ "${#artifacts[@]}" -eq "$native_targets" ] ||
  fail "the payload carries ${#artifacts[@]} native addon(s) but napi.targets names $native_targets native target(s) — a build is missing"
[ "${#artifacts[@]}" -gt 0 ] || fail "napi.targets is empty — nothing to publish"
if [ "$want_wasm" -eq 1 ]; then
  [ -f "$payload/$binary.wasm32-wasi.wasm" ] ||
    fail "the payload has no $binary.wasm32-wasi.wasm for the wasm32 target"
  artifacts+=("$binary.wasm32-wasi.wasm")
fi
for artifact in "${artifacts[@]}"; do
  cp "$payload/$artifact" "$stage_pkg/$artifact"
done

# `files` has to glob the artifacts in, or npm quietly drops them and the
# package ships as metadata only. Adding the two globs here keeps the staged
# manifest in step with what was just copied, whichever tag the workflow
# checked out.
node - "$stage_pkg/package.json" <<'STUB'
const fs = require('fs')
const path = process.argv[2]
const pkg = JSON.parse(fs.readFileSync(path, 'utf8'))
const files = (pkg.files || []).filter(
  (entry) => !entry.endsWith('.node') && !entry.endsWith('.wasm'),
)
files.push('*.node', `${pkg.napi.binaryName}.wasm32-wasi.wasm`)
pkg.files = files
// One package means no per-platform packages to depend on; a leftover list
// would make npm install registries that are no longer published.
delete pkg.optionalDependencies
fs.writeFileSync(path, `${JSON.stringify(pkg, null, 2)}\n`)
STUB

# The third-party notices are the same texts the crates and the GitHub Release
# ship. npm packs a `files` entry and little else, so being listed is the whole
# mechanism — assert it here rather than noticing a license-free tarball later.
for entry in NOTICE THIRD_PARTY_LICENSES.md LICENSES; do
  case $(json "$package_dir/package.json" ".files.join('\n')") in
    *"$entry"*) ;;
    *) fail "$package_dir/package.json does not list $entry — npm would publish without it (note that the manifest comes from the tag, so an old tag cannot be re-published this way)" ;;
  esac
done

# Nothing else validates the tarball: `npm publish` would happily ship a package
# with no `.node` in it at all.
for artifact in "${artifacts[@]}"; do
  [ -f "$stage_pkg/$artifact" ] || fail "$artifact did not reach $stage_pkg"
done

if "$npm_bin" view "$name@$version" version >/dev/null 2>&1; then
  echo "skip  $name $version — already on the registry"
  echo "done: $name $version"
  exit 0
fi

if ((dry_run)); then
  echo "would publish $name $version ($stage_pkg)"
  echo "  with ${#artifacts[@]} artifact(s): ${artifacts[*]}"
else
  echo "publishing $name $version"
  (cd "$stage_pkg" && "$npm_bin" publish $publish_flags)
fi

echo "done: $name $version"