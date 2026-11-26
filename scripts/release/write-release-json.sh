#!/usr/bin/env bash
# Write the release.json manifest published with every querymt release.
#
#   write-release-json.sh <version> <stable|nightly> <commit-sha> <output-file> \
#     [--desktop-dir <dir> --version-name <vX.Y.Z|nightly>]
#
# Consumers (querymt-desktop) read the version and commit from this file
# instead of parsing asset names or release notes.
#
# With --desktop-dir and --version-name the manifest also gets `desktop_updates`:
# for each platform key QueryMT Desktop can update itself on, the file name,
# size and SHA-256 of the installer in <dir>
# (QueryMT-Desktop-<version-name>-<platform>). The script fails when any of
# those files is missing. The two flags go together.
set -euo pipefail

usage="usage: write-release-json.sh <version> <stable|nightly> <commit-sha> <output-file> [--desktop-dir <dir> --version-name <name>]"
if [ "$#" -lt 4 ]; then
  echo "$usage" >&2
  exit 1
fi

version="$1"
channel="$2"
commit="$3"
output="$4"
shift 4

desktop_dir=""
version_name=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --desktop-dir)
      [ "$#" -ge 2 ] || { echo "write-release-json: --desktop-dir needs a value" >&2; exit 1; }
      desktop_dir="$2"
      shift 2
      ;;
    --version-name)
      [ "$#" -ge 2 ] || { echo "write-release-json: --version-name needs a value" >&2; exit 1; }
      version_name="$2"
      shift 2
      ;;
    *)
      echo "$usage" >&2
      exit 1
      ;;
  esac
done

if { [ -n "$desktop_dir" ] && [ -z "$version_name" ]; } || { [ -z "$desktop_dir" ] && [ -n "$version_name" ]; }; then
  echo "write-release-json: --desktop-dir and --version-name go together" >&2
  exit 1
fi

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

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Platform key -> installer suffix. The key is what the app looks up; the file
# is what collect-bundles.sh names the installer (QueryMT-Desktop-<name>-<suffix>).
desktop_platforms=(
  "darwin-aarch64:macos-aarch64.app.tar.gz"
  "darwin-x86_64:macos-x86_64.app.tar.gz"
  "windows-x86_64:windows-x64-setup.exe"
  "linux-x86_64:linux-x86_64.AppImage"
  "linux-aarch64:linux-aarch64.AppImage"
)

desktop_updates='null'
if [ -n "$desktop_dir" ]; then
  desktop_updates='{}'
  for entry in "${desktop_platforms[@]}"; do
    key="${entry%%:*}"
    file="QueryMT-Desktop-${version_name}-${entry#*:}"
    path="$desktop_dir/$file"
    if [ ! -f "$path" ]; then
      echo "write-release-json: desktop update asset for $key is missing: $path" >&2
      exit 1
    fi
    size="$(wc -c < "$path" | tr -d ' ')"
    sha="$(sha256_of "$path")"
    desktop_updates="$(jq -n \
      --argjson acc "$desktop_updates" \
      --arg key "$key" --arg file "$file" --argjson size "$size" --arg sha "$sha" \
      '$acc + {($key): {file: $file, size: $size, sha256: $sha}}')"
  done
fi

jq -n \
  --arg version "$version" \
  --arg channel "$channel" \
  --arg commit "$commit" \
  --arg published_at "$published_at" \
  --argjson desktop_updates "$desktop_updates" \
  '{version: $version, channel: $channel, commit: $commit, published_at: $published_at}
   + (if $desktop_updates == null then {} else {desktop_updates: $desktop_updates} end)' \
  > "$output"
