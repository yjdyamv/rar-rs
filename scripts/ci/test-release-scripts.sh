#!/usr/bin/env bash
# Fixture tests for the three scripts the `release` job runs.
#
# The point is that the release payload's rules — one addon per platform, every
# WASI file present, a reproducible generated loader, no flattened-license
# collision, the release-notes grouping, the refusal to replace published bytes
# without being asked, and the npm publish order — are enforced on every push,
# not discovered by the first tag. These scripts are pure filesystem/git logic plus a stubbed `gh`, so
# seconds of fixtures here replace a nine-target rebuild.
#
# Run from anywhere: bash scripts/ci/test-release-scripts.sh
set -euo pipefail
export LC_ALL=C

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

passed=0
failed=0
ok() {
  printf 'ok   %s\n' "$1"
  passed=$((passed + 1))
}
not_ok() {
  printf 'FAIL %s\n' "$1" >&2
  failed=$((failed + 1))
}
# check <description> <needle> <haystack>
check() {
  if grep -qF -- "$2" <<<"$3"; then ok "$1"; else not_ok "$1 (missing: $2)"; fi
}
check_absent() {
  if grep -qF -- "$2" <<<"$3"; then not_ok "$1 (unexpected: $2)"; else ok "$1"; fi
}
# expect_failure <description> <command...>
expect_failure() {
  local description=$1
  shift
  if "$@" >"$tmp/out.log" 2>&1; then
    not_ok "$description (expected a failure, exited 0)"
  else
    ok "$description"
  fi
}

# ---------------------------------------------------------------------------
# release-notes.sh
# ---------------------------------------------------------------------------
g() { git -c user.name=ci -c user.email=ci@example.invalid -c commit.gpgsign=false "$@"; }
commit() { g commit -q --allow-empty -m "$1"; }

repo="$tmp/repo"
mkdir -p "$repo"
cd "$repo"
git init -q

commit 'feat(core): add thing'
commit 'fix: repair thing'
g tag v0.1.0
commit 'feat: second thing'
commit 'fix(reader): escape path'
commit 'docs: explain thing'
commit 'random work'
g tag v0.2.0
commit 'perf: speed up'
g tag v0.3.0

notes=$(GITHUB_REPOSITORY=example/rar-rs bash "$here/release-notes.sh" v0.2.0)
check 'notes: features section' '### Features' "$notes"
check 'notes: fix section' '### Fixes' "$notes"
check 'notes: other section' '### Other changes' "$notes"
check 'notes: feat listed' '- feat: second thing' "$notes"
check 'notes: scoped fix listed' '- fix(reader): escape path' "$notes"
check 'notes: unmatched subject lands in Other' '- random work' "$notes"
check 'notes: docs counted, not listed' '_1 documentation, style, chore and test commits are omitted' "$notes"
check 'notes: commit outside the range is not listed' 'compare/v0.1.0...v0.2.0' "$notes"
check_absent 'notes: range ends at the tag' 'perf: speed up' "$notes"
check_absent 'notes: unreleased work stays out' 'feat(core): add thing' "$notes"

# Re-releasing an old tag must diff against the previous tag merged into it, not
# against the newest tag (which would produce an empty range and a blank page).
notes=$(GITHUB_REPOSITORY=example/rar-rs bash "$here/release-notes.sh" v0.3.0)
check 'notes: re-release uses the merged previous tag' 'compare/v0.2.0...v0.3.0' "$notes"
check 'notes: newest range lists perf' '- perf: speed up' "$notes"

# First release: no previous tag, so the range starts at the root commit.
cd "$tmp"
mkdir -p first && cd first
git init -q
commit 'feat: only thing'
g tag v0.1.0
root_commit=$(git rev-list --max-parents=0 HEAD)
notes=$(GITHUB_REPOSITORY=example/rar-rs bash "$here/release-notes.sh" v0.1.0)
check 'notes: first release diffs from the root commit' "compare/${root_commit}...v0.1.0" "$notes"
check 'notes: first release lists its only commit' '- feat: only thing' "$notes"

# Without GITHUB_REPOSITORY the repo is inferred from the remote. Unset it
# explicitly: GitHub Actions exports GITHUB_REPOSITORY, so "not set locally" is
# not the same as "unset here", and the inherited value silently wins.
g remote add origin https://github.com/example/inferred.git
notes=$(env -u GITHUB_REPOSITORY bash "$here/release-notes.sh" v0.1.0)
check 'notes: repo inferred from origin' 'github.com/example/inferred/compare/' "$notes"

# OUT_FILE writes the file and keeps stdout quiet.
stdout=$(bash "$here/release-notes.sh" v0.1.0 "$tmp/notes.md" 2>/dev/null)
if [ -s "$tmp/notes.md" ] && [ -z "$stdout" ]; then
  ok 'notes: writes OUT_FILE and stays quiet'
else
  not_ok 'notes: writes OUT_FILE and stays quiet'
fi

# ---------------------------------------------------------------------------
# collect-release-assets.sh
# ---------------------------------------------------------------------------
addon_targets=(
  darwin-arm64
  win32-x64-msvc
  win32-ia32-msvc
  win32-arm64-msvc
  linux-x64-gnu
  linux-x64-musl
  linux-arm64-gnu
  linux-arm64-musl
  linux-arm-gnueabihf
)
wasi_files=(
  rar-rs-napi.wasm32-wasi.wasm
  rar-rs-napi.wasm32-wasi.debug.wasm
  rar-rs-napi.wasi.cjs
  rar-rs-napi.wasi.d.cts
  rar-rs-napi.wasi-browser.js
  wasi-path-map.cjs
  wasi-worker.mjs
  wasi-worker-browser.mjs
)
expected_files=$(( ${#addon_targets[@]} + ${#wasi_files[@]} + 2 + 3 + 2 + 1 ))

make_fixture() {
  local dir=$1
  rm -rf "$dir"
  mkdir -p "$dir/bindings" "$dir/wasm" "$dir/LICENSES/vendor"
  printf 'license\n' >"$dir/LICENSE"
  printf 'notice\n' >"$dir/NOTICE"
  printf 'third party\n' >"$dir/THIRD_PARTY_LICENSES.md"
  printf 'apache\n' >"$dir/LICENSES/Apache-2.0.txt"
  printf 'mit\n' >"$dir/LICENSES/vendor/MIT.txt"
  printf 'export const loader = 1;\n' >"$dir/bindings/binding.js"
  printf 'export declare const loader: 1;\n' >"$dir/bindings/binding.d.ts"
  local target
  for target in "${addon_targets[@]}"; do
    mkdir -p "$dir/bindings/$target"
    printf 'addon for %s\n' "$target" >"$dir/bindings/$target/rar-rs-napi.$target.node"
    cp "$dir/bindings/binding.js" "$dir/bindings/$target/binding.js"
    cp "$dir/bindings/binding.d.ts" "$dir/bindings/$target/binding.d.ts"
  done
  local name
  for name in "${wasi_files[@]}"; do
    printf 'wasi %s\n' "$name" >"$dir/wasm/$name"
  done
}

fixture="$tmp/assets"
make_fixture "$fixture"
bash "$here/collect-release-assets.sh" \
  "$fixture/bindings" "$fixture/wasm" "$fixture/release" "$fixture" >/dev/null

count=$(find "$fixture/release" -maxdepth 1 -type f | wc -l | tr -d ' ')
if [ "$count" = "$expected_files" ]; then
  ok "assets: complete payload ($count files)"
else
  not_ok "assets: complete payload (expected $expected_files files, found $count)"
fi

if (cd "$fixture/release" && sha256sum --quiet -c SHA-256SUMS >/dev/null); then
  ok 'assets: SHA-256SUMS verifies every shipped file'
else
  not_ok 'assets: SHA-256SUMS verifies every shipped file'
fi

if grep -qxF 'SHA-256SUMS' <(sed 's/^[0-9a-f]*  //' "$fixture/release/SHA-256SUMS"); then
  not_ok 'assets: the manifest does not hash itself'
else
  ok 'assets: the manifest does not hash itself'
fi

if [ -f "$fixture/release/THIRD_PARTY_LICENSE-vendor__MIT.txt" ]; then
  ok 'assets: nested license text is flattened'
else
  not_ok 'assets: nested license text is flattened'
fi

if [ -f "$fixture/release/binding.js" ]; then
  ok 'assets: one copy of the generated loader'
else
  not_ok 'assets: one copy of the generated loader'
fi

# Negative cases: each rule that protects a release must actually fire.
make_fixture "$fixture"
rm "$fixture/bindings/linux-arm-gnueabihf/rar-rs-napi.linux-arm-gnueabihf.node"
expect_failure 'assets: missing addon fails' bash "$here/collect-release-assets.sh" \
  "$fixture/bindings" "$fixture/wasm" "$fixture/release" "$fixture"

make_fixture "$fixture"
mkdir -p "$fixture/bindings/extra"
cp "$fixture/bindings/linux-x64-gnu/rar-rs-napi.linux-x64-gnu.node" \
  "$fixture/bindings/extra/rar-rs-napi.linux-x64-gnu.node"
expect_failure 'assets: duplicated addon fails' bash "$here/collect-release-assets.sh" \
  "$fixture/bindings" "$fixture/wasm" "$fixture/release" "$fixture"

make_fixture "$fixture"
printf 'export const loader = 2;\n' >"$fixture/bindings/darwin-arm64/binding.js"
expect_failure 'assets: loader that differs between targets fails' bash "$here/collect-release-assets.sh" \
  "$fixture/bindings" "$fixture/wasm" "$fixture/release" "$fixture"

make_fixture "$fixture"
rm "$fixture/wasm/wasi-worker.mjs"
expect_failure 'assets: missing WASI file fails' bash "$here/collect-release-assets.sh" \
  "$fixture/bindings" "$fixture/wasm" "$fixture/release" "$fixture"

make_fixture "$fixture"
rm -rf "$fixture/LICENSES"
expect_failure 'assets: missing license tree fails' bash "$here/collect-release-assets.sh" \
  "$fixture/bindings" "$fixture/wasm" "$fixture/release" "$fixture"

make_fixture "$fixture"
printf 'dupes\n' >"$fixture/LICENSES/vendor__MIT.txt"
expect_failure 'assets: flattened license collision fails' bash "$here/collect-release-assets.sh" \
  "$fixture/bindings" "$fixture/wasm" "$fixture/release" "$fixture"

# ---------------------------------------------------------------------------
# publish-crates.sh: the sparse index path is a URL built from the crate name,
# and the skip rule is what makes a re-leased tag a no-op instead of a failure
# on the versions that are already up. A `file://` index exercises both
# branches without a network or a token.
# ---------------------------------------------------------------------------
publish="$here/publish-crates.sh"
check 'publish: a long name splits into two directories' \
  'ra/r-/rar-rs' "$(bash "$publish" --index-path rar-rs)"
check 'publish: a three character name nests under its first letter' \
  '3/a/abc' "$(bash "$publish" --index-path abc)"

# A stub `curl` stands in for the registry: only the names it was given a file
# for exist, so the dry run proves both branches without a network or a token.
fake_index="$tmp/fake-index"
stub_bin="$tmp/bin"
mkdir -p "$fake_index" "$stub_bin"
tree_version=$(sed -n '/^\[package\]/,/^\[/{s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p}' \
  "$here/../../crates/rar/Cargo.toml" | head -1)
printf '{"name":"rar-rs","vers":"%s"}\n' "$tree_version" >"$fake_index/rar-rs"
cat >"$stub_bin/curl" <<'STUB'
#!/usr/bin/env bash
# Emulates `curl -fsS <url>` against $FAKE_INDEX: the last path segment is the
# crate name, and a missing file is the sparse index's 404.
for arg in "$@"; do url=$arg; done
if [ -f "$FAKE_INDEX/${url##*/}" ]; then cat "$FAKE_INDEX/${url##*/}"; else exit 22; fi
STUB
chmod +x "$stub_bin/curl"
dry=$(PATH="$stub_bin:$PATH" FAKE_INDEX="$fake_index" bash "$publish" --dry-run)
check 'publish: skips the crate already in the index' "skip  rar-rs $tree_version" "$dry"
check 'publish: still publishes the crates that are missing' 'would publish rar-cli' "$dry"

# The list is data: a crate added to the workspace but not to the script would
# compile, test and release, and never reach the registry — the failure is
# silent until someone tries to install the new name.
script_crates=$(sed -n 's/^CRATES=(\(.*\))$/\1/p' "$publish" | tr ' ' '\n' | sort)
workspace_crates=$(for manifest in "$here"/../../crates/*/Cargo.toml; do
  sed -n '/^\[package\]/,/^\[/{s/^name[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p}' "$manifest"
done | sort)
if [ "$script_crates" = "$workspace_crates" ]; then
  ok 'publish: the crate list covers every workspace crate'
else
  not_ok "publish: crate list is [$script_crates], workspace has [$workspace_crates]"
fi

# ---------------------------------------------------------------------------
# publish-npm.sh: the platform packages are derived from package.json and
# filled from the payload, so what a fixture can prove is exactly what a real
# tag would get wrong — a target publishing without its binary, the root
# package's `optionalDependencies` missing a platform, a notice dropped from
# `files`, or the root going up before the packages it names.
# ---------------------------------------------------------------------------
npm_publish="$here/publish-npm.sh"
# `json()` in the script reads package.json with node, and node.exe does not
# resolve the `/tmp/...` spelling Git Bash hands out — give the fixture the
# Windows form here and the plain path on Linux.
fixture=$(cd "$tmp" && { pwd -W 2>/dev/null || pwd; })/npm
payload="$fixture/payload"
npmdirs="$fixture/npm-dirs"
root="$fixture/root"
npm_package="$root/crates/rar-napi"
mkdir -p "$payload" "$npmdirs" "$npm_package"

# mkpkg <dir> <name> <version> <main> <file>...
mkpkg() {
  local dir=$1 name=$2 version=$3 main=$4 sep=''
  shift 4
  mkdir -p "$dir"
  {
    printf '{\n  "name": "%s",\n  "version": "%s",\n  "main": "%s",\n  "files": [' "$name" "$version" "$main"
    for file in "$@"; do
      printf '%s"%s"' "$sep" "$file"
      sep=', '
    done
    printf ']\n}\n'
  } >"$dir/package.json"
  printf '# %s\n' "$name" >"$dir/README.md"
}

mkpkg "$npm_package" rar-rs 1.2.3 rar-rs.js \
  rar-rs.js binding.js rar-rs-napi.wasm32-wasi.wasm README.md browser.js \
  NOTICE THIRD_PARTY_LICENSES.md LICENSES
mkpkg "$npmdirs/darwin-arm64" rar-rs-darwin-arm64 1.2.3 \
  rar-rs-napi.darwin-arm64.node rar-rs-napi.darwin-arm64.node
mkpkg "$npmdirs/linux-x64-gnu" rar-rs-linux-x64-gnu 1.2.3 \
  rar-rs-napi.linux-x64-gnu.node rar-rs-napi.linux-x64-gnu.node
mkpkg "$npmdirs/wasm32-wasi" rar-rs-wasm32-wasi 1.2.3 rar-rs-napi.wasi.cjs \
  rar-rs-napi.wasm32-wasi.wasm rar-rs-napi.wasi.cjs

for file in rar-rs.js binding.js LICENSE NOTICE THIRD_PARTY_LICENSES.md; do
  printf '%s\n' "$file" >"$npm_package/$file"
done
mkdir -p "$npm_package/LICENSES"
printf 'MIT\n' >"$npm_package/LICENSES/MIT.txt"
printf '# rar-rs\n\nROOT-README marker.\n' >"$root/README.md"
for file in rar-rs-napi.darwin-arm64.node rar-rs-napi.linux-x64-gnu.node \
  rar-rs-napi.wasm32-wasi.wasm rar-rs-napi.wasi.cjs binding.js; do
  printf '%s\n' "$file" >"$payload/$file"
done

# The stub answers the two commands the script makes: `view` decides the skip
# rule from $FAKE_NPM_PUBLISHED (one `name@version` per line), and `publish`
# records which package directory it ran in, so the order is observable.
cat >"$stub_bin/npm" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in
  view) grep -qxF -- "$2" "$FAKE_NPM_PUBLISHED" 2>/dev/null ;;
  publish)
    grep -o '"name": *"[^"]*"' package.json |
      sed 's/.*"\([^"]*\)"$/\1/' >>"$FAKE_NPM_LOG"
    ;;
  *) exit 1 ;;
esac
STUB
chmod +x "$stub_bin/npm"

published="$tmp/npm-published"
log="$tmp/npm-log"
stage="$fixture/stage"
: >"$published"
: >"$log"
run_npm() {
  PATH="$stub_bin:$PATH" ROOT="$root" NPM_STAGE_DIR="$stage" \
    NODE_AUTH_TOKEN=fixture-token HOME="$tmp/empty-home" \
    FAKE_NPM_PUBLISHED="$published" FAKE_NPM_LOG="$log" \
    bash "$npm_publish" "$payload" "$npmdirs" "$@"
}
mkdir -p "$tmp/empty-home"

plan=$(run_npm --dry-run)
check 'publish-npm: the dry run plans each platform package' \
  'would publish rar-rs-darwin-arm64 1.2.3' "$plan"
check 'publish-npm: the dry run ends with the root package' \
  'would publish rar-rs 1.2.3' "$(tail -2 <<<"$plan" | head -1)"
check_absent 'publish-npm: the dry run publishes nothing' rar-rs "$(cat "$log")"

run_npm >"$tmp/npm.log"
published_count=$(wc -l <"$log" | tr -d ' ')
if [ "$published_count" = 4 ]; then
  ok 'publish-npm: publishes four packages, one per platform plus the root'
else
  not_ok "publish-npm: published [$published_count] packages, expected 4"
fi
check 'publish-npm: the root package goes last' rar-rs "$(tail -1 "$log")"
check 'publish-npm: the WASI package is published too' rar-rs-wasm32-wasi \
  "$(tr '\n' ' ' <"$log")"
check 'publish-npm: optionalDependencies name each platform package' \
  '"rar-rs-linux-x64-gnu": "1.2.3"' "$(cat "$stage/rar-rs/package.json")"
check 'publish-npm: the README comes from the repository root' ROOT-README \
  "$(cat "$stage/rar-rs/README.md")"
check 'publish-npm: browser.js re-exports the WASI package' \
  "export * from 'rar-rs-wasm32-wasi'" "$(cat "$stage/rar-rs/browser.js")"
check 'publish-npm: the notices travel with the root package' NOTICE \
  "$(ls "$stage/rar-rs")"

# A version already on the registry is a skip, not a failure: npm owns a
# version number forever, so re-running a half-finished publish has to work.
printf 'rar-rs-linux-x64-gnu@1.2.3\n' >"$published"
: >"$log"
skipped=$(run_npm)
check 'publish-npm: skips a version already on the registry' \
  'skip  rar-rs-linux-x64-gnu 1.2.3' "$skipped"
check_absent 'publish-npm: a skipped package is not published' \
  rar-rs-linux-x64-gnu "$(cat "$log")"
: >"$published"

# No token and no `~/.npmrc` must fail before the first publish, naming the
# secret: npm can only take a token for a *new* package name, so the
# alternative is a bare `ENEEDAUTH` after the Release has gone up.
expect_failure 'publish-npm: a run without credentials fails' \
  env -u NODE_AUTH_TOKEN HOME="$tmp/empty-home" PATH="$stub_bin:$PATH" \
  ROOT="$root" bash "$npm_publish" "$payload" "$npmdirs"
check 'publish-npm: a run without credentials names the secret' \
  'no npm credentials' "$(cat "$tmp/out.log")"

# The payload is the authority: a target whose binary is not in it must fail
# here, not publish a package that cannot be loaded. A real run fills the
# directories in place, so drop what the earlier run left behind first — in CI
# `napi create-npm-dirs` always hands over fresh ones.
rm -f "$npmdirs/darwin-arm64/rar-rs-napi.darwin-arm64.node"
mv "$payload/rar-rs-napi.darwin-arm64.node" "$tmp/held-addon.node"
expect_failure 'publish-npm: a target without its artifact fails' run_npm
mv "$tmp/held-addon.node" "$payload/rar-rs-napi.darwin-arm64.node"

sed -i.bak 's/"NOTICE", //' "$npm_package/package.json"
expect_failure 'publish-npm: a files list without the notices fails' run_npm
mv "$npm_package/package.json.bak" "$npm_package/package.json"

# ---------------------------------------------------------------------------
# The two lists that have to agree: what the build matrix produces and what the
# release insists on. Editing one without the other is the failure mode this
# whole file exists to catch, so check it here rather than in a comment.
# ---------------------------------------------------------------------------
matrix_addons=$(grep -o "addon: '[^']*'" "$here/build-matrix.js" | sed "s/addon: '//; s/'$//" | sort)
release_addons=$(grep -o 'rar-rs-napi\.[a-z0-9-]*\.node' "$here/collect-release-assets.sh" | sort -u)
if [ "$matrix_addons" = "$release_addons" ]; then
  ok 'contract: the build matrix ships exactly the addons the release expects'
else
  not_ok 'contract: the build matrix and the release addon lists disagree'
  diff <(echo "$release_addons") <(echo "$matrix_addons") >&2 || true
fi

smoke_entries=$(grep -c '^    smoke: true' "$here/build-matrix.js" || true)
if [ "$smoke_entries" = "1" ]; then
  ok 'contract: exactly one smoke entry for pull requests'
else
  not_ok "contract: expected exactly one smoke entry, found $smoke_entries"
fi

printf '\n%d passed, %d failed\n' "$passed" "$failed"
(( failed == 0 ))
