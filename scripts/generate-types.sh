#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Verify typeshare CLI is available
if ! command -v typeshare &> /dev/null; then
    echo "Error: typeshare CLI not found. Install with: cargo install typeshare-cli"
    exit 1
fi

TS_OUT="$REPO_ROOT/crates/agent/ui/src/generated/types.ts"

echo "Generating TypeScript types..."
typeshare "$REPO_ROOT/crates/agent" \
  --lang=typescript \
  --output-file="$TS_OUT"

echo "Done."
