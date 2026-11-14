#!/usr/bin/env bash
# Stamp a build version (e.g. 0.1.1-nightly.20261114120000) into the CI
# workspace only: the [package] version in Cargo.toml and the matching
# querymt-gpui entry in Cargo.lock, so `cargo build --locked` still passes and
# the binary reports the stamped CARGO_PKG_VERSION.
#
#   stamp-version.sh <version> [Cargo.toml] [Cargo.lock]
#
# versions.yml validates the committed files; this only runs in CI checkouts.
set -euo pipefail

die() {
  echo "stamp-version: $*" >&2
  exit 1
}

[ "$#" -ge 1 ] || die "usage: stamp-version.sh <version> [Cargo.toml] [Cargo.lock]"
version="$1"
manifest="${2:-Cargo.toml}"
lockfile="${3:-Cargo.lock}"

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
  || die "version '$version' is not semver"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

awk -v v="$version" '
  /^\[/ { in_pkg = ($0 == "[package]") }
  in_pkg && !done && /^version[[:space:]]*=/ { print "version = \"" v "\""; done = 1; next }
  { print }
  END { if (!done) exit 1 }
' "$manifest" >"$tmp" || die "no [package] version in $manifest"
cat "$tmp" >"$manifest"

awk -v v="$version" '
  /^\[\[package\]\]/ { in_self = 0 }
  /^name = "querymt-gpui"$/ { in_self = 1 }
  in_self && !done && /^version = / { print "version = \"" v "\""; done = 1; next }
  { print }
  END { if (!done) exit 1 }
' "$lockfile" >"$tmp" || die "no querymt-gpui package in $lockfile"
cat "$tmp" >"$lockfile"

echo "Stamped version $version"
