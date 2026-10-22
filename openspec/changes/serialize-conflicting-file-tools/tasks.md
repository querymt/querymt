# Tasks

## 1. File access declaration

- [x] 1.1 Add `FileAccess` enum and `Tool::file_access(&self, args, cwd) -> FileAccess` (default `None`) in `crates/agent/src/tools/context.rs`, re-export from `tools/mod.rs`; verify `cargo check -p querymt-agent` passes with no tool changes
- [x] 1.2 Add shared `normalize_tool_path(raw, cwd)` helper (resolve, lexical clean, canonicalize deepest existing ancestor, fall back to cleaned absolute path); verify unit tests for `./a.md` vs `a.md` vs absolute, `..` segments, non-existent file in existing dir, symlinked dir
- [x] 1.3 Implement `file_access` for `edit` and `multiedit` (writes `filePath`), `write_file` and `delete_file` (writes `path`); verify unit tests per tool, including invalid args returning `None`
- [x] 1.4 Implement `file_access` for `replace_symbol` (writes every `requests[].path`, resolved against `root` the same way as the tool); verify a unit test with two files and a custom `root`
- [x] 1.5 Implement `file_access` for `shell` (`Unknown`) and for `read_tool` (reads its path); verify unit tests

## 2. Conflict-aware wave scheduling

- [x] 2.1 Implement pure `plan_waves(&[CallInfo]) -> Vec<Wave>` with `conflicts()` rules from design D3 (write/write, read/write, Unknown vs writer, barrier classes as solo waves); verify unit tests covering: same-file edits split into ordered waves, different-file edits in one wave, parallel reads, read-after-write ordering, shell vs edit, shell alongside reads, multi-file `replace_symbol` vs single edit, `None`-access tool alongside edit, barrier ordering
- [x] 2.2 Replace both batch paths in `transition_processing_tool_calls` (`transitions.rs` mixed-boundary and all-parallel fast path) with a single wave executor using `plan_waves`, keeping per-call cancellation `select!`, serial-stateful state reload, clarification-boundary skipping, and cancelled/skipped synthetic results; verify existing scheduling tests in `agent/execution_tests.rs` pass unchanged
- [x] 2.3 Make `store_all_tool_results` stable-sort results by `call_id` position in the last assistant message's tool calls (unknown ids keep relative order at the end); verify a unit test with pre-filled out-of-order results
- [x] 2.4 Add execution tests: two real `edit` calls on one temp file in one batch both persist (regression for the incident); edit A then B where B's `oldString` only exists after A succeeds; cancellation while a conflicting call waits yields exactly one result per call; verify with `cargo nextest run -p querymt-agent execution`
- [x] 2.5 Add `tracing` span fields for wave count / serialized call count on the tool batch span and remove the now-unused `execution_mode` variants; verify `cargo clippy -p querymt-agent -- -D warnings` passes

## 3. Parallel edit guard middleware

- [x] 3.1 Create `crates/agent/src/middleware/parallel_edit_guard.rs` with `ParallelEditGuard { registry, mode, pending: Mutex<HashMap<session_id, String>> }` and detection of 2+ writes to one normalised path (using `runtime.cwd`, `file_access`, `normalize_tool_path`) only on fresh batches; verify unit tests: two edits same file flagged, different files not flagged, shell not counted
- [x] 3.2 Implement warn mode: pass state through, store note, inject once via `inject_message` in `on_step_start` on `BeforeLlmCall`; `reset` clears pending; verify tests that the note names the file and mentions `multiedit`, appears after tool results, and is injected only once
- [x] 3.3 Implement deny mode: keep first writer per flagged path plus non-writers in `remaining_calls`, move later writers to error `results` naming the file and recommending `multiedit`; verify tests for one result per call, unaffected calls still run, and correct result ordering after 2.3
- [x] 3.4 Add `ParallelEditGuardFactory` (`type = "parallel_edit_guard"`, `mode`, `enabled` with dedup-style "disabled" error) and register it in `middleware/factory.rs`; verify factory tests for default warn, explicit deny, invalid mode error, and disabled
- [x] 3.5 Auto-add a warn-mode guard in `api/agent.rs` (and `api/quorum.rs` middleware assembly) when no driver named `ParallelEditGuard` exists and no `parallel_edit_guard` config entry is present (including disabled); verify builder tests: default agent has the guard, explicit deny config yields a single deny instance, disabled config yields none
- [x] 3.6 Document `parallel_edit_guard` (modes, default, disable) in `docs/docs/agent/middleware.md` and add a config example in `docs/docs/agent/configuration.md`; verify the documented TOML parses via a config test or an existing example config loading test

## 4. Integration checks

- [x] 4.1 End-to-end test with a mock provider emitting one response with two `edit` calls on the same file: both edits persist, and the next request contains the warn guidance after the tool results; verify with `cargo nextest run -p querymt-agent`
- [x] 4.2 Run full `cargo nextest run -p querymt-agent` and `cargo clippy --workspace -- -D warnings`; verify both pass

## Workflow follow-up

- Archive the change after review and merge.
