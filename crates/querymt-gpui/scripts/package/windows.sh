#!/usr/bin/env bash
# Package a Windows release build as an NSIS setup exe (run from Git Bash).
#
#   scripts/package/windows.sh <version> <target>
#
# Reads target/<target>/release/querymt-gpui.exe and writes
# target/<target>/release/bundle/nsis/, the same layout `tauri build` uses.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

VERSION="${1:?usage: windows.sh <version> <target>}"
TARGET="${2:?usage: windows.sh <version> <target>}"
case "$TARGET" in
  x86_64-pc-windows-*) ARCH=x64 ;;
  aarch64-pc-windows-*) ARCH=arm64 ;;
  *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
esac

TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}"
BINARY="$TARGET_ROOT/$TARGET/release/querymt-gpui.exe"
[ -f "$BINARY" ] || { echo "binary not found at $BINARY" >&2; exit 1; }

OUT="$TARGET_ROOT/$TARGET/release/bundle/nsis"
rm -rf "$OUT"
mkdir -p "$OUT"

MAKENSIS="${MAKENSIS:-$(command -v makensis || true)}"
if [ -z "$MAKENSIS" ] && [ -x "/c/Program Files (x86)/NSIS/makensis.exe" ]; then
  MAKENSIS="/c/Program Files (x86)/NSIS/makensis.exe"
fi
[ -n "$MAKENSIS" ] || { echo "makensis not found (install NSIS)" >&2; exit 1; }

# makensis is a native Windows program: hand it Windows paths. `-D` rather than
# `/D` so Git Bash does not rewrite the option as a path.
winpath() { if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else echo "$1"; fi; }
"$MAKENSIS" -V3 \
  "-DVERSION=$VERSION" \
  "-DSRC_EXE=$(winpath "$BINARY")" \
  "-DICON=$(winpath "$ROOT/packaging/icons/icon.ico")" \
  "-DOUT_FILE=$(winpath "$OUT/QueryMT_${VERSION}_${ARCH}-setup.exe")" \
  "$(winpath "$ROOT/packaging/windows/installer.nsi")"

echo "Windows installer written to $OUT"
