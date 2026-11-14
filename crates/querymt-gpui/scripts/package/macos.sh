#!/usr/bin/env bash
# Package a macOS release build as QueryMT.app, an app.tar.gz and a dmg.
#
#   scripts/package/macos.sh <version> <target>
#
# Reads target/<target>/release/querymt-gpui and writes
# target/<target>/release/bundle/{macos,dmg}/, the same layout `tauri build`
# produces for querymt-desktop. The app is ad-hoc signed only: there is no
# Developer ID signing or notarization yet.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

VERSION="${1:?usage: macos.sh <version> <target>}"
TARGET="${2:?usage: macos.sh <version> <target>}"
case "$TARGET" in
  aarch64-apple-darwin) ARCH=aarch64 ;;
  x86_64-apple-darwin) ARCH=x64 ;;
  *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
esac

TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}"
BINARY="$TARGET_ROOT/$TARGET/release/querymt-gpui"
[ -x "$BINARY" ] || { echo "binary not found at $BINARY" >&2; exit 1; }

OUT="$TARGET_ROOT/$TARGET/release/bundle"
rm -rf "$OUT/macos" "$OUT/dmg"
mkdir -p "$OUT/macos" "$OUT/dmg"

# Bundle versions must be numeric (X.Y.Z): drop any prerelease suffix.
BUNDLE_VERSION="${VERSION%%-*}"

APP="$OUT/macos/QueryMT.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/querymt-gpui"
cp packaging/icons/icon.icns "$APP/Contents/Resources/QueryMT.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>QueryMT</string>
  <key>CFBundleExecutable</key><string>querymt-gpui</string>
  <key>CFBundleIconFile</key><string>QueryMT</string>
  <key>CFBundleIdentifier</key><string>com.querymt.gpui</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>QueryMT</string>
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
codesign --verify --verbose "$APP"

# Updater-style archive, as tauri's bundle/macos/*.app.tar.gz.
tar -C "$OUT/macos" -czf "$OUT/macos/QueryMT.app.tar.gz" QueryMT.app

# Drag-to-install disk image.
STAGE="$OUT/dmg/stage"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname QueryMT -srcfolder "$STAGE" -ov -format UDZO \
  "$OUT/dmg/QueryMT_${VERSION}_${ARCH}.dmg"
rm -rf "$STAGE"

echo "macOS bundles written to $OUT"
