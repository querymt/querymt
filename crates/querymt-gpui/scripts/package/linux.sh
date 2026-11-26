#!/usr/bin/env bash
# Package a Linux release build as AppImage and deb.
#
#   crates/querymt-gpui/scripts/package/linux.sh <version> <target>
#
# Reads <workspace target>/<target>/<profile>/querymt-gpui (profile from
# CARGO_PROFILE, default release-gpui) and writes
# <workspace target>/<target>/<profile>/bundle/{appimage,deb}/. Follows querymt-desktop's
# scripts/cef/package.sh: appimagetool (optionally with a pinned runtime in
# APPIMAGE_RUNTIME_FILE) and nfpm. GPUI renders through the host's Vulkan/EGL
# drivers, so no graphics libraries are bundled.
#
# QMTCODE_BINARY is the (static musl) qmtcode of the same release. It goes to
# usr/lib/querymt-gpui/qmtcode, outside PATH; the app finds it from
# ../lib/querymt-gpui. Unset: an error under CI, a warning elsewhere.
set -euo pipefail

# The crate directory: packaging/ paths below are relative to it.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKSPACE="$(cd "$ROOT/../.." && pwd)"
cd "$ROOT"
# shellcheck source=common.sh
source "$ROOT/scripts/package/common.sh"

VERSION="${1:?usage: linux.sh <version> <target>}"
TARGET="${2:?usage: linux.sh <version> <target>}"
case "$TARGET" in
  x86_64-*-linux-*) ARCH=x86_64; PKG_ARCH=amd64 ;;
  aarch64-*-linux-*) ARCH=aarch64; PKG_ARCH=arm64 ;;
  *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
esac

TARGET_ROOT="${CARGO_TARGET_DIR:-$WORKSPACE/target}"
PROFILE="${CARGO_PROFILE:-release-gpui}"
BINARY="$TARGET_ROOT/$TARGET/$PROFILE/querymt-gpui"
[ -x "$BINARY" ] || { echo "binary not found at $BINARY" >&2; exit 1; }
# Before anything is written: a CI run without qmtcode leaves no installer.
QMTCODE="$(resolve_qmtcode)" || exit 1

OUT="$TARGET_ROOT/$TARGET/$PROFILE/bundle"
WORK="$TARGET_ROOT/$TARGET/package-linux"
rm -rf "$WORK" "$OUT/appimage" "$OUT/deb"
mkdir -p "$WORK" "$OUT/appimage" "$OUT/deb"

# --- AppImage ----------------------------------------------------------------
APPDIR="$WORK/QueryMT.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps"
cp "$BINARY" "$APPDIR/usr/bin/querymt-gpui"
if [ -n "$QMTCODE" ]; then
  mkdir -p "$APPDIR/usr/lib/querymt-gpui"
  install -m 755 "$QMTCODE" "$APPDIR/usr/lib/querymt-gpui/qmtcode"
fi
cp packaging/linux/querymt-gpui.desktop "$APPDIR/querymt-gpui.desktop"
cp packaging/linux/querymt-gpui.desktop "$APPDIR/usr/share/applications/"
cp packaging/icons/icon.png "$APPDIR/querymt-gpui.png"
cp packaging/icons/128x128@2x.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/querymt-gpui.png"
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/querymt-gpui" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun" "$APPDIR/usr/bin/querymt-gpui"

APPIMAGETOOL="${APPIMAGETOOL:-$(command -v appimagetool || command -v appimagetool.AppImage || true)}"
[ -n "$APPIMAGETOOL" ] || { echo "appimagetool not found" >&2; exit 1; }
APPIMAGE_ARGS=()
if [ -n "${APPIMAGE_RUNTIME_FILE:-}" ]; then
  [ -f "$APPIMAGE_RUNTIME_FILE" ] || { echo "AppImage runtime not found at $APPIMAGE_RUNTIME_FILE" >&2; exit 1; }
  APPIMAGE_ARGS+=(--runtime-file "$APPIMAGE_RUNTIME_FILE")
fi
# appimagetool reads the target architecture from ARCH.
export ARCH
APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" ${APPIMAGE_ARGS[@]+"${APPIMAGE_ARGS[@]}"} "$APPDIR" \
  "$OUT/appimage/QueryMT-Desktop-$VERSION-linux-$ARCH.AppImage"

# --- deb ---------------------------------------------------------------------
NFPM="${NFPM:-$(command -v nfpm || true)}"
[ -n "$NFPM" ] || { echo "nfpm not found" >&2; exit 1; }
PKGROOT="$WORK/pkgroot"
mkdir -p "$PKGROOT/usr/bin" "$PKGROOT/usr/share/applications" \
  "$PKGROOT/usr/share/icons/hicolor/256x256/apps"
cp "$BINARY" "$PKGROOT/usr/bin/querymt-gpui"
chmod 755 "$PKGROOT/usr/bin/querymt-gpui"
if [ -n "$QMTCODE" ]; then
  mkdir -p "$PKGROOT/usr/lib/querymt-gpui"
  install -m 755 "$QMTCODE" "$PKGROOT/usr/lib/querymt-gpui/qmtcode"
fi
cp packaging/linux/querymt-gpui.desktop "$PKGROOT/usr/share/applications/"
cp packaging/icons/128x128@2x.png "$PKGROOT/usr/share/icons/hicolor/256x256/apps/querymt-gpui.png"
PKGROOT="$PKGROOT" PKG_ARCH="$PKG_ARCH" PKG_VERSION="$VERSION" "$NFPM" pkg \
  -f packaging/nfpm.yaml -p deb -t "$OUT/deb/QueryMT-Desktop-$VERSION-linux-$ARCH.deb"

echo "Linux packages written to $OUT"
