# Proposal

## Why

`PostToolUse` currently runs before QueryMT computes the tool's filesystem diff, so hooks cannot reliably react to the files actually changed by a tool. This forces integrations to infer edits from tool names and arguments, missing changes made by shell commands, formatters, MCP tools, or provider tools.

## What Changes

- Reorder post-tool processing so filesystem effects are reconciled before `PostToolUse` handlers run.
- Add a normalized, optional file-effects payload to `PostToolUse` requests and command-hook input.
- Detect effects independently of tool names, including added, modified, and removed files produced by arbitrary tools.
- Preserve the distinction between unavailable tracking, a completed empty diff, and a non-empty diff.
- Keep snapshot persistence and undo policy separate from the ability to report post-tool file effects.
- Preserve existing hook matching and output behavior for hooks that ignore the new payload.

## Capabilities

### New Capabilities

- `post-tool-file-effects`: Defines post-tool filesystem reconciliation, normalized effect reporting, and delivery through the existing `PostToolUse` lifecycle.

### Modified Capabilities

None.

## Impact

- Affects tool execution ordering in `crates/agent/src/agent/execution/tool_calls.rs`.
- Extends hook request and generated wire schemas under `crates/agent/src/hooks/`.
- Reuses or refactors Merkle/workspace indexing code under `crates/agent/src/index/` without introducing an external watcher.
- Adds filesystem reconciliation cost only when post-tool effects are requested or another existing feature already requires a diff.
- Does not add an LSP dependency or a new lifecycle event.
