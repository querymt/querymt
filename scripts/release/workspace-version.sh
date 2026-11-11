#!/usr/bin/env bash
# Print the [workspace.package] version from the root Cargo.toml.
#
#   workspace-version.sh [path/to/Cargo.toml]
set -euo pipefail

manifest="${1:-Cargo.toml}"

version="$(
  awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]") }
    in_pkg && /^version[[:space:]]*=/ {
      gsub(/^version[[:space:]]*=[[:space:]]*"/, "")
      gsub(/".*$/, "")
      print
      exit
    }
  ' "$manifest"
)"

if [ -z "$version" ]; then
  echo "workspace-version: no [workspace.package] version in $manifest" >&2
  exit 1
fi

echo "$version"
