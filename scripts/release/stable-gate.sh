#!/usr/bin/env bash
# Gate a querymt stable release.
#
#   stable-gate.sh <tag> <workspace-version> <tag-commit> <nightly-release.json | none>
#
# Run from a checkout that has the full history. Fails (non-zero, message on
# stderr) when:
#
#   1. the tag does not match the workspace version, or
#   2. the tag version sorts above the published nightly and the tag commit does
#      not contain the nightly's commit.
#
# Why (2): a stable with a higher version but older code than the published
# nightly would sort above it, and users on the nightly would be offered it as
# an upgrade while it carries fewer database migrations than they already ran.
# Version order alone cannot detect that; ancestry can.
#
# `semver(tag) > nightly` means tag >= base(nightly): a nightly
# `X.Y.Z-nightly.*` sorts below the stable `X.Y.Z`. Patch releases on an older
# line (v0.5.3 while the nightly is 0.6.0-nightly.*) sort below the nightly and
# pass without an ancestry check.
#
# The last argument is a path to the nightly's release.json, or `none` when no
# nightly release exists yet.
set -euo pipefail

die() {
  echo "::error::stable-gate: $*" >&2
  exit 1
}

[ "$#" -eq 4 ] || die "usage: stable-gate.sh <tag> <workspace-version> <tag-commit> <nightly-release.json|none>"
tag="$1"
workspace_version="$2"
tag_commit="$3"
nightly_json="$4"

[[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]] || die "tag '$tag' is not vX.Y.Z"
tag_version="${BASH_REMATCH[1]}"

if [ "$tag_version" != "$workspace_version" ]; then
  die "tag $tag does not match the workspace version $workspace_version; bump [workspace.package].version or retag"
fi

if [ "$nightly_json" = "none" ]; then
  echo "stable-gate: no published nightly release; nothing to compare against"
  exit 0
fi

[ -f "$nightly_json" ] || die "nightly release.json '$nightly_json' not found"

nightly_version="$(jq -er '.version' "$nightly_json")" || die "nightly release.json has no version"
nightly_commit="$(jq -er '.commit' "$nightly_json")" || die "nightly release.json has no commit"

[[ "$nightly_version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-nightly\.[0-9]+$ ]] \
  || die "nightly version '$nightly_version' is not X.Y.Z-nightly.<timestamp>"
nightly_base="${BASH_REMATCH[1]}"

# Returns 0 when $1 >= $2 (both X.Y.Z).
version_ge() {
  local IFS=.
  local -a a=($1) b=($2)
  local i
  for i in 0 1 2; do
    if ((a[i] > b[i])); then return 0; fi
    if ((a[i] < b[i])); then return 1; fi
  done
  return 0
}

if ! version_ge "$tag_version" "$nightly_base"; then
  echo "stable-gate: $tag_version sorts below the published nightly $nightly_version; no ancestry check needed"
  exit 0
fi

if git merge-base --is-ancestor "$nightly_commit" "$tag_commit" 2>/dev/null; then
  echo "stable-gate: $tag ($tag_commit) contains the published nightly $nightly_version ($nightly_commit)"
  exit 0
fi

die "$tag ($tag_commit) does not contain the commit of the published nightly $nightly_version ($nightly_commit). Releasing it would offer users on the nightly a higher version with older code. Tag a commit that contains $nightly_commit, or wait for a newer nightly."
