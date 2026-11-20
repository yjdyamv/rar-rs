#!/usr/bin/env bash
# Publish the napi-rs payload to npm: nine platform packages and the WASI one,
# then the root package that names them in `optionalDependencies`.
#
# The platform packages are not checked in. `napi create-npm-dirs` derives one
# per target from `crates/rar-napi/package.json` — name, version, `os`/`cpu`
# and `libc`, and the WASI flavour's runtime dependencies — so the package
# names cannot drift from the targets, and the version has one source. This
# script fills those directories from the *release payload*
# (`collect-release-assets.sh`), so what npm serves is byte-for-byte what the
# GitHub Release serves, and a missing addon fails here instead of shipping a
# package that cannot load on someone's machine.
#
# Order is not cosmetic: npm installs a root package's `optionalDependencies`
# right after the root resolves, so publishing the root before the platform
# packages leaves a window where `npm i rar-rs` installs nothing that can load.
# A version that is already on the registry is skipped — npm owns a version
# number forever, so a failed run has to be re-runnable without hand-edits.
#
# Usage: publish-npm.sh [--dry-run] [PAYLOAD_DIR] [NPM_DIR]
#
#   PAYLOAD_DIR  flattened release payload (default: release/)
#   NPM_DIR      directories created by `napi create-npm-dirs`
#                (default: crates/rar-napi/npm)
#
# Environment: NPM (default `npm`), ROOT (default `.`),
# NPM_PUBLISH_FLAGS (default `--access public`; the workflow adds
# `--provenance`). The token comes from `NODE_AUTH_TOKEN`, which is how npm
# itself reads an injected secret — this script never handles it.
set -euo pipefail
export LC_ALL=C

dry_run=0
positional=()
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    -h | --help)
      sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'
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
npm_dir=${positional[1]:-$package_dir/npm}
npm_bin=${NPM:-npm}
publish_flags=${NPM_PUBLISH_FLAGS:---access public}

fail() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*" >&2; else echo "!! $*" >&2; fi
  exit 1
}

# `node -p` rather than sed: package.json is JSON, and every field this script
# needs (`files`, `main`, `optionalDependencies`) is one parse away.
json() {
  node -e "const p=require('$1');process.stdout.write(String(p$2 ?? ''))"
}

[ -f "$package_dir/package.json" ] || fail "no package.json at $package_dir"
[ -d "$npm_dir" ] ||
  fail "no $npm_dir — run \`napi create-npm-dirs\` in $package_dir first"
[ -d "$payload" ] ||
  fail "no payload directory at $payload — run collect-release-assets.sh first"

# npm reads the token from `NODE_AUTH_TOKEN` (what `actions/setup-node` turns
# into an `.npmrc` line) or from `~/.npmrc`. Failing here names the missing
# secret; failing at the first `npm publish` would come after the Release and
# read as "npm is broken". A dry run needs none of this.
if ((!dry_run)) && [ -z "${NODE_AUTH_TOKEN:-}" ] && ! grep -q _authToken "$HOME/.npmrc" 2>/dev/null; then
  fail "no npm credentials: set NODE_AUTH_TOKEN (and note that a *new* package name cannot be published over OIDC)"
fi

name=$(json "$package_dir/package.json" .name)
version=$(json "$package_dir/package.json" .version)

# Each platform package is filled from its own `files` list, so a target added
# to `napi.targets` without a matching artifact fails on the missing `main`
# below rather than publishing an empty package.
main_of() { json "$1/package.json" .main; }

for dir in "$npm_dir"/*/; do
  dir=${dir%/}
  has_files=$(json "$dir/package.json" .files)
  [ -n "$has_files" ] || fail "$dir/package.json has no files list"
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    # The payload is authoritative: it is the artifact the `release` job
    # checksums, so a stale copy in the worktree must not win.
    if [ -f "$payload/$entry" ]; then
      cp "$payload/$entry" "$dir/$entry"
    elif [ -f "$package_dir/$entry" ]; then
      cp "$package_dir/$entry" "$dir/$entry"
    fi
  done < <(json "$dir/package.json" ".files.join('\n')"; echo)
  main=$(main_of "$dir")
  [ -n "$main" ] || fail "$dir/package.json has no main"
  [ -f "$dir/$main" ] || fail "$dir is missing $main — the payload has no artifact for it"
  # npm redirects a bare `LICENSE` into the tarball regardless of `files`, so
  # the notice travels with every package, not just the root one.
  cp "$package_dir/LICENSE" "$dir/LICENSE"
done

# The root package is staged in a temporary directory: `binding.js`, the WASI
# loaders and `browser.js` are build outputs (gitignored, produced by
# `napi build`), and publicating from the worktree would either ship a stale
# copy or leave publish-time edits behind.
if [ -n "${NPM_STAGE_DIR:-}" ]; then
  # A seam for `test-release-scripts.sh` and for reproducing a publish by hand:
  # the staged root package is otherwise deleted on exit.
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
# are. `README.md` and `browser.js` are the two exceptions: the README lives at
# the repository root (one README for the crate, the CLI and npm), and
# `browser.js` is the one-line re-export `napi build` writes only when it
# compiles a target — the publish job never does.
while IFS= read -r entry; do
  [ -n "$entry" ] || continue
  case "$entry" in
    README.md)
      cp "$root/README.md" "$stage_pkg/README.md"
      ;;
    browser.js)
      wasm_pkg=""
      for dir in "$npm_dir"/*/; do
        case $(json "${dir%/}/package.json" .name) in
          *-wasm32-*) wasm_pkg=$(json "${dir%/}/package.json" .name) ;;
        esac
      done
      [ -n "$wasm_pkg" ] || fail "no wasm32 package in $npm_dir to re-export from"
      printf "export * from '%s'\n" "$wasm_pkg" >"$stage_pkg/browser.js"
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

# The third-party notices are the same texts the crates and the GitHub Release
# ship. npm packs a `files` entry and little else, so being listed is the whole
# mechanism — assert it here rather than noticing a license-free tarball later.
for entry in NOTICE THIRD_PARTY_LICENSES.md LICENSES; do
  case $(json "$package_dir/package.json" ".files.join('\n')") in
    *"$entry"*) ;;
    *) fail "files does not list $entry — npm would publish without it" ;;
  esac
done

# The root's `optionalDependencies` are exactly the platform packages this run
# created, at this version — derived rather than listed, because a hand-kept
# list is the same failure mode as a hand-kept target list.
node - "$stage_pkg/package.json" "$npm_dir" <<'STUB'
const fs = require('fs')
const [pkgPath, dir] = process.argv.slice(2)
const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'))
const optional = {}
for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
  if (!entry.isDirectory()) continue
  const path = `${dir}/${entry.name}/package.json`
  if (!fs.existsSync(path)) continue
  optional[JSON.parse(fs.readFileSync(path, 'utf8')).name] = pkg.version
}
pkg.optionalDependencies = { ...(pkg.optionalDependencies || {}), ...optional }
fs.writeFileSync(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`)
STUB

# Platform packages first, sorted so a dry run's plan is stable, root last.
targets=()
for dir in "$npm_dir"/*/; do targets+=("${dir%/}"); done
targets+=("$stage_pkg")

for dir in "${targets[@]}"; do
  pkg_name=$(json "$dir/package.json" .name)
  if "$npm_bin" view "$pkg_name@$version" version >/dev/null 2>&1; then
    echo "skip  $pkg_name $version — already on the registry"
    continue
  fi
  if ((dry_run)); then
    echo "would publish $pkg_name $version ($dir)"
    continue
  fi
  echo "publishing $pkg_name $version"
  (cd "$dir" && "$npm_bin" publish $publish_flags)
done

echo "done: $name $version"