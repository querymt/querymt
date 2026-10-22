# Design

## Context

See proposal.md (Why) for the incident. The relevant current state:

- `Tool::execution_class()` (`crates/agent/src/tools/context.rs:240`) defaults to `ParallelSafe`. Only `question` (clarification), `task`/`create_task` (serial-stateful) override it. No file tool does.
- `transition_processing_tool_calls` (`crates/agent/src/agent/execution/transitions.rs:1620`) has **two** batch paths:
  - a mixed-boundary path (~1665-1872) that groups consecutive `ParallelSafe` calls and runs each group with `join_all`;
  - an all-parallel fast path (~1874-1960) that runs the whole batch with `join_all`. This is the common case and the one hit in the incident.
- `edit`, `multiedit`, `replace_symbol`, `write_file` and `delete_file` each do an unguarded `read_to_string` and then `fs::write` (or `remove_file`).
- Middleware hooks: `on_processing_tool_calls` runs before the batch executes and can rewrite `remaining_calls` and `results`. `on_step_start` runs on `BeforeLlmCall`, after tool results are stored. `ConversationContext::inject_message` appends a user message. Factories get `&AgentConfig` (which includes `tool_registry`). Hooks get `Option<&Arc<SessionRuntime>>` (which includes `cwd`).
- Cross-session and delegate isolation is handled by per-session git worktrees (separate branch), so only conflicts inside one batch of one session matter here.

## Goals / Non-Goals

**Goals:**
- One scheduling algorithm for both batch paths, implemented as a pure function that can be unit-tested.
- Declaring file access is opt-in per tool, and the declaration is shared by the scheduler and the guard middleware.
- Keep the maximum parallelism that is safe.

**Non-Goals:**
- No OS-level file locks, no in-tool compare-and-swap, no atomic temp-file writes (dropped, given worktrees; can be revisited).
- No coordination with writers outside the batch (user editor, other processes, other sessions).
- No parsing of shell commands to infer the paths they touch.
- No change to how tool calls are ordered across *different* model responses (those are already sequential).

## Decisions

### D1. Declare file access per call: `Tool::file_access`

```rust
pub enum FileAccess {
    None,                                            // default
    Paths { reads: Vec<PathBuf>, writes: Vec<PathBuf> },
    Unknown,                                         // e.g. shell
}
fn file_access(&self, args: &Value, cwd: Option<&Path>) -> FileAccess { FileAccess::None }
```

The method lives on the `Tool` trait, next to `execution_class`. It takes the raw arguments, so `replace_symbol` can report every `requests[].path`, and `edit` can report `filePath`.

- *Alternative: a new `ExecutionClass::SerialFileWrite`.* Rejected. It would serialize edits to *different* files, losing the parallelism the user wants to keep.
- *Alternative: a hard-coded table of tool names in the scheduler.* Rejected. It wouldn't extend to new or third-party tools and would duplicate argument parsing.
- **Arguments that fail to parse return `FileAccess::None`.** The tool will then fail its own validation, and a failing call doesn't write anything.

Declared access per tool:

| Tool | Access |
|---|---|
| `edit`, `multiedit` | writes `filePath` |
| `write_file`, `delete_file` | writes `path` |
| `replace_symbol` | writes each `replacements[].path`, resolved exactly like the tool (`ToolContext::resolve_path`; a relative path is joined with the cwd, so `root` only has to resolve and never changes which file is named) |
| `shell` | `Unknown` |
| everything else | `None` (default) |

Read-only file tools declare their reads, so a read is ordered against a write of the same file (or of a file beneath a directory they read). `read_tool`, `get_function`, `get_symbol`, `index`, `mdq` and `find_references` read the files they name; `search_text`, `ls` and `glob` read the directory they walk (`path`, default cwd). Path overlap is containment (equal, or one is an ancestor of the other), so `search_text` over `src/` waits for an `edit` of `src/a.rs`. Tools that declare nothing stay `None`.

### D2. Normalise paths in one shared helper

`normalize_tool_path(raw, cwd) -> PathBuf`:
1. Resolve against cwd, the same way as `ToolContext::resolve_path`.
2. Lexically clean `.` and `..`.
3. Canonicalize the longest existing prefix of the *uncleaned* path (so `link/../x` follows the symlink target as the OS does) and append the remaining components, resolving `.`/`..` lexically. Canonicalization uses `dunce` so Windows paths never carry the `\\\\?\\` verbatim prefix.

The scheduler and the middleware both use it. That keeps `./a.md`, `a.md`, the absolute path and symlinked paths as the same key. The helper does not try to handle case-insensitive file systems; see Risks.

### D3. A pure wave planner plus one wave executor

`plan_waves(calls: &[CallInfo]) -> Vec<Wave>`, where `CallInfo = { class, access }` and a wave is a list of call indices that are safe to run together.

Algorithm (keeps the model's order, places each call as early as possible):

```
for each call i in order:
  if class(i) != ParallelSafe:         -> close current wave, emit [i] alone as a barrier wave
  else:
    earliest = index after the last barrier wave
    for each earlier call j in waves >= earliest:
       if conflicts(i, j): earliest = max(earliest, wave(j) + 1)
    put i in wave `earliest` (create it if needed)
```

`conflicts(a, b)`:
- both declare paths and `writes(a) ∩ (reads(b) ∪ writes(b)) ≠ ∅` or the reverse;
- or one is `Unknown` and the other has non-empty `writes`.

A call only moves to a later wave than every call it conflicts with. That guarantees model order for each conflicting pair, while unrelated calls stay in early waves.

The executor runs waves in order. Each wave uses the existing per-call `tokio::select!` with the cancellation token, inside `join_all`. Results are put back in the original call order before they are stored. This replaces *both* the fast path and the mixed-boundary path. The barrier handling for `SerialStateful` and `ClarificationBoundary` stays the same: state reload after serial-stateful calls, intent update after a clarification, then skip the remaining calls. Those become actions taken when a barrier wave runs.

On cancellation, every call that hasn't run yet gets the existing synthetic "Cancelled by user" result. The rule "one result per call id" is preserved.

- *Alternative: a per-path async mutex taken inside `execute_tool_call`, keeping `join_all`.* It's simpler, but the order of competing writes is non-deterministic (whichever task polls first). That breaks model order and the "B depends on A" scenario. Rejected.
- *Alternative: only patch the fast path.* Rejected. The mixed path has the same bug for runs of consecutive parallel calls.

### D4. The `ParallelEditGuard` middleware

- **Construction.** Built by `ParallelEditGuardFactory` (`type = "parallel_edit_guard"`). Config: `mode = "warn" | "deny"` (default `warn`), `enabled` (default `true`). Following the `dedup_check` convention, `enabled = false` returns a "disabled" error, which the builder skips. The guard keeps a clone of the tool registry from `AgentConfig`.
- **Detection** (`on_processing_tool_calls`, only when `results.is_empty()`, i.e. a fresh batch):
  - Compute `file_access` for each call, using `runtime.cwd` and D2.
  - Group the writing calls by normalised path; any path with two or more writes is flagged.
- **Warn mode.** State passes through unchanged. A pending note is stored in `Mutex<HashMap<session_id, String>>`. In `on_step_start` (`BeforeLlmCall`) the note is taken (removed) and added with `context.inject_message`. This places it after the tool results, so `tool_use`/`tool_result` adjacency holds for Anthropic, and the note is sent only once.
- **Deny mode.**
  - Keep the first writing call for each flagged path, plus all non-writing calls.
  - Every later writing call for a flagged path becomes a `ToolResult` error ("Rejected: N writes to `<path>` in one response. Combine them into a single `multiedit` call.") in `results`.
  - The new `ProcessingToolCalls` contains the reduced `remaining_calls`.
  - This depends on the order fix in D5.
- **Default on.** In `api/agent.rs`, next to the existing ContextMiddleware auto-add: if no driver is named `ParallelEditGuard` *and* the config has no `parallel_edit_guard` entry (including a disabled one), push a warn-mode instance. Apply the same in `api/quorum.rs` where middleware entries are built.
- **Event.** The guard publishes a `HookNotice` event (`event_name = "parallel_edit_guard"`) so UIs can show the warning. It gets the event sink and the live tool registry through `MiddlewareDriver::bind_agent`, called when the `AgentConfig` is assembled, so it always agrees with the scheduler about which tools exist.
- **Stale notes.** A pending note is dropped at turn start and when the turn ends (`Complete`/`Stopped`/`Cancelled`); the wording is neutral about whether the calls succeeded.
- **Every batch is inspected.** Calls gated earlier for invalid arguments pre-fill `results`; this does not disable detection. Deny mode appends its rejections to existing results.
- **Middleware order.** The auto-added guard is pushed last in both `api/agent.rs` and `api/quorum.rs`, so its note is the final message of the request.

- *Alternative: a context fragment.* Rejected in discussion (less visible to the model).
- *Alternative: a new post-execution hook that annotates results.* Rejected (needs a new hook; option (a) was chosen).

### D5. Keep result order when middleware pre-fills `results`

Today the executor appends new results after `results`. In deny mode, the pre-filled rejected results would then come *before* the earlier calls' results. The history needs results in call order. Two options:

- sort `all_results` by the original call order before storing, or
- have the guard keep the original call list and let the executor merge.

**Choice:** `store_all_tool_results` already gets the assistant message's call list through the context. The executor sorts `all_results` by the position of each `call_id` in the original assistant tool calls before storing. If that list can't be reached, it falls back to sorting by the original `remaining_calls` order plus pre-filled ids. This also fixes a latent ordering issue in the steering-skip path. It's a small change, verified by a test.

## Risks / Trade-offs

- **Shell is `Unknown`, so it waits for writing tools in the same batch.** → Small loss of parallelism, by design (confirmed with user).
- **Case-insensitive file systems (macOS default, Windows).** Two spellings of a *not-yet-existing* file that differ only by case won't be detected as the same file. → Rare. Existing files are handled by canonicalisation.
- **Paths outside the workspace or with odd symlinks.** Canonicalisation could fail. → Fall back to the lexically cleaned absolute path. Worst case, two calls run in parallel as they do today.
- **Restructuring a central executor.** → `plan_waves` is pure and tested on its own. The existing scheduling tests in `agent/execution_tests.rs` (`scheduling_tool`) must keep passing unchanged, and new tests cover conflicts and cancellation.
- **Deny mode costs the model a round-trip.** → It's opt-in; warn is the default.
- **Tool calls whose arguments get rewritten later** (e.g. by hooks or permission edits) might have different paths at execution time than at planning time. → Plan using the arguments as they appear in `remaining_calls`, the same ones passed to `execute_tool_call`.

## Migration Plan

No data or config migration. Rollback means reverting the commit. The guard can be turned off with `[[middleware]] type = "parallel_edit_guard"` and `enabled = false`, without a code change.

## Open Questions

- Whether to add a generic "middleware warning" `AgentEventKind` so UIs can show guard warnings. This doesn't affect the specs or the scheduling design.
t the specs or the scheduling design.
