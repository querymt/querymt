#!/usr/bin/env bash
# Package a macOS release build as "QueryMT Desktop.app", an app.tar.gz and a dmg.
#
#   crates/querymt-gpui/scripts/package/macos.sh <version> <target>
#
# Reads <workspace target>/<target>/<profile>/querymt-gpui (profile from
# CARGO_PROFILE, default release-gpui) and writes
# <workspace target>/<target>/<profile>/bundle/{macos,dmg}/, the same layout `tauri build`
# produces for querymt-desktop. The app is ad-hoc signed only: there is no
# Developer ID signing or notarization yet.
#
# QMTCODE_BINARY is the qmtcode of the same release for <target>. It is copied
# to Contents/MacOS/qmtcode. Unset: an error under CI, a warning elsewhere.
set -euo pipefail

# The crate directory: packaging/ paths below are relative to it.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKSPACE="$(cd "$ROOT/../.." && pwd)"
cd "$ROOT"
# shellcheck source=common.sh
source "$ROOT/scripts/package/common.sh"

VERSION="${1:?usage: macos.sh <version> <target>}"
TARGET="${2:?usage: macos.sh <version> <target>}"
case "$TARGET" in
  aarch64-apple-darwin) ARCH=aarch64 ;;
  x86_64-apple-darwin) ARCH=x64 ;;
  *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
esac

TARGET_ROOT="${CARGO_TARGET_DIR:-$WORKSPACE/target}"
PROFILE="${CARGO_PROFILE:-release-gpui}"
BINARY="$TARGET_ROOT/$TARGET/$PROFILE/querymt-gpui"
[ -x "$BINARY" ] || { echo "binary not found at $BINARY" >&2; exit 1; }
# Before anything is written: a CI run without qmtcode leaves no installer.
QMTCODE="$(resolve_qmtcode)" || exit 1

OUT="$TARGET_ROOT/$TARGET/$PROFILE/bundle"
rm -rf "$OUT/macos" "$OUT/dmg"
mkdir -p "$OUT/macos" "$OUT/dmg"

# Bundle versions must be numeric (X.Y.Z): drop any prerelease suffix.
BUNDLE_VERSION="${VERSION%%-*}"

APP_NAME="QueryMT Desktop"
APP="$OUT/macos/$APP_NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/querymt-gpui"
cp packaging/icons/icon.icns "$APP/Contents/Resources/QueryMT.icns"
if [ -n "$QMTCODE" ]; then
  cp "$QMTCODE" "$APP/Contents/MacOS/qmtcode"
  chmod 755 "$APP/Contents/MacOS/qmtcode"
  # A bundle signature does not cover an unsigned helper in Contents/MacOS, so
  # sign qmtcode first; `codesign --verify --strict` fails otherwise.
  codesign --force --sign - --timestamp=none "$APP/Contents/MacOS/qmtcode"
fi
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>${APP_NAME}</string>
  <key>CFBundleExecutable</key><string>querymt-gpui</string>
  <key>CFBundleIconFile</key><string>QueryMT</string>
  <key>CFBundleIdentifier</key><string>mt.query.desktop</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>${APP_NAME}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${BUNDLE_VERSION}</string>
  <key>CFBundleVersion</key><string>${BUNDLE_VERSION}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --strict --verbose "$APP"

# Updater-style archive, as tauri's bundle/macos/*.app.tar.gz.
tar -C "$OUT/macos" -czf "$OUT/macos/$APP_NAME.app.tar.gz" "$APP_NAME.app"

# Drag-to-install disk image.
STAGE="$OUT/dmg/stage"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO \
  "$OUT/dmg/QueryMT_${VERSION}_${ARCH}.dmg"
rm -rf "$STAGE"

echo "macOS bundles written to $OUT"
