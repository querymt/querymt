#!/usr/bin/env bash
# Out-of-workspace consumer check (design D5a).
#
# Creates a temporary crate outside this workspace with a path dependency on
# querymt-agent-session-core, builds it natively and for wasm32-unknown-unknown,
# and fails if its dependency tree contains tokio, rusqlite or the workspace hack.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/session-core-native.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/src"
cat >"$WORK/Cargo.toml" <<EOF
[package]
name = "session-core-consumer"
version = "0.0.0"
edition = "2021"

[dependencies]
querymt-agent-session-core = { path = "$REPO_ROOT/crates/agent-session-core" }

[workspace]
EOF
cat >"$WORK/src/lib.rs" <<'EOF'
use querymt_agent_session_core::SessionCore;

pub fn new_view() -> SessionCore {
    SessionCore::new("s1")
}
EOF

cd "$WORK"
export CARGO_TARGET_DIR="${SESSION_CORE_CHECK_TARGET_DIR:-$WORK/target}"

echo "cargo check (native)"
cargo check --quiet
echo "cargo check (wasm32-unknown-unknown)"
cargo check --quiet --target wasm32-unknown-unknown

echo "cargo tree: forbidden dependencies"
forbidden="$(cargo tree --edges normal --target all --prefix none \
  | grep -E '^(tokio|rusqlite|querymt-workspace-hack) ' || true)"
if [ -n "$forbidden" ]; then
  echo "error: forbidden dependencies in the consumer's tree:" >&2
  echo "$forbidden" >&2
  exit 1
fi
echo "ok: no tokio, rusqlite or querymt-workspace-hack"
