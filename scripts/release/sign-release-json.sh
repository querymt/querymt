#!/usr/bin/env bash
# Sign release.json with the update key, so QueryMT Desktop can trust it.
#
#   sign-release-json.sh <release.json> <version> <stable|nightly>
#
# Writes <release.json>.minisig, a minisign signature whose trusted comment is
# `version:<version> channel:<channel>`. The app checks the comment against the
# manifest it verified, so a signature cannot be moved to another release.
#
# Environment:
#   QMT_UPDATE_SIGNING_KEY   the minisign secret key (the contents of the key
#                            file; a password-less key). Required.
#   QMT_UPDATE_PUBLIC_KEY    public key file to verify the new signature with
#                            (default: crates/querymt-gpui/packaging/update-key.pub,
#                            the key the app embeds). A signature the app would
#                            reject fails the script instead of the release.
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: sign-release-json.sh <release.json> <version> <stable|nightly>" >&2
  exit 1
fi

manifest="$1"
version="$2"
channel="$3"

case "$channel" in
  stable | nightly) ;;
  *)
    echo "sign-release-json: channel must be stable or nightly, got '$channel'" >&2
    exit 1
    ;;
esac

if [ -z "${QMT_UPDATE_SIGNING_KEY:-}" ]; then
  echo "sign-release-json: QMT_UPDATE_SIGNING_KEY is not set; refusing to publish an unsigned release" >&2
  exit 1
fi
[ -f "$manifest" ] || { echo "sign-release-json: $manifest does not exist" >&2; exit 1; }
command -v minisign >/dev/null 2>&1 || { echo "sign-release-json: minisign is not installed" >&2; exit 1; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
public_key="${QMT_UPDATE_PUBLIC_KEY:-$here/../../crates/querymt-gpui/packaging/update-key.pub}"
[ -f "$public_key" ] || { echo "sign-release-json: public key $public_key does not exist" >&2; exit 1; }

# The secret key lives in a private temp file only while minisign runs.
keydir="$(mktemp -d)"
trap 'rm -rf "$keydir"' EXIT
chmod 700 "$keydir"
keyfile="$keydir/update-key.key"
(umask 077 && printf '%s\n' "$QMT_UPDATE_SIGNING_KEY" > "$keyfile")

signature="$manifest.minisig"
rm -f "$signature"
minisign -S -s "$keyfile" -m "$manifest" -x "$signature" \
  -t "version:$version channel:$channel" >/dev/null \
  || { echo "sign-release-json: minisign failed to sign $manifest" >&2; exit 1; }

# The key in the secret must be the pair of the key embedded in the app.
minisign -V -p "$public_key" -m "$manifest" -x "$signature" >/dev/null \
  || {
    rm -f "$signature"
    echo "sign-release-json: the signature does not verify with $public_key; is QMT_UPDATE_SIGNING_KEY the pair of the embedded key?" >&2
    exit 1
  }
echo "signed $manifest (version:$version channel:$channel)"
