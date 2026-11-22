#!/usr/bin/env bash
set -eu

root=$(mktemp -d "${TMPDIR:-/tmp}/querymt-test-home.XXXXXXXX")
trap 'rm -rf -- "$root"' EXIT

# Cargo and rustup still need the installed toolchain and dependency cache.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export HOME="$root/home" QMT_HOME="$root/qmt" QMT_CONFIG_DIR="$root/config"
export XDG_CONFIG_HOME="$root/xdg-config" XDG_DATA_HOME="$root/xdg-data"
export XDG_CACHE_HOME="$root/xdg-cache" XDG_STATE_HOME="$root/xdg-state"
unset QMT_SESSIONS_DB QMT_PROVIDER_CACHE_DIR
mkdir -p "$HOME" "$QMT_HOME" "$QMT_CONFIG_DIR" \
    "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME"

"$@"
