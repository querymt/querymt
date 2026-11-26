#!/usr/bin/env bash
# Collect the installers of one build job into dist/ with predictable names:
#
#   <ASSET_PREFIX>-<VERSION_NAME>-<ASSET_SUFFIX><ext>
#
# VERSION_NAME is `vX.Y.Z` for stable or `nightly` for the rolling release, so
# the rolling release URLs stay stable. Mirrors querymt-desktop's
# scripts/release/collect-bundles.sh, without Tauri updater signatures.
#
# Environment:
#   TARGET         Rust target triple (decides which bundles are expected)
#   ASSET_PREFIX   e.g. QueryMT-GPUI
#   ASSET_SUFFIX   e.g. macos-aarch64, windows-x64-setup, linux-x86_64
#   VERSION_NAME   vX.Y.Z | nightly
#   CARGO_TARGET_DIR   optional, default target
#   DIST               optional, default dist
#
# Expected bundles (written by scripts/package/*.sh):
#   *-apple-darwin   dmg, app.tar.gz
#   *-windows-*      NSIS setup exe
#   *-linux-*        AppImage, deb
set -euo pipefail

: "${TARGET:?TARGET is required}"
: "${ASSET_PREFIX:?ASSET_PREFIX is required}"
: "${ASSET_SUFFIX:?ASSET_SUFFIX is required}"
: "${VERSION_NAME:?VERSION_NAME is required}"

TARGET_ROOT="${CARGO_TARGET_DIR:-target}"
DIST="${DIST:-dist}"
mkdir -p "$DIST"
shopt -s nullglob

die() {
  echo "::error::collect-bundles: $*" >&2
  exit 1
}

BUNDLE="$TARGET_ROOT/$TARGET/release/bundle"
[ -d "$BUNDLE" ] || die "bundle directory $BUNDLE does not exist"

collected=0

# collect <dir> <glob> <ext>
# Copies the single file matching <dir>/<glob> to dist/. Exactly one match is
# required: several would make the asset name ambiguous.
collect() {
  local dir="$1" glob="$2" ext="$3"
  # shellcheck disable=SC2206 # $glob is meant to expand.
  local -a matches=("$BUNDLE/$dir"/$glob)
  [ "${#matches[@]}" -eq 1 ] \
    || die "expected exactly one $BUNDLE/$dir/$glob, found ${#matches[@]}"

  cp "${matches[0]}" "$DIST/${ASSET_PREFIX}-${VERSION_NAME}-${ASSET_SUFFIX}${ext}"
  collected=$((collected + 1))
}

case "$TARGET" in
  *-apple-darwin)
    collect dmg '*.dmg' .dmg
    collect macos '*.app.tar.gz' .app.tar.gz
    ;;
  *-windows-*)
    collect nsis '*.exe' .exe
    ;;
  *-linux-*)
    collect appimage '*.AppImage' .AppImage
    collect deb '*.deb' .deb
    ;;
  *) die "unsupported target '$TARGET'" ;;
esac

[ "$collected" -gt 0 ] || die "no bundles collected for $TARGET"
ls -l "$DIST"
