#!/usr/bin/env bash
# Compute the version of a querymt nightly build.
#
#   nightly-version.sh <workspace-version> <UTC-timestamp-YYYYMMDDHHMMSS>  < tags
#
# Output: <base>-nightly.<timestamp>
#
# <base> is the workspace version, unless the stable tag v<workspace-version>
# already exists, in which case it is the next patch version. That keeps every
# nightly sorted above the stable it follows and below the next stable.
#
# Tags are read from stdin, one per line. Plain tag names (v0.5.2), refs
# (refs/tags/v0.5.2) and `git ls-remote --tags` output are all accepted:
#
#   git ls-remote --tags origin | scripts/release/nightly-version.sh 0.6.0 "$(date -u +%Y%m%d%H%M%S)"
set -euo pipefail

die() {
  echo "nightly-version: $*" >&2
  exit 1
}

[ "$#" -eq 2 ] || die "usage: nightly-version.sh <workspace-version> <timestamp> < tags"
workspace_version="$1"
timestamp="$2"

[[ "$workspace_version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] \
  || die "workspace version '$workspace_version' is not X.Y.Z"
major="${BASH_REMATCH[1]}"
minor="${BASH_REMATCH[2]}"
patch="${BASH_REMATCH[3]}"

[[ "$timestamp" =~ ^[0-9]{14}$ ]] || die "timestamp '$timestamp' is not YYYYMMDDHHMMSS"

tags="$(awk '{print $NF}' | sed -e 's#^refs/tags/##' -e 's#\^{}$##')"

if printf '%s\n' "$tags" | grep -Fxq "v${workspace_version}"; then
  patch=$((patch + 1))
fi

echo "${major}.${minor}.${patch}-nightly.${timestamp}"
