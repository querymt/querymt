#!/usr/bin/env bash
# Check that the installers of one build job contain the bundled qmtcode, so a
# release never publishes an installer without it.
#
#   check-bundles.sh <target> <version>
#
# <target> is the Rust target triple the installers were built for and <version>
# the version the packaging scripts were given; the bundled qmtcode must report
# it from `--version`. Bundles are read from
# <CARGO_TARGET_DIR>/<target>/<CARGO_PROFILE>/bundle/, as collect-bundles.sh does.
#
# Checks (a failure exits non-zero and names the installer):
#   *-apple-darwin  "QueryMT Desktop.app" has an executable Contents/MacOS/qmtcode,
#                   passes `codesign --verify --strict`, and its qmtcode runs and
#                   reports <version> (when this host's architecture matches);
#                   the app.tar.gz lists qmtcode; the dmg holds it (when hdiutil
#                   is available).
#   *-linux-*       deb: ./usr/lib/querymt-gpui/qmtcode is listed and executable,
#                   nothing named qmtcode is in /usr/bin, and the package is
#                   querymt-desktop. AppImage: usr/lib/querymt-gpui/qmtcode is in
#                   the image, and it reports <version> (host architecture matches).
#   *-windows-*     the setup exe lists qmtcode.exe (7-Zip).
#
# Environment (all optional; tests replace the platform tools):
#   CARGO_TARGET_DIR, CARGO_PROFILE   where the bundles are (default target, release-gpui)
#   CODESIGN, DPKG_DEB, HDIUTIL       tool commands (defaults codesign, dpkg-deb, hdiutil)
#   SEVEN_ZIP                         7-Zip command (default: 7z, 7zz, 7za or Program Files)
#   CHECK_HOST_ARCH                   host architecture (default uname -m)
set -euo pipefail

TARGET="${1:?usage: check-bundles.sh <target> <version>}"
VERSION="${2:?usage: check-bundles.sh <target> <version>}"

TARGET_ROOT="${CARGO_TARGET_DIR:-target}"
BUNDLE="$TARGET_ROOT/$TARGET/${CARGO_PROFILE:-release-gpui}/bundle"
CODESIGN="${CODESIGN:-codesign}"
DPKG_DEB="${DPKG_DEB:-dpkg-deb}"
HDIUTIL="${HDIUTIL:-hdiutil}"

tmp="$(mktemp -d)"
cleanup() {
  if [ -n "${MOUNT:-}" ]; then "$HDIUTIL" detach -quiet "$MOUNT" >/dev/null 2>&1 || true; fi
  rm -rf "$tmp"
}
trap cleanup EXIT

die() {
  echo "::error::check-bundles: $*" >&2
  exit 1
}
note() { echo "check-bundles: $*"; }

host_arch="${CHECK_HOST_ARCH:-$(uname -m)}"
case "$host_arch" in arm64) host_arch=aarch64 ;; amd64) host_arch=x86_64 ;; esac
target_arch="${TARGET%%-*}"

[ -d "$BUNDLE" ] || die "bundle directory $BUNDLE does not exist"

# single <dir> <glob>: the one file matching, or fail.
single() {
  local dir="$1" glob="$2"
  local -a matches=()
  local f
  for f in "$dir"/$glob; do
    [ -e "$f" ] && matches+=("$f")
  done
  [ "${#matches[@]}" -eq 1 ] \
    || die "expected exactly one $dir/$glob, found ${#matches[@]}"
  printf '%s\n' "${matches[0]}"
}

# check_version <label> <qmtcode path>: `--version` names <version>.
check_version() {
  local label="$1" qmtcode="$2" out
  out="$("$qmtcode" --version 2>&1)" || die "$label: qmtcode --version failed: $out"
  if ! printf '%s\n' "$out" | tr -s ' \t' '\n\n' | grep -qxF -e "$VERSION" -e "v$VERSION"; then
    die "$label: bundled qmtcode reports '$out', expected version $VERSION"
  fi
  note "$label: qmtcode reports $VERSION"
}

# can_run: this host can run binaries built for the target.
can_run() { [ "$host_arch" = "$target_arch" ]; }

check_macos() {
  local app="$BUNDLE/macos/QueryMT Desktop.app"
  [ -d "$app" ] || die "macOS: $app does not exist"
  [ -x "$app/Contents/MacOS/qmtcode" ] \
    || die "macOS: $app has no executable Contents/MacOS/qmtcode"
  "$CODESIGN" --verify --strict "$app" \
    || die "macOS: $app fails the strict code signature check"
  if can_run; then
    check_version "macOS" "$app/Contents/MacOS/qmtcode"
  else
    note "macOS: not running qmtcode ($host_arch host, $target_arch target)"
  fi

  local archive
  archive="$(single "$BUNDLE/macos" '*.app.tar.gz')"
  tar -tzf "$archive" | grep -qxF 'QueryMT Desktop.app/Contents/MacOS/qmtcode' \
    || die "macOS: $archive does not contain QueryMT Desktop.app/Contents/MacOS/qmtcode"

  local dmg
  dmg="$(single "$BUNDLE/dmg" '*.dmg')"
  if command -v "$HDIUTIL" >/dev/null 2>&1; then
    MOUNT="$tmp/dmg-mount"
    mkdir -p "$MOUNT"
    "$HDIUTIL" attach -readonly -nobrowse -noverify -quiet -mountpoint "$MOUNT" "$dmg" \
      || die "macOS: cannot open $dmg"
    [ -x "$MOUNT/QueryMT Desktop.app/Contents/MacOS/qmtcode" ] \
      || die "macOS: $dmg has no executable QueryMT Desktop.app/Contents/MacOS/qmtcode"
    "$HDIUTIL" detach -quiet "$MOUNT" >/dev/null 2>&1 || true
    MOUNT=""
  else
    note "macOS: hdiutil not available, not opening $dmg"
  fi
}

check_linux() {
  local deb listing line pkg
  deb="$(single "$BUNDLE/deb" '*.deb')"
  listing="$("$DPKG_DEB" -c "$deb")" || die "deb: cannot list $deb"
  line="$(printf '%s\n' "$listing" | awk '$NF == "./usr/lib/querymt-gpui/qmtcode"')"
  [ -n "$line" ] || die "deb: $deb does not list ./usr/lib/querymt-gpui/qmtcode"
  case "$line" in
    -r?x*) ;;
    *) die "deb: qmtcode in $deb is not executable: $line" ;;
  esac
  if printf '%s\n' "$listing" | awk '$NF == "./usr/bin/qmtcode"' | grep -q .; then
    die "deb: $deb installs qmtcode into /usr/bin"
  fi
  pkg="$("$DPKG_DEB" -f "$deb" Package)" || die "deb: cannot read the package name of $deb"
  [ "$pkg" = querymt-desktop ] || die "deb: package is '$pkg', expected querymt-desktop"

  local appimage root
  appimage="$(single "$BUNDLE/appimage" '*.AppImage')"
  if can_run; then
    appimage="$(cd "$(dirname "$appimage")" && pwd)/$(basename "$appimage")"
    mkdir -p "$tmp/appimage"
    (cd "$tmp/appimage" && "$appimage" --appimage-extract >/dev/null) \
      || die "AppImage: cannot extract $appimage"
    root="$tmp/appimage/squashfs-root"
    [ -x "$root/usr/lib/querymt-gpui/qmtcode" ] \
      || die "AppImage: $appimage has no executable usr/lib/querymt-gpui/qmtcode"
    check_version "AppImage" "$root/usr/lib/querymt-gpui/qmtcode"
  else
    note "AppImage: not extracting $appimage ($host_arch host, $target_arch target)"
  fi
}

check_windows() {
  local setup seven
  setup="$(single "$BUNDLE/nsis" '*.exe')"
  seven="${SEVEN_ZIP:-}"
  if [ -z "$seven" ]; then
    local candidate
    for candidate in 7z 7zz 7za "/c/Program Files/7-Zip/7z.exe"; do
      if command -v "$candidate" >/dev/null 2>&1 || [ -x "$candidate" ]; then
        seven="$candidate"
        break
      fi
    done
  fi
  [ -n "$seven" ] || die "Windows: 7-Zip not found (set SEVEN_ZIP)"
  "$seven" l "$setup" | grep -Eq '(^|[ /\\])qmtcode\.exe[[:space:]]*$' \
    || die "Windows: $setup does not contain qmtcode.exe"
  note "Windows: $setup contains qmtcode.exe"
}

case "$TARGET" in
  *-apple-darwin) check_macos ;;
  *-linux-*) check_linux ;;
  *-windows-*) check_windows ;;
  *) die "unsupported target '$TARGET'" ;;
esac
echo "check-bundles: $TARGET ok"
