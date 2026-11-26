#!/usr/bin/env bash
# Tests for the desktop app's bundle collection (collect-bundles.sh), using fake
# inputs (no real build). Run by the workspace's scripts/release/test-release-scripts.sh.
#
#   crates/querymt-gpui/scripts/release/test-release-scripts.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

expect_eq() { # name expected actual
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: expected '$2', got '$3'"; fi
}

# --- collect-bundles.sh -------------------------------------------------------
script="$here/collect-bundles.sh"
dist="$tmp/dist"
P=QueryMT-Desktop
touch_files() { # dir file...
  local dir="$1"; shift
  mkdir -p "$dir"
  for f in "$@"; do echo "$f" > "$dir/$f"; done
}
run() { # name expect(ok|fail) env...; uses $root and $dist
  local name="$1" expect="$2"; shift 2
  rm -rf "$dist"
  if env CARGO_TARGET_DIR="$root" DIST="$dist" "$@" bash "$script" >/dev/null 2>&1; then
    [ "$expect" = ok ] && pass "$name" || fail "$name (expected failure)"
  else
    [ "$expect" = fail ] && pass "$name" || fail "$name (expected success)"
  fi
}
listing() { (cd "$dist" && ls | sort | tr '\n' ' '); }
expect_listing() { local actual; actual="$(listing)"; expect_eq "$1" "$2" "$actual"; }

root="$tmp/mac"; T=aarch64-apple-darwin; b="$root/$T/release-gpui/bundle"
touch_files "$b/dmg" "QueryMT_0.6.0_aarch64.dmg"
touch_files "$b/macos" "QueryMT.app.tar.gz"
run "macos collects dmg and app.tar.gz" ok TARGET=$T ASSET_PREFIX=$P ASSET_SUFFIX=macos-aarch64 VERSION_NAME=nightly
expect_listing "macos names" "$P-nightly-macos-aarch64.app.tar.gz $P-nightly-macos-aarch64.dmg "
run "macos stable naming" ok TARGET=$T ASSET_PREFIX=$P ASSET_SUFFIX=macos-aarch64 VERSION_NAME=v0.6.0
expect_listing "macos stable names" "$P-v0.6.0-macos-aarch64.app.tar.gz $P-v0.6.0-macos-aarch64.dmg "
run "default profile is release-gpui, not release" fail TARGET=$T ASSET_PREFIX=$P ASSET_SUFFIX=macos-aarch64 VERSION_NAME=nightly CARGO_PROFILE=release

root="$tmp/win"; T=x86_64-pc-windows-msvc
touch_files "$root/$T/release-gpui/bundle/nsis" "QueryMT_0.6.0_x64-setup.exe"
run "windows collects setup exe" ok TARGET=$T ASSET_PREFIX=$P ASSET_SUFFIX=windows-x64-setup VERSION_NAME=nightly
expect_listing "windows names" "$P-nightly-windows-x64-setup.exe "

root="$tmp/linux"; T=x86_64-unknown-linux-gnu; b="$root/$T/release-gpui/bundle"
touch_files "$b/appimage" "QueryMT-Desktop-0.6.0-linux-x86_64.AppImage"
touch_files "$b/deb" "QueryMT-Desktop-0.6.0-linux-x86_64.deb"
run "linux collects AppImage and deb" ok TARGET=$T ASSET_PREFIX=$P ASSET_SUFFIX=linux-x86_64 VERSION_NAME=nightly
expect_listing "linux names" "$P-nightly-linux-x86_64.AppImage $P-nightly-linux-x86_64.deb "

touch_files "$b/appimage" "Second.AppImage"
run "two AppImages is ambiguous" fail TARGET=$T ASSET_PREFIX=$P ASSET_SUFFIX=linux-x86_64 VERSION_NAME=nightly
run "missing bundle dir fails" fail TARGET=aarch64-unknown-linux-gnu ASSET_PREFIX=$P ASSET_SUFFIX=linux-aarch64 VERSION_NAME=nightly
run "unsupported target fails" fail TARGET=x86_64-unknown-freebsd ASSET_PREFIX=a ASSET_SUFFIX=b VERSION_NAME=nightly

if [ "$failures" -gt 0 ]; then
  echo "$failures test(s) failed" >&2
  exit 1
fi
echo "all desktop release script tests passed"
