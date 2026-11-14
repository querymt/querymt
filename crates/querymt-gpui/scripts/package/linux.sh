#!/usr/bin/env bash
# Package a Linux release build as AppImage and deb.
#
#   scripts/package/linux.sh <version> <target>
#
# Reads target/<target>/release/querymt-gpui and writes
# target/<target>/release/bundle/{appimage,deb}/. Follows querymt-desktop's
# scripts/cef/package.sh: appimagetool (optionally with a pinned runtime in
# APPIMAGE_RUNTIME_FILE) and nfpm. GPUI renders through the host's Vulkan/EGL
# drivers, so no graphics libraries are bundled.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

VERSION="${1:?usage: linux.sh <version> <target>}"
TARGET="${2:?usage: linux.sh <version> <target>}"
case "$TARGET" in
  x86_64-*-linux-*) ARCH=x86_64; PKG_ARCH=amd64 ;;
  aarch64-*-linux-*) ARCH=aarch64; PKG_ARCH=arm64 ;;
  *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
esac

TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}"
BINARY="$TARGET_ROOT/$TARGET/release/querymt-gpui"
[ -x "$BINARY" ] || { echo "binary not found at $BINARY" >&2; exit 1; }

OUT="$TARGET_ROOT/$TARGET/release/bundle"
WORK="$TARGET_ROOT/$TARGET/package-linux"
rm -rf "$WORK" "$OUT/appimage" "$OUT/deb"
mkdir -p "$WORK" "$OUT/appimage" "$OUT/deb"

# --- AppImage ----------------------------------------------------------------
APPDIR="$WORK/QueryMT.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps"
cp "$BINARY" "$APPDIR/usr/bin/querymt-gpui"
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
APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" "${APPIMAGE_ARGS[@]}" "$APPDIR" \
  "$OUT/appimage/QueryMT-GPUI-$VERSION-linux-$ARCH.AppImage"

# --- deb ---------------------------------------------------------------------
NFPM="${NFPM:-$(command -v nfpm || true)}"
[ -n "$NFPM" ] || { echo "nfpm not found" >&2; exit 1; }
PKGROOT="$WORK/pkgroot"
mkdir -p "$PKGROOT/usr/bin" "$PKGROOT/usr/share/applications" \
  "$PKGROOT/usr/share/icons/hicolor/256x256/apps"
cp "$BINARY" "$PKGROOT/usr/bin/querymt-gpui"
chmod 755 "$PKGROOT/usr/bin/querymt-gpui"
cp packaging/linux/querymt-gpui.desktop "$PKGROOT/usr/share/applications/"
cp packaging/icons/128x128@2x.png "$PKGROOT/usr/share/icons/hicolor/256x256/apps/querymt-gpui.png"
PKGROOT="$PKGROOT" PKG_ARCH="$PKG_ARCH" PKG_VERSION="$VERSION" "$NFPM" pkg \
  -f packaging/nfpm.yaml -p deb -t "$OUT/deb/QueryMT-GPUI-$VERSION-linux-$ARCH.deb"

echo "Linux packages written to $OUT"
