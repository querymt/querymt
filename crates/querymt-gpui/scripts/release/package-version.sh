#!/usr/bin/env bash
# Print the [package] version from Cargo.toml.
#
#   package-version.sh [path/to/Cargo.toml]
#
# The counterpart of querymt's workspace-version.sh: this crate is a single
# package, not a workspace.
set -euo pipefail

manifest="${1:-Cargo.toml}"

version="$(
  awk '
    /^\[/ { in_pkg = ($0 == "[package]") }
    in_pkg && /^version[[:space:]]*=/ {
      gsub(/^version[[:space:]]*=[[:space:]]*"/, "")
      gsub(/".*$/, "")
      print
      exit
    }
  ' "$manifest"
)"

if [ -z "$version" ]; then
  echo "package-version: no [package] version in $manifest" >&2
  exit 1
fi

echo "$version"
