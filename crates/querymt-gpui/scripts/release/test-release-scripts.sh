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
touch_files "$b/macos" "QueryMT Desktop.app.tar.gz"
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

# --- check-bundles.sh ---------------------------------------------------------
# Fake bundles: directory trees and file lists. The platform tools (codesign,
# dpkg-deb, 7-Zip) are stubbed through environment overrides; hdiutil is
# pointed at a command that does not exist, so the dmg is not opened.
check="$here/check-bundles.sh"
V=0.6.0-nightly.20261105111147
stubs="$tmp/stubs"
mkdir -p "$stubs"
# dpkg-deb: `-c <deb>` prints <deb>.list, `-f <deb> Package` prints <deb>.pkg.
cat > "$stubs/dpkg-deb" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  -c) cat "$2.list" ;;
  -f) cat "$2.pkg" ;;
esac
STUB
# 7z: `l <file>` prints <file>.list.
cat > "$stubs/7z" <<'STUB'
#!/usr/bin/env bash
cat "$2.list"
STUB
chmod 755 "$stubs/dpkg-deb" "$stubs/7z"

fake_qmtcode() { # path version-text
  mkdir -p "$(dirname "$1")"
  printf '#!/bin/sh\necho "qmtcode %s"\n' "$2" > "$1"
  chmod 755 "$1"
}

# bundle_check <name> <expect ok|fail> <target> <root> [env...]
bundle_check() {
  local name="$1" expect="$2" target="$3" root="$4"; shift 4
  if env CARGO_TARGET_DIR="$root" HDIUTIL=/nonexistent DPKG_DEB="$stubs/dpkg-deb" \
    SEVEN_ZIP="$stubs/7z" CODESIGN=true CHECK_HOST_ARCH=aarch64 "$@" \
    bash "$check" "$target" "$V" >"$tmp/check.out" 2>&1; then
    if [ "$expect" = ok ]; then pass "$name"; else fail "$name (expected failure)"; fi
  else
    if [ "$expect" = fail ]; then pass "$name"; else fail "$name (expected success): $(cat "$tmp/check.out")"; fi
  fi
}

# macOS: an app directory, its tar.gz and a dmg placeholder.
make_mac() { # root [qmtcode version text]
  local root="$1" b="$1/aarch64-apple-darwin/release-gpui/bundle"
  rm -rf "$root"
  fake_qmtcode "$b/macos/QueryMT Desktop.app/Contents/MacOS/qmtcode" "${2:-$V}"
  echo x > "$b/macos/QueryMT Desktop.app/Contents/MacOS/querymt-gpui"
  tar -C "$b/macos" -czf "$b/macos/QueryMT Desktop.app.tar.gz" "QueryMT Desktop.app"
  mkdir -p "$b/dmg"; echo dmg > "$b/dmg/QueryMT_0.6.0_aarch64.dmg"
}
root="$tmp/chk-mac"; T=aarch64-apple-darwin; b="$root/$T/release-gpui/bundle"
make_mac "$root"
bundle_check "installer content check passes for a complete macOS bundle" ok $T "$root"
bundle_check "installer content check fails the build: macOS codesign fails" fail $T "$root" CODESIGN=false
bundle_check "installer content check skips running qmtcode on another architecture" ok $T "$root" CHECK_HOST_ARCH=x86_64
make_mac "$root" "9.9.9"
bundle_check "installer content check fails the build: macOS qmtcode has another version" fail $T "$root"
make_mac "$root"; rm "$b/macos/QueryMT Desktop.app/Contents/MacOS/qmtcode"
bundle_check "installer content check fails the build: macOS app has no qmtcode" fail $T "$root"
make_mac "$root"
tar -C "$b/macos" -czf "$b/macos/QueryMT Desktop.app.tar.gz" "QueryMT Desktop.app/Contents/MacOS/querymt-gpui"
bundle_check "installer content check fails the build: macOS tar.gz has no qmtcode" fail $T "$root"

# Linux: a deb file list, and an AppImage that extracts a fake tree.
make_linux() { # root [qmtcode path in the deb list] [package name] [version text]
  local root="$1" b="$1/aarch64-unknown-linux-gnu/release-gpui/bundle"
  rm -rf "$root"; mkdir -p "$b/deb" "$b/appimage"
  local deb="$b/deb/QueryMT-Desktop-0.6.0-linux-aarch64.deb"
  : > "$deb"
  printf -- '-rwxr-xr-x root/root 1 2026-11-05 00:00 ./usr/bin/querymt-gpui\n-rwxr-xr-x root/root 1 2026-11-05 00:00 %s\n' \
    "${2:-./usr/lib/querymt-gpui/qmtcode}" > "$deb.list"
  echo "${3:-querymt-desktop}" > "$deb.pkg"
  cat > "$b/appimage/QueryMT-Desktop-0.6.0-linux-aarch64.AppImage" <<APPIMAGE
#!/bin/sh
[ "\$1" = --appimage-extract ] || exit 1
mkdir -p squashfs-root/usr/lib/querymt-gpui
printf '#!/bin/sh\necho "qmtcode %s"\n' '${4:-$V}' > squashfs-root/usr/lib/querymt-gpui/qmtcode
chmod 755 squashfs-root/usr/lib/querymt-gpui/qmtcode
APPIMAGE
  chmod 755 "$b/appimage/QueryMT-Desktop-0.6.0-linux-aarch64.AppImage"
}
root="$tmp/chk-linux"; T=aarch64-unknown-linux-gnu
make_linux "$root"
bundle_check "installer content check passes for complete Linux packages" ok $T "$root"
make_linux "$root" ./usr/bin/qmtcode
bundle_check "installer content check fails the build: deb has qmtcode only in /usr/bin" fail $T "$root"
make_linux "$root" ./usr/lib/querymt-gpui/qmtcode querymt-gpui
bundle_check "installer content check fails the build: deb has the old package name" fail $T "$root"
make_linux "$root" ./usr/lib/querymt-gpui/other
bundle_check "installer content check fails the build: deb lacks qmtcode" fail $T "$root"
make_linux "$root" ./usr/lib/querymt-gpui/qmtcode querymt-desktop 1.0.0
bundle_check "installer content check fails the build: AppImage qmtcode has another version" fail $T "$root"

# Windows: the listing of the setup exe.
root="$tmp/chk-win"; T=x86_64-pc-windows-msvc; b="$root/$T/release-gpui/bundle/nsis"
mkdir -p "$b"; echo exe > "$b/QueryMT_0.6.0_x64-setup.exe"
printf '   Date      Time    Attr   Size   Name\n2026-11-05 00:00:00 .....  1  1  qmtcode.exe\n2026-11-05 00:00:00 .....  1  1  querymt-gpui.exe\n' > "$b/QueryMT_0.6.0_x64-setup.exe.list"
bundle_check "installer content check passes for a Windows setup with qmtcode" ok $T "$root"
printf '2026-11-05 00:00:00 .....  1  1  querymt-gpui.exe\n' > "$b/QueryMT_0.6.0_x64-setup.exe.list"
bundle_check "installer content check fails the build: Windows setup has no qmtcode.exe" fail $T "$root"
bundle_check "installer content check fails the build: missing bundle directory" fail $T "$tmp/chk-none"

# --- packaging without qmtcode (scripts/package/common.sh) --------------------
pkg_common="$here/../package/common.sh"
resolve() { # env... ; prints "<exit> <stdout>|<stderr>"
  local out err rc=0
  out="$(env -u CI -u QMTCODE_BINARY "$@" bash -c "source '$pkg_common'; resolve_qmtcode" 2>"$tmp/resolve.err")" || rc=$?
  err="$(cat "$tmp/resolve.err")"
  echo "$rc $out|$err"
}
echo qmtcode > "$tmp/qmtcode-bin"
case "$(resolve CI=1)" in
  "1 |error: QMTCODE_BINARY is not set"*) pass "ci packaging without qmtcode fails (helper)" ;;
  *) fail "ci packaging without qmtcode fails (helper): $(resolve CI=1)" ;;
esac
case "$(resolve)" in
  "0 |warning: QMTCODE_BINARY is not set; the installer will have no bundled qmtcode") pass "local packaging without qmtcode warns (helper)" ;;
  *) fail "local packaging without qmtcode warns (helper): $(resolve)" ;;
esac
expect_eq "packaging with qmtcode uses it" "0 $tmp/qmtcode-bin|" "$(resolve CI=1 QMTCODE_BINARY="$tmp/qmtcode-bin")"
case "$(resolve QMTCODE_BINARY="$tmp/missing")" in
  "1 |error: QMTCODE_BINARY does not point to a file"*) pass "packaging with a missing qmtcode file fails" ;;
  *) fail "packaging with a missing qmtcode file fails: $(resolve QMTCODE_BINARY="$tmp/missing")" ;;
esac

# The Linux script end to end, with stubbed appimagetool and nfpm.
lin_root="$tmp/pkg-linux"; T=x86_64-unknown-linux-gnu
mkdir -p "$lin_root/$T/release-gpui" "$tmp/pkg-stubs"
printf '#!/bin/sh\n' > "$lin_root/$T/release-gpui/querymt-gpui"; chmod 755 "$lin_root/$T/release-gpui/querymt-gpui"
printf '#!/bin/sh\nfor a; do last=$a; done; touch "$last"\n' > "$tmp/pkg-stubs/appimagetool"
printf '#!/bin/sh\nwhile [ $# -gt 0 ]; do [ "$1" = -t ] && touch "$2"; shift; done\n' > "$tmp/pkg-stubs/nfpm"
chmod 755 "$tmp/pkg-stubs/appimagetool" "$tmp/pkg-stubs/nfpm"
linux_pkg() { # env... ; runs linux.sh
  env -u CI -u QMTCODE_BINARY CARGO_TARGET_DIR="$lin_root" APPIMAGETOOL="$tmp/pkg-stubs/appimagetool" \
    NFPM="$tmp/pkg-stubs/nfpm" "$@" bash "$here/../package/linux.sh" 0.0.0 "$T" >"$tmp/pkg.out" 2>&1
}
bundles_written() { find "$lin_root/$T/release-gpui/bundle" -type f 2>/dev/null | grep -c . || true; }
rm -rf "$lin_root/$T/release-gpui/bundle"
if linux_pkg CI=1; then fail "ci packaging without qmtcode fails (linux.sh, expected failure)"; else
  if grep -q QMTCODE_BINARY "$tmp/pkg.out"; then pass "ci packaging without qmtcode fails"; else fail "ci packaging without qmtcode fails: error does not name QMTCODE_BINARY"; fi
  expect_eq "ci packaging without qmtcode writes no installer" 0 "$(bundles_written)"
fi
if linux_pkg; then
  if grep -q "no bundled qmtcode" "$tmp/pkg.out"; then pass "local packaging without qmtcode warns"; else fail "local packaging without qmtcode warns: no warning"; fi
  expect_eq "local packaging without qmtcode still writes the installers" 2 "$(bundles_written)"
  [ ! -e "$lin_root/$T/package-linux/pkgroot/usr/lib/querymt-gpui/qmtcode" ] && pass "local packaging without qmtcode bundles none" || fail "local packaging without qmtcode bundles none"
else fail "local packaging without qmtcode warns (linux.sh failed): $(cat "$tmp/pkg.out")"; fi
rm -rf "$lin_root/$T/release-gpui/bundle"
if linux_pkg CI=1 QMTCODE_BINARY="$tmp/qmtcode-bin"; then
  [ -x "$lin_root/$T/package-linux/pkgroot/usr/lib/querymt-gpui/qmtcode" ] \
    && [ -x "$lin_root/$T/package-linux/QueryMT.AppDir/usr/lib/querymt-gpui/qmtcode" ] \
    && pass "linux packaging puts qmtcode in usr/lib/querymt-gpui" || fail "linux packaging puts qmtcode in usr/lib/querymt-gpui"
else fail "linux packaging with qmtcode: $(cat "$tmp/pkg.out")"; fi

if [ "$failures" -gt 0 ]; then
  echo "$failures test(s) failed" >&2
  exit 1
fi
echo "all desktop release script tests passed"
