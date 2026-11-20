#!/usr/bin/env bash
# Package locally cross-built qmt-headless binaries like CI does
# (`qmt-headless-<ver>-<target>/qmt-headless` in a tar.gz), record their SHA-256 and
# write configs/local.toml: configs/reference.toml with [headless_binary] pinned to the
# local archives (absolute `path` entries, so it is not committed).
#
#   cargo zigbuild --release -p querymt-agent --example qmt-headless --target aarch64-unknown-linux-musl
#   cargo zigbuild --release -p querymt-agent --example qmt-headless --target x86_64-unknown-linux-musl
#   scripts/package-headless.sh [version]      # default: 0.0.0-local.<short commit>
#   QMT_HARNESS_CONFIG=configs/local.toml uv run ...
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
repo="$(cd "$here/../.." && pwd)"
commit="$(git -C "$repo" rev-parse --short=12 HEAD)"
version="${1:-0.0.0-local.$commit}"
dist="$here/dist"
mkdir -p "$dist"

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

local_toml="$here/configs/local.toml"
# Everything in the reference up to the [headless_binary] table.
sed '/^\[headless_binary\]/,$d' "$here/configs/reference.toml" >"$local_toml"
{
  echo "[headless_binary]"
  echo "version = \"$version\"   # build commit $commit"
} >>"$local_toml"

for pair in linux-aarch64:aarch64-unknown-linux-musl linux-x86_64:x86_64-unknown-linux-musl; do
  platform="${pair%%:*}"
  target="${pair#*:}"
  bin="$repo/target/$target/release/examples/qmt-headless"
  [ -f "$bin" ] || { echo "missing $bin (build it first)" >&2; exit 1; }
  pkg="qmt-headless-$version-$target"
  stage="$dist/$pkg"
  rm -rf "$stage"
  mkdir -p "$stage"
  cp "$bin" "$stage/qmt-headless"
  cp "$repo/README.md" "$repo/LICENSE" "$stage/"
  archive="$dist/$pkg.tar.gz"
  # Fixed owner and timestamps so the same binary gives the same archive hash.
  COPYFILE_DISABLE=1 tar --uid 0 --gid 0 --uname root --gname root -czf "$archive" -C "$dist" "$pkg" 2>/dev/null \
    || COPYFILE_DISABLE=1 tar -czf "$archive" -C "$dist" "$pkg"
  rm -rf "$stage"
  sum="$(sha256 "$archive")"
  echo "$platform  $sum  $archive"
  {
    echo
    echo "[headless_binary.platforms.$platform]"
    echo "path = \"$archive\""
    echo "sha256 = \"$sum\""
  } >>"$local_toml"
done
echo "wrote $local_toml (build commit $commit)"
