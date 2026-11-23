#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Verify typeshare CLI is available
if ! command -v typeshare &> /dev/null; then
    echo "Error: typeshare CLI not found. Install with: cargo install typeshare-cli"
    exit 1
fi

TS_OUT="$REPO_ROOT/target/generated/types.ts"
mkdir -p "$(dirname "$TS_OUT")"

echo "Generating TypeScript types..."
typeshare \
  "$REPO_ROOT/crates/agent" \
  "$REPO_ROOT/crates/agent-core" \
  "$REPO_ROOT/crates/agent-index" \
  "$REPO_ROOT/crates/agent-knowledge" \
  "$REPO_ROOT/crates/agent-services" \
  "$REPO_ROOT/crates/agent-runtime" \
  "$REPO_ROOT/crates/querymt" \
  "$REPO_ROOT/crates/querymt-remote" \
  --lang=typescript \
  --output-file="$TS_OUT"

# Typeshare 1.x cannot emit internally tagged unions with fields. Keep these
# flat shapes in sync with the serde wire enums rather than changing the wire.
cat >>"$TS_OUT" <<'TS'

export type ModelSelection =
  | { mode: "all" }
  | { mode: "selected"; model_ids: string[] };

export type PeerGrant =
  | { mode: "none" }
  | { mode: "full" }
  | { mode: "custom"; sessions: boolean; filesystem: boolean; shell?: boolean; providers: ProviderGrant[] };

export type MeshQuotaLimit =
  | { kind: "unlimited" }
  | { kind: "finite"; limit_tokens: string; window?: QuotaWindow };

export type MeshPeerIncomingAccess =
  | { status: "granted"; effective: MeshPeerEffectiveAccess; quotas: MeshPeerQuota[] }
  | { status: "denied" | "legacy" | "unavailable" | "incompatible" };
TS

echo "Done."
