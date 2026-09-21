# Design

## Context

Tool execution currently invokes `PostToolUse` before computing the optional Merkle snapshot diff. The diff is tied to snapshot policy, while the workspace watcher emits asynchronous debounced batches that cannot guarantee a complete per-tool result. See `proposal.md` and `specs/post-tool-file-effects/spec.md`.

## Goals / Non-Goals

**Goals:**
- Give `PostToolUse` an authoritative per-tool effect set for arbitrary tools.
- Reuse one reconciliation result for hook delivery, turn diffs, and snapshot metadata.
- Avoid filesystem scanning when no configured feature requires effects.

**Non-Goals:**
- Add a new lifecycle event or external change detector.
- Attribute background changes perfectly when they overlap a tool call.
- Expose file contents or patches in the hook payload.

## Decisions

### Compute one post-tool reconciliation before hooks

The executor will determine whether the call needs change tracking before invocation, capture the pre-tool state, execute the tool, and reconcile once immediately afterward. It will then invoke `PostToolUse`, truncate the transformed result, emit completion events, and persist the snapshot metadata.

This reorders the existing diff rather than adding a `ToolEffectsFinalized` event. A separate event was rejected because `PostToolUse` already represents the required lifecycle boundary.

### Decouple effect calculation from snapshot persistence

Introduce an internal reconciliation result containing the post-tree and normalized `DiffPaths`. Snapshot policy determines whether the tree/hash is persisted, not whether the diff can be computed. Tracking is requested when snapshotting, turn-diff consumers, or matching post-tool hooks need it.

The Merkle implementation remains the correctness path because watcher delivery is asynchronous. The workspace actor may later provide dirty-path hints to reduce work, but correctness must not depend on watcher timing.

### Treat potentially mutating tools conservatively

Use the existing mutating-tool policy: explicitly read-only calls can skip pre-state capture, while `assume_mutating` and configured mutating tools enable it. Unknown provider, MCP, and shell tools are tracked under the conservative default. The design does not infer changed files from arguments.

### Add one optional wire field

Extend the internal request and command input with `file_changes: Option<ToolFileChanges>`. `None` means unavailable/not tracked; `Some` with empty vectors means reconciled and clean. Paths are normalized, workspace-relative, sorted, and deduplicated. Rename detection is not promised because the current diff represents renames as remove plus add.

### Preserve output ordering

Hook transformations and additional context are applied to the canonical tool result after reconciliation. Truncation still occurs once after all handlers finish. The `ToolCallEnd` event and persisted result observe the transformed output; snapshot metadata reuses the already-computed reconciliation result.

## Risks / Trade-offs

- [Additional scan latency after mutating tools] -> Gate capture on actual consumers and reuse previous Merkle hashes and the same result across features.
- [Background process writes overlap a tool call] -> Define the effect set as the workspace difference across the tool interval rather than claiming process-level causality.
- [Large change sets inflate hook input] -> Send paths only, apply deterministic count/byte limits, and include truncation metadata if limits are reached.
- [Reordering changes event timing] -> Add ordering tests covering hooks, `ToolCallEnd`, snapshot events, and persistence.

## Migration Plan

1. Add optional schema types and compatibility tests without changing execution order.
2. Refactor reconciliation into a reusable internal result.
3. Move reconciliation before `PostToolUse` and reuse it in snapshot/turn-diff handling.
4. Regenerate hook schema fixtures and document the new field.
5. Roll back by leaving the optional field unset and restoring the old call order; no stored-data migration is required.
