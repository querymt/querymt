# Proposal

## Why

When one assistant message carries several tool calls (Anthropic models routinely do this), the executor runs every `ParallelSafe` call concurrently via `join_all`. File-mutating tools (`edit`, `multiedit`, `replace_symbol`, `write_file`, `delete_file`) perform an unguarded read-modify-write, so two calls targeting the same file race and the later write silently erases the earlier one while both report success. This was proven in session `01a162a1`: two `edit` calls on `design.md` in message 219858 both succeeded, yet one change was lost and the model had to re-apply it. Lost edits that report success are a correctness bug that undermines trust in every multi-call turn.

Cross-session and delegate isolation is handled separately by per-session git worktrees (delegates get their own worktree), so this change only needs to fix conflicts **within a single tool batch** of one session.

## What Changes

- Tools can declare which file paths a given call reads and writes (`Tool::file_access(args, cwd)`), defaulting to "no file access" so existing and third-party tools keep their current behaviour.
- Built-in file tools declare their access: `edit`, `multiedit`, `write_file`, `delete_file` and `replace_symbol` declare writes; `shell` declares unknown access.
- The tool-batch executor becomes conflict-aware. Calls are scheduled into ordered waves: calls that conflict on a path (write/write or read/write), or an unknown-access call (shell) against any writing call, run sequentially in the model's original order. Non-conflicting calls still run in parallel. Existing `SerialStateful` and `ClarificationBoundary` barriers keep their semantics.
- Both batch execution paths in `transition_processing_tool_calls` (the all-parallel fast path and the mixed-boundary path) use the same wave scheduler, so the race cannot reappear through either path.
- A new `parallel_edit_guard` middleware detects batches with more than one writing call to the same file. In the default `warn` mode all calls run (now safely serialized) and a guidance message recommending `multiedit` is injected before the next LLM call. An opt-in `deny` mode executes only the first writing call per file and returns error results for the rest that tell the model to use `multiedit`.
- The guard is enabled by default (in `warn` mode) and can be configured or disabled through `[[middleware]] type = "parallel_edit_guard"`.

## Capabilities

### New Capabilities
- `tool-batch-scheduling`: How the agent executes multiple tool calls from a single model response: parallelism, ordering, and conflict-based serialization for file-touching tools.
- `parallel-edit-guidance`: Detection of multiple same-file mutations in one model response and the guidance (warn) or rejection (deny) returned to the model.

### Modified Capabilities
<!-- None: no existing spec covers tool batch execution or edit guidance. -->

## Impact

- `crates/agent/src/tools/context.rs`: new `FileAccess` type and `Tool::file_access` default method.
- `crates/agent/src/tools/builtins/{edit,multiedit,write_file,delete_file,replace_symbol,shell}.rs`: `file_access` implementations.
- New shared path-normalisation helper (tools module) used by the scheduler and the middleware.
- `crates/agent/src/agent/execution/transitions.rs`: replace both `join_all` batch paths (around lines 1677-1730 and 1874-1914) with a single wave executor built on a pure, unit-testable `plan_waves` function.
- New `crates/agent/src/middleware/parallel_edit_guard.rs`; registration in `middleware/factory.rs`; default auto-enable in `api/agent.rs` (and `quorum.rs` if applicable), alongside how `ContextMiddleware` is auto-added.
- Behaviour change visible to users: same-file mutations in one batch are no longer concurrent, so such batches can take slightly longer; shell calls no longer overlap with writing tools in the same batch.
- No new dependencies; no storage or API schema changes.
