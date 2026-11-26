#!/usr/bin/env bash
# Tests for the release helper scripts (nightly version, package version,
# version stamping, bundle collection), using fake inputs (no real build).
#
#   scripts/release/test-release-scripts.sh
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

# --- nightly-version.sh (same table as querymt's) ----------------------------
TS=20261114120000
nv() { # version tags...
  local v="$1"; shift
  printf '%s\n' "$@" | "$here/nightly-version.sh" "$v" "$TS"
}
expect_eq "nightly: unreleased version" "0.1.0-nightly.$TS" "$(nv 0.1.0 v0.0.9)"
expect_eq "nightly: version already tagged -> next patch" "0.1.1-nightly.$TS" "$(nv 0.1.0 v0.1.0)"
expect_eq "nightly: ls-remote output with peeled tags" "0.1.1-nightly.$TS" \
  "$(printf '%s\n' $'aaaa\trefs/tags/v0.1.0' $'bbbb\trefs/tags/v0.1.0^{}' | "$here/nightly-version.sh" 0.1.0 "$TS")"
expect_eq "nightly: no tags at all" "0.1.0-nightly.$TS" "$(printf '' | "$here/nightly-version.sh" 0.1.0 "$TS")"
expect_eq "nightly: v0.1.0 not matched by v0.1.00 or v10.1.0" "0.1.0-nightly.$TS" "$(nv 0.1.0 v0.1.00 v10.1.0 v0.1.0-rc1)"
if "$here/nightly-version.sh" 0.1 "$TS" </dev/null 2>/dev/null; then
  fail "nightly: rejects non X.Y.Z version"
else pass "nightly: rejects non X.Y.Z version"; fi

# --- package-version.sh -------------------------------------------------------
cat > "$tmp/Cargo.toml" <<'EOF'
[package]
name = "querymt-gpui"
version = "0.4.2"
edition = "2024"

[dependencies]
version-sync = { version = "9.9.9" }
EOF
expect_eq "package-version: reads [package]" "0.4.2" "$("$here/package-version.sh" "$tmp/Cargo.toml")"
expect_eq "package-version: matches the repo manifest" \
  "$(awk -F'"' '/^\[package\]/{f=1} f&&/^version/{print $2; exit}' "$here/../../Cargo.toml")" \
  "$("$here/package-version.sh" "$here/../../Cargo.toml")"
printf '[workspace]\n' > "$tmp/Empty.toml"
if "$here/package-version.sh" "$tmp/Empty.toml" >/dev/null 2>&1; then
  fail "package-version: fails without [package]"
else pass "package-version: fails without [package]"; fi

# --- stamp-version.sh ---------------------------------------------------------
cat > "$tmp/Cargo.lock" <<'EOF'
[[package]]
name = "querymt-gpui"
version = "0.4.2"
dependencies = [
 "serde",
]

[[package]]
name = "serde"
version = "1.0.0"
EOF
"$here/stamp-version.sh" "0.4.3-nightly.$TS" "$tmp/Cargo.toml" "$tmp/Cargo.lock" >/dev/null
expect_eq "stamp: Cargo.toml [package] version" "0.4.3-nightly.$TS" "$("$here/package-version.sh" "$tmp/Cargo.toml")"
expect_eq "stamp: dependency versions untouched" '9.9.9' "$(sed -n 's/.*{ version = "\(.*\)" }/\1/p' "$tmp/Cargo.toml")"
expect_eq "stamp: Cargo.lock own entry" "0.4.3-nightly.$TS" "$(awk '/^name = "querymt-gpui"/{f=1;next} f{sub(/version = "/,"");sub(/"$/,"");print;exit}' "$tmp/Cargo.lock")"
expect_eq "stamp: other lock entries untouched" "1.0.0" "$(awk '/^name = "serde"/{f=1;next} f{sub(/version = "/,"");sub(/"$/,"");print;exit}' "$tmp/Cargo.lock")"
if "$here/stamp-version.sh" "not-a-version" "$tmp/Cargo.toml" "$tmp/Cargo.lock" >/dev/null 2>&1; then
  fail "stamp: rejects a non-semver version"
else pass "stamp: rejects a non-semver version"; fi

# --- collect-bundles.sh -------------------------------------------------------
script="$here/collect-bundles.sh"
dist="$tmp/dist"
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

root="$tmp/mac"; T=aarch64-apple-darwin; b="$root/$T/release/bundle"
touch_files "$b/dmg" "QueryMT_0.1.0_aarch64.dmg"
touch_files "$b/macos" "QueryMT.app.tar.gz"
run "macos collects dmg and app.tar.gz" ok TARGET=$T ASSET_PREFIX=QueryMT-GPUI ASSET_SUFFIX=macos-aarch64 VERSION_NAME=nightly
expect_listing "macos names" "QueryMT-GPUI-nightly-macos-aarch64.app.tar.gz QueryMT-GPUI-nightly-macos-aarch64.dmg "
run "macos stable naming" ok TARGET=$T ASSET_PREFIX=QueryMT-GPUI ASSET_SUFFIX=macos-aarch64 VERSION_NAME=v0.1.0
expect_listing "macos stable names" "QueryMT-GPUI-v0.1.0-macos-aarch64.app.tar.gz QueryMT-GPUI-v0.1.0-macos-aarch64.dmg "

root="$tmp/win"; T=x86_64-pc-windows-msvc
touch_files "$root/$T/release/bundle/nsis" "QueryMT_0.1.0_x64-setup.exe"
run "windows collects setup exe" ok TARGET=$T ASSET_PREFIX=QueryMT-GPUI ASSET_SUFFIX=windows-x64-setup VERSION_NAME=nightly
expect_listing "windows names" "QueryMT-GPUI-nightly-windows-x64-setup.exe "

root="$tmp/linux"; T=x86_64-unknown-linux-gnu; b="$root/$T/release/bundle"
touch_files "$b/appimage" "QueryMT-GPUI-0.1.0-linux-x86_64.AppImage"
touch_files "$b/deb" "QueryMT-GPUI-0.1.0-linux-x86_64.deb"
run "linux collects AppImage and deb" ok TARGET=$T ASSET_PREFIX=QueryMT-GPUI ASSET_SUFFIX=linux-x86_64 VERSION_NAME=nightly
expect_listing "linux names" "QueryMT-GPUI-nightly-linux-x86_64.AppImage QueryMT-GPUI-nightly-linux-x86_64.deb "

touch_files "$b/appimage" "Second.AppImage"
run "two AppImages is ambiguous" fail TARGET=$T ASSET_PREFIX=QueryMT-GPUI ASSET_SUFFIX=linux-x86_64 VERSION_NAME=nightly
run "missing bundle dir fails" fail TARGET=aarch64-unknown-linux-gnu ASSET_PREFIX=QueryMT-GPUI ASSET_SUFFIX=linux-aarch64 VERSION_NAME=nightly
run "unsupported target fails" fail TARGET=x86_64-unknown-freebsd ASSET_PREFIX=a ASSET_SUFFIX=b VERSION_NAME=nightly

if [ "$failures" -gt 0 ]; then
  echo "$failures test(s) failed" >&2
  exit 1
fi
echo "all release script tests passed"
