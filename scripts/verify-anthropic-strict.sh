#!/usr/bin/env bash
# Live verification of Anthropic strict tool use (openspec change
# harden-tool-argument-handling, task 7.7).
#
# For each model, sends small requests over both authentication paths:
#   A  API key, converted schema,    strict:true  -> expect 200 + tool_use
#   B  OAuth,   converted schema,    strict:true  -> expect 200 + tool_use
#   C  API key, UNconverted schema,  strict:true  -> expect 400 (strict is enforced)
#   D  OAuth,   UNconverted schema,  strict:true  -> expect 400
#
# Credentials: ANTHROPIC_API_KEY from the environment, OAuth access token from
# the macOS keychain (service "querymt", account "oauth_anthropic"). Secrets are
# passed to curl via `-H @-` on stdin, never as arguments, and are never
# printed. Output is limited to status, error type/message and tool_use presence.
#
# Usage: scripts/verify-anthropic-strict.sh [model ...]
set -euo pipefail

MODELS=("$@")
[ ${#MODELS[@]} -eq 0 ] && MODELS=(claude-sonnet-4-5-20250929 claude-sonnet-5-5)
API=https://api.anthropic.com/v1/messages
OAUTH_VERSION=2.1.284

: "${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY is not set}"

OAUTH_TOKEN="$(security find-generic-password -s querymt -a oauth_anthropic -w 2>/dev/null |
  python3 -c '
import sys, json, time
d = json.load(sys.stdin)
if d.get("expires_at", 0) <= time.time():
    sys.exit("EXPIRED")
print(d["access_token"])' )" || { echo "OAuth token missing or expired: run 'qmt auth login'" >&2; exit 2; }

# build_body <model> <converted|original> <prefix>
build_body() {
  python3 - "$1" "$2" "$3" <<'EOF'
import sys, json, copy
model, which, prefix = sys.argv[1:4]
# Shape of the agent's `replace_symbol` builtin parameters (descriptions shortened).
schema = {
    "type": "object",
    "properties": {
        "replacements": {
            "type": "array",
            "description": "reqs",
            "items": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."},
                    "symbol": {"type": "string", "description": "sym"},
                    "kind": {"type": "string", "description": "kind", "default": "any"},
                    "occurrence": {"type": "integer", "description": "0-based occurrence when multiple symbols match.", "default": 0, "minimum": 0},
                    "expected_hash": {"type": "string", "description": "hash"},
                    "new_text": {"type": "string", "description": "body"},
                },
                "required": ["path", "symbol", "new_text"],
                "additionalProperties": False,
            },
        },
        "root": {"type": "string", "description": "Workspace root directory.", "default": "."},
    },
    "required": ["replacements"],
    "additionalProperties": False,
}
if which == "converted":
    # Same transformation strict_schema::convert applies to this schema
    # (covered by a golden test): minimum moves into the description.
    schema = copy.deepcopy(schema)
    occ = schema["properties"]["replacements"]["items"]["properties"]["occurrence"]
    occ.pop("minimum")
    occ["description"] += " (minimum: 0)"
name = "Replace_symbol" if prefix else "replace_symbol"
body = {
    "model": model,
    "max_tokens": 256,
    "messages": [{"role": "user", "content":
        "Call the tool once to replace symbol `foo` in `src/a.rs` with `fn foo() {}`."}],
    "tools": [{"name": prefix + name, "description": "Replace symbol bodies.",
               "input_schema": schema, "strict": True}],
    "tool_choice": {"type": "any"} if False else {"type": "auto"},
}
if prefix:
    body["system"] = [{"type": "text",
        "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."}]
print(json.dumps(body))
EOF
}

SUMMARIZE=$(cat <<'PY'
import sys, json
raw = sys.stdin.read()
body, _, status = raw.rpartition("\nHTTP ")
status = status.strip()
try:
    d = json.loads(body)
except Exception:
    print("HTTP " + status + "  unparseable body")
    sys.exit()
if "error" in d:
    e = d["error"]
    print("HTTP " + status + "  error=" + str(e.get("type")) + "  message=" + str(e.get("message"))[:200])
else:
    tu = [b for b in d.get("content", []) if b.get("type") == "tool_use"]
    print("HTTP " + status + "  stop_reason=" + str(d.get("stop_reason")) + "  tool_use=" + str(bool(tu)))
PY
)

# send <auth: api|oauth> <body>
send() {
  local auth="$1" body="$2" out
  if [ "$auth" = api ]; then
    out="$(printf 'x-api-key: %s\n' "$ANTHROPIC_API_KEY" |
      curl -sS -m 60 -H @- -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' \
        -d "$body" -w '\nHTTP %{http_code}' "$API")"
  else
    out="$(printf 'authorization: Bearer %s\n' "$OAUTH_TOKEN" |
      curl -sS -m 60 -H @- -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' \
        -H 'anthropic-beta: oauth-2025-04-20,interleaved-thinking-2025-05-14' \
        -H "user-agent: claude-cli/$OAUTH_VERSION (external, cli)" \
        -d "$body" -w '\nHTTP %{http_code}' "$API?beta=true")"
  fi
  printf '%s' "$out" | python3 -c "$SUMMARIZE"
}

for model in "${MODELS[@]}"; do
  echo "== $model"
  printf 'A api   converted strict : '; send api   "$(build_body "$model" converted "")"
  printf 'B oauth converted strict : '; send oauth "$(build_body "$model" converted "mcp_")"
  printf 'C api   original  strict : '; send api   "$(build_body "$model" original "")"
  printf 'D oauth original  strict : '; send oauth "$(build_body "$model" original "mcp_")"
done
