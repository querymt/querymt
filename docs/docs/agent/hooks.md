# QueryMT Agent - Hooks

Hooks let you run configured local commands at specific points in the agent lifecycle. They are useful for policy checks, audit logging, approval automation, tool input rewriting, and stop-time validation.

## Overview

Hooks are configured at the agent/profile level through `[agent.hooks]`. A hook command receives JSON on stdin and returns JSON on stdout. QueryMT validates the wire format with generated schemas.

Hooks are a good fit for:

- enforcing local tool policies without recompiling QueryMT
- review or planning profiles with stricter approval rules
- custom shell safety checks
- stop-time verification before a turn ends
- schema-backed lifecycle command hooks

## Security Model

Hooks execute arbitrary local commands.

- Hooks are disabled by default.
- Enable hooks only in trusted agent configs or profiles.
- Hook commands receive prompt and tool metadata on stdin.
- For now, QueryMT supports config/profile-level hooks only.
- Automatic `~/.qmt/hooks` or project `.qmt/hooks` discovery is not implemented yet.

## Configuration

Hooks are configured under `[agent.hooks]`.

```toml
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
cwd = "."

[agent.hooks]
enabled = true

[[agent.hooks.pre_tool_use]]
matcher = "^shell$"

[[agent.hooks.pre_tool_use.hooks]]
type = "command"
command = "sh ./hooks/check-shell.sh"
timeout_sec = 5
status_message = "Checking shell command"

[[agent.hooks.stop]]

[[agent.hooks.stop.hooks]]
type = "command"
command = "sh ./hooks/stop-verify.sh"
timeout_sec = 5
```

Because hooks live in `[agent.hooks]`, they work well with QueryMT profiles. For example, a review profile can enable stricter `stop` hooks while a coding profile can enable shell-policy hooks. See `crates/agent/examples/confs/hook_guarded_coder.toml` and the companion scripts in `crates/agent/examples/hooks/` for a complete runnable example.

## Hook Command Protocol

Each hook command:

- runs as a local process
- receives one JSON object on stdin
- returns one JSON object on stdout
- may return an empty JSON object (`{}`) when it has no action to take

Common input fields include:

```json
{
  "session_id": "session-id",
  "turn_id": "turn-id",
  "transcript_path": null,
  "cwd": "/workspace",
  "hook_event_name": "pre_tool_use",
  "model": "claude-sonnet-4-5-20250929",
  "permission_mode": "plan"
}
```

`permission_mode` is derived from the agent mode captured when the turn starts. If the user changes mode while a turn is running, hooks for that turn continue using the captured mode; the next turn uses the new mode.

Current values are:

- `default`: Build mode
- `plan`: Plan mode
- `accept_edits`: Review mode

## Events

QueryMT currently supports these hook events:

| Event | Matcher | Effect |
|---|---|---|
| `session_start` | none | Observe session creation |
| `user_prompt_submit` | none | Block a prompt |
| `pre_tool_use` | tool name regex | Block or rewrite tool input |
| `permission_request` | tool name regex | Allow or deny permission prompts |
| `post_tool_use` | tool name regex | Mark a tool result as blocked/error |
| `stop` | none | Request one extra LLM step |

## Examples

### pre_tool_use block

Example script for `crates/agent/examples/hooks/check-shell.sh`:

```sh
#!/bin/sh
input="$(cat)"

case "$input" in
  *"rm -rf"*|*"git reset --hard"*)
    printf '{"decision":"block","reason":"Dangerous shell command blocked by local policy"}'
    ;;
  *)
    printf '{}'
    ;;
esac
```

Expected hook output:

```json
{
  "decision": "block",
  "reason": "Shell command touches a protected path"
}
```

### pre_tool_use rewrite

```json
{
  "hook_specific_output": {
    "hook_event_name": "pre_tool_use",
    "updated_input": {
      "command": "cargo test -p querymt-agent --lib"
    }
  }
}
```

### permission_request allow

Example script for `crates/agent/examples/hooks/approve-safe-shell.sh`:

```sh
#!/bin/sh
input="$(cat)"

case "$input" in
  *"cargo test"*|*"cargo check"*)
    printf '{"hook_specific_output":{"hook_event_name":"permission_request","decision":{"behavior":"allow"}}}'
    ;;
  *)
    printf '{}'
    ;;
esac
```

Expected hook output:

```json
{
  "hook_specific_output": {
    "hook_event_name": "permission_request",
    "decision": {
      "behavior": "allow"
    }
  }
}
```

### stop continuation

Example script for `crates/agent/examples/hooks/stop-verify.sh`:

```sh
#!/bin/sh
input="$(cat)"

case "$input" in
  *"validation"*|*"tests"*)
    printf '{}'
    ;;
  *)
    printf '{"continue":false,"reason":"The turn ended without describing validation.","hook_specific_output":{"hook_event_name":"stop","additional_context":"Ask the agent to summarize what validation was run or why it was skipped."}}'
    ;;
esac
```

Expected hook output:

```json
{
  "continue": false,
  "reason": "The turn ended without describing validation.",
  "hook_specific_output": {
    "hook_event_name": "stop",
    "additional_context": "Ask the agent to summarize what validation was run or why it was skipped."
  }
}
```

## Stop Hook Behavior

`stop` runs when a turn would normally complete.

If a `stop` hook returns `"continue": false`, QueryMT runs one additional LLM step for that turn. QueryMT injects a clearly labeled runtime control message into the next LLM call, wrapped as a `<system-reminder>` block and marked as generated by the hook runtime rather than by the user.

To avoid runaway loops, QueryMT currently allows at most one stop-hook continuation per turn.

## JSON Schemas

Generated schemas are committed in the repository and define the stdin/stdout contract for hook authors. QueryMT's snake_case schemas are the source of truth for this feature.

Relevant files include:

- `crates/agent/src/hooks/schema/generated/pre-tool-use.command.input.schema.json`
- `crates/agent/src/hooks/schema/generated/pre-tool-use.command.output.schema.json`
- `crates/agent/src/hooks/schema/generated/permission-request.command.input.schema.json`
- `crates/agent/src/hooks/schema/generated/permission-request.command.output.schema.json`
- `crates/agent/src/hooks/schema/generated/stop.command.input.schema.json`
- `crates/agent/src/hooks/schema/generated/stop.command.output.schema.json`

## Hooks vs Middleware

Hooks and middleware are complementary extension points.

Hooks are profile/config-level command integrations with JSON stdin/stdout contracts. Middleware is compiled Rust code that participates directly in the agent state machine.

A hook policy can often be reimplemented as middleware, but middleware is not a drop-in replacement for hooks. Rewriting a hook as middleware changes how it is authored, distributed, configured, validated, and trusted.

## Hook Notices

When a hook exits successfully but returns invalid or non-JSON stdout, QueryMT ignores that hook output for control-flow purposes and emits a durable `hook_notice` event instead.

`hook_notice` includes:

- `event_name`: the hook lifecycle event such as `pre_tool_use` or `stop`
- `message`: a human-readable warning describing the invalid hook output
- `is_error`: `true` when the notice represents an error condition

This lets dashboards, session timelines, and event subscribers surface hook problems without breaking the turn.

## Current Limitations

- Hooks are configured through agent configs and profiles only.
- Automatic global or project hook discovery is not implemented yet.
- `additional_context` is currently used for `stop` continuation messages, but other hook events do not yet inject it back into the model context.
- Hook input JSON is produced from typed Rust structs and not runtime-validated against JSON Schema; the generated schemas serve as the documented/tested contract.
