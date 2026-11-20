#!/usr/bin/env bash
# Fixture tests for the three scripts the `release` job runs.
#
# The point is that the release payload's rules — one addon per platform, every
# WASI file present, a reproducible generated loader, no flattened-license
# collision, the release-notes grouping, and the refusal to replace published
# bytes without being asked — are enforced on every push, not discovered by the
# first tag. These scripts are pure filesystem/git logic plus a stubbed `gh`, so
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
# publish-release.sh: what a re-run of `release` is allowed to do, driven through
# a stub `gh`. The workflow's `force_assets` input carries the intent; these cases
# are the mechanism honouring it — a differing asset is refused unless the input
# is true, because anything pinning its hash (smart-archiver does) breaks the
# moment it changes silently.
# ---------------------------------------------------------------------------
stubbin=$tmp/stubbin
mkdir -p "$stubbin"
cat >"$stubbin/gh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$GH_STUB_LOG"
case "$1 $2" in
  "release view") [ "${GH_STUB_VIEW:-present}" = present ] && exit 0 || exit 1 ;;
  "api repos/"*) cat "${GH_STUB_DIGESTS:-/dev/null}" ;;
  "release create") exit 0 ;;
  "release edit") exit 0 ;;
  "release upload") exit 0 ;;
esac
exit 0
STUB
chmod +x "$stubbin/gh"

pub=$tmp/pub
mkdir -p "$pub/assets" "$tmp/digests"
printf 'alpha\n' >"$pub/assets/alpha.node"
printf 'beta\n' >"$pub/assets/beta.node"
printf 'gamma\n' >"$pub/assets/gamma.node"
printf 'notes\n' >"$pub/notes.md"

# digest_of <file> -> its sha256, the way the script computes it.
digest_of() { sha256sum -- "$1" | cut -d' ' -f1; }
# write_digests <file> <name:sum>... -> the `gh api` output the stub replays.
# A name that is absent from the file is an asset the release does not have.
write_digests() {
  local out=$1
  shift
  : >"$out"
  local spec
  for spec in "$@"; do
    printf '%s\t%s\n' "${spec%%:*}" "${spec#*:}" >>"$out"
  done
}

zeros=$(printf '%064d' 0)
matching=$tmp/digests/matching.tsv
write_digests "$matching" \
  "alpha.node:$(digest_of "$pub/assets/alpha.node")" \
  "beta.node:$(digest_of "$pub/assets/beta.node")" \
  "gamma.node:$(digest_of "$pub/assets/gamma.node")"

pub_log=$tmp/publish.log
# publish_try <view> <digests-file> <force> [assets-dir] [notes-file]
# Leaves the combined output in $out and the exit status in $rc.
publish_try() {
  local view=$1 digests=$2 force=$3
  local assets=${4:-$pub/assets} notes=${5:-$pub/notes.md}
  : >"$pub_log"
  if out=$(GH_STUB_LOG=$pub_log GH_STUB_VIEW=$view GH_STUB_DIGESTS=$digests \
    GITHUB_REPOSITORY=example/rar-rs FORCE_ASSETS=$force \
    PATH="$stubbin:$PATH" bash "$here/publish-release.sh" v1 "$notes" "$assets" 2>&1); then
    rc=0
  else
    rc=$?
  fi
}

publish_try absent "$matching" ''
if [ "$rc" -eq 0 ]; then ok 'publish: creates the release when it does not exist'; else
  not_ok "publish: creates a missing release (rc=$rc: $out)"
fi
check 'publish: create carries the payload' 'gamma.node' "$(cat "$pub_log")"

publish_try present "$matching" ''
if [ "$rc" -eq 0 ]; then ok 'publish: a matching release is left alone'; else
  not_ok "publish: matching release (rc=$rc: $out)"
fi
check_absent 'publish: nothing is uploaded when the payload matches' 'release upload' "$(cat "$pub_log")"

repair=$tmp/digests/repair.tsv
write_digests "$repair" \
  "alpha.node:$(digest_of "$pub/assets/alpha.node")" \
  "beta.node:$(digest_of "$pub/assets/beta.node")"
publish_try present "$repair" ''
log=$(cat "$pub_log")
if [ "$rc" -eq 0 ]; then ok 'publish: a half-finished release is completed'; else
  not_ok "publish: repair run (rc=$rc: $out)"
fi
check 'publish: uploads only the missing asset' 'gamma.node' "$log"
check_absent 'publish: leaves the matching assets alone' 'alpha.node' "$log"

differing=$tmp/digests/differing.tsv
write_digests "$differing" \
  "alpha.node:$(digest_of "$pub/assets/alpha.node")" \
  "beta.node:$(digest_of "$pub/assets/beta.node")" \
  "gamma.node:$zeros"
publish_try present "$differing" ''
if [ "$rc" -ne 0 ]; then ok 'publish: a differing asset is refused'; else
  not_ok 'publish: a differing asset is refused (exited 0)'
fi
check 'publish: the refusal names the asset' 'gamma.node' "$out"
check 'publish: the refusal names the override' 'FORCE_ASSETS=true' "$out"

publish_try present "$differing" true
if [ "$rc" -eq 0 ]; then ok 'publish: force_assets=true allows the replacement'; else
  not_ok "publish: forced replacement (rc=$rc: $out)"
fi
check 'publish: the forced run uploads the asset' 'gamma.node' "$(cat "$pub_log")"

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
