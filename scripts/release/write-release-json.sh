#!/usr/bin/env bash
# Write the release.json manifest published with every querymt release.
#
#   write-release-json.sh <version> <stable|nightly> <commit-sha> <output-file>
#
# Consumers (querymt-desktop) read the version and commit from this file
# instead of parsing asset names or release notes.
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: write-release-json.sh <version> <stable|nightly> <commit-sha> <output-file>" >&2
  exit 1
fi

version="$1"
channel="$2"
commit="$3"
output="$4"

case "$channel" in
  stable | nightly) ;;
  *)
    echo "write-release-json: channel must be stable or nightly, got '$channel'" >&2
    exit 1
    ;;
esac

if [[ ! "$commit" =~ ^[0-9a-f]{40}$ ]]; then
  echo "write-release-json: commit must be a full 40-character SHA, got '$commit'" >&2
  exit 1
fi

published_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

jq -n \
  --arg version "$version" \
  --arg channel "$channel" \
  --arg commit "$commit" \
  --arg published_at "$published_at" \
  '{version: $version, channel: $channel, commit: $commit, published_at: $published_at}' \
  > "$output"
