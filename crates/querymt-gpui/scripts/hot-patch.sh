#!/usr/bin/env bash
# Run QueryMT Desktop under `dx serve` with Rust hot-patching (debug, native).
#
#   crates/querymt-gpui/scripts/hot-patch.sh [app args]
#
# Edits to code in this crate (render functions in src/ui/**) are patched into
# the running window without a restart. Arguments go to the app, as with
# `cargo r-gpui`:
#
#   crates/querymt-gpui/scripts/hot-patch.sh --agent host:port --page sessions
#
# Needs the Dioxus CLI 0.7.10 (the version of gpui-pre's `subsecond`):
#   cargo binstall dioxus-cli@0.7.10
#
# QMT_DX overrides the CLI path. The default is $CARGO_HOME/bin/dx, never a bare
# `dx`: another tool's `dx` (Deno's) can come first on PATH. See
# crates/querymt-gpui/knowledge/development.md ("Hot-patching the UI").
set -euo pipefail

DX_VERSION="0.7.10"

dx="${QMT_DX:-${CARGO_HOME:-$HOME/.cargo}/bin/dx}"

install_hint() {
  echo "install it with: cargo binstall dioxus-cli@${DX_VERSION}" >&2
  echo "            or: cargo install dioxus-cli@${DX_VERSION} --locked" >&2
}

if [ ! -x "$dx" ]; then
  echo "error: Dioxus CLI not found at $dx" >&2
  install_hint
  exit 1
fi

# `dx --version` prints "dioxus 0.7.10 (<commit>)". Deno's `dx` prints "deno ...".
version_line="$("$dx" --version 2>&1 | head -n 1 || true)"
case "$version_line" in
  "dioxus ${DX_VERSION}" | "dioxus ${DX_VERSION} "*) ;;
  *)
    echo "error: $dx is not the Dioxus CLI ${DX_VERSION} (it reports: ${version_line:-nothing})" >&2
    install_hint
    exit 1
    ;;
esac

case "$(uname -s)" in
  Darwin) platform="macos" ;;
  Linux) platform="linux" ;;
  MINGW* | MSYS* | CYGWIN*) platform="windows" ;;
  *) platform="desktop" ;;
esac

# `dx serve` takes the app's arguments as one --args string (there is no
# trailing `-- <args>` form), so an argument that contains a space is not
# supported.
app_args=()
if [ "$#" -gt 0 ]; then
  for arg in "$@"; do
    case "$arg" in
      *[[:space:]]*)
        echo "error: app arguments with whitespace are not supported: '$arg'" >&2
        exit 1
        ;;
    esac
  done
  # `--args=...`: clap rejects a separate value that starts with `--`.
  app_args=("--args=$*")
fi

cd "$(dirname "${BASH_SOURCE[0]}")/../../.."

# dx records the linker arguments of the tip crate while cargo compiles it. When
# cargo finds the crate fresh, nothing is recorded and dx stops with "Missing
# linker args for fat link of 'querymt_gpui'", on a warm start with or without
# sccache. Touching the entry file makes cargo recompile just this crate (a few
# seconds incremental).
touch crates/querymt-gpui/src/main.rs

# --open false and --always-on-top false: the app is a native GPUI window, not a
# web page, and an always-on-top window gets in the way of the editor.
exec "$dx" serve \
  --hot-patch \
  --platform "$platform" \
  --open false \
  --always-on-top false \
  -p querymt-gpui \
  --bin querymt-gpui \
  --features hot-patch \
  ${app_args[@]+"${app_args[@]}"}
