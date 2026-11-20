#!/usr/bin/env bash
# Turn commit subjects into the GitHub Release body.
#
# Commit subjects are this repo's changelog (`type(scope): summary`, see the
# `git log` history), so grouping them by prefix is enough — and unlike
# `gh release create --generate-notes` it regenerates on every run instead of
# freezing the first run's text. Docs/style/chore/test commits are counted, not
# listed: a release page that lists 200 bullets is worse than one that points at
# the changelog for them.
#
# A script rather than an inline step so the grouping, the previous-tag lookup
# and the no-previous-tag fallback are covered by
# `scripts/ci/test-release-scripts.sh` on every push.
#
# Usage: release-notes.sh TAG [OUT_FILE]
#   OUT_FILE defaults to stdout. Missing previous tag = diff against the root
#   commit's parentless history (the repo's first release).
set -euo pipefail

tag=${1:?usage: release-notes.sh TAG [OUT_FILE]}
out=${2:-}

# The previous **release** tag, not merely the newest tag: `--merged` keeps a
# re-release of an old tag from diffing against a newer one (which would yield
# an empty range and a blank release page).
prev="$(git tag --merged "$tag" --sort=-version:refname | grep -vx "$tag" | head -1 || true)"
if [ -n "$prev" ]; then
  range="$prev..$tag"
  base=$prev
else
  # First release: there is nothing to diff against, and `root..tag` would drop
  # the root commit itself — so list the whole history and link from the root.
  base="$(git rev-list --max-parents=0 HEAD)"
  range=$tag
fi

repo=${GITHUB_REPOSITORY:-}
if [ -z "$repo" ]; then
  url=$(git config --get remote.origin.url || true)
  case $url in
    git@github.com:*) repo=${url#git@github.com:}; repo=${repo%.git} ;;
    https://github.com/*) repo=${url#https://github.com/}; repo=${repo%.git} ;;
    *) repo='<repository>' ;;
  esac
fi

mapfile -t subjects < <(git log --no-merges --format='%s' "$range")

features=() fixes=() other=()
skipped=0
for subject in "${subjects[@]}"; do
  if [[ $subject =~ ^(feat|perf)(\(|:|$) ]]; then
    features+=("- $subject")
  elif [[ $subject =~ ^fix(\(|:|$) ]]; then
    fixes+=("- $subject")
  elif [[ $subject =~ ^(docs|style|chore|test)(\(|:|$) ]]; then
    skipped=$((skipped + 1))
  else
    other+=("- $subject")
  fi
done

# An empty array expands to nothing, so `$#` is 0 and no heading is printed —
# no section with a blank body.
print_section() {
  local title="$1"
  shift
  (( $# )) || return 0
  echo "### $title"
  echo
  printf '%s\n' "$@"
  echo
}

emit() {
  echo "## What's Changed"
  echo
  print_section 'Features' "${features[@]}"
  print_section 'Fixes' "${fixes[@]}"
  print_section 'Other changes' "${other[@]}"
  if (( skipped > 0 )); then
    echo "_$skipped documentation, style, chore and test commits are omitted; the full changelog is below._"
    echo
  fi
  echo "**Full Changelog**: https://github.com/$repo/compare/${base}...${tag}"
}

if [ -n "$out" ]; then
  emit > "$out"
  echo "notes for $tag (previous tag: ${prev:-none}) -> $out" >&2
else
  emit
fi
