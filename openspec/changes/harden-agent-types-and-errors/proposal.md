# Proposal

## Why

Commit f780076 replaced the string turn and snapshot ids with `TurnId` and `SnapshotId`, but finished only part of the job. A review of `querymt-agent` against the rust-skills guidelines found more of the same in other places, plus problems that are worse than string-typed ids:

- One panic inside a SQL closure poisons the shared connection mutex, and every later storage call then panics.
- Opening a session's event stream takes that same lock synchronously on an async worker.
- A public, unused helper runs LLM-written text through `sh -c`.
- Error causes are flattened into `String` in many places, so retry decisions, logs and API errors lose the cause.

This change fixes all of the review's findings except the `SessionId`/`ScheduleId` newtypes.

## What Changes

- **Storage robustness**
  - The SQLite connection lock no longer poisons. A panic in one storage operation fails only that operation; later operations keep working.
  - Creating a session's event stream no longer blocks an async worker on the storage connection. The durable sequence base is read off the async runtime.
  - The nine copies of the blocking-DB helper become one. A failed blocking task is reported as its own error kind instead of `Other("Task execution failed: …")`.
- **Removals**
  - **BREAKING (public Rust API):** remove `querymt_agent::tasks::run_verification` and `extract_verification_commands`. They have no callers, and they ran substrings of model output as shell commands.
- **Error types**
  - `SessionError` gains typed variants for blocking-task failures. `SessionError::Other` is no longer used where a typed variant fits, and conversion to `LLMError` keeps the source.
  - The kameo actor replies `Rebuild`, `GetOrCreate` and `AdmissionRequest` return typed errors instead of `String`.
  - Delegation claiming retries only transient storage errors (busy/locked). Other errors fail on the first attempt.
  - **BREAKING (public Rust API):** `HookEngine::new`, `HookEngine::run_stop` and `HooksConfig::validate` return `thiserror` error types instead of `anyhow::Error`.
  - `index::search` returns a typed error instead of `Box<dyn Error + Send>`.
- **Run and turn identity**
  - One run has one id. `run_id` in run events, steering and `ActiveRun`, and `turn_id` in hook payloads and snapshot records, all come from the same `TurnId`. The value is no longer kept as a parallel `String` copy.
  - Hook payloads never carry an empty `turn_id`. Hook-emitting paths that run outside a prompt turn (for example native `/compact`) mint their own turn id.
  - `TurnId` no longer implements `Default`.
- **Typed event kinds**
  - Each journal event's kind tag comes from one accessor on the event kind, not from serializing the event to JSON and reading `"type"`. Stored tag strings are unchanged.
  - Kind-tag literals in Rust code use that accessor. A test checks that the accessor matches the serde tag for every variant.
  - The forked-session event filter decodes typed events instead of digging through `serde_json::Value`.
- **Typed states**
  - Todo `status` and `priority` become enums.
  - The schedule `state` in `ScheduleInfo` becomes the existing `ScheduleState` enum.
  - `PendingInputSummary.delivery`/`state`, the ATIF export attempt state and the `get_symbol` `context_mode` become enums.
  - Wire strings are unchanged.
- **Todo storage**
  - Todo lists move from a process-global map into per-session state. They are removed when the session is deleted, so they no longer leak for the life of the process.
- **Small cleanups**
  - Anonymous tuples in undo patch resolution and fork row loading become named structs.
- **Changelog**
  - A CHANGELOG "Unreleased" entry covers the breaking API removals and the hook `turn_id` guarantee.

## Non-goals

- `SessionId`, `ScheduleId`, `TaskId` and other `*_public_id` newtypes. That is a separate, incremental change.
- Changing any wire string, stored JSON shape, event kind tag or DB schema. No migrations.
- Auditing `as` casts, `let _ = …await` result drops and remaining `unwrap()` outside storage locks. Each needs its own review.
- Removing `anyhow` from the application-facing `api::*` surface (`QueryMTAgent` methods). Only the library-level hooks API changes.
- UI work in querymt-desktop.

## Capabilities

### New Capabilities

- `session-storage-reliability`: the session store keeps serving after a failed operation, event stream setup does not stall async execution, and storage failures keep their cause.
- `session-todos`: behaviour of the `todo_write`/`todo_read` tools: per-session isolation, enumerated status and priority, summary counts, and cleanup on session delete.

### Modified Capabilities

- `session-run-lifecycle`: adds the requirement that a run has one identifier, shared by run lifecycle events, steering, hook payloads and turn snapshot records, and that hook payloads never carry an empty turn id.

## Impact

- **Packages:** `querymt-agent` only. Downstream crates that compile against the changed public API need a rebuild check: `querymt-agent-py`, `querymt-mobile-ffi`. Feature-gated code touched: `remote` (`agent/remote/node_manager.rs` admission reply). No provider, Extism or `querymt` core changes.
- **Public Rust API:**
  - **BREAKING:** `tasks::run_verification` and `tasks::extract_verification_commands` are removed.
  - **BREAKING:** hooks error types change from `anyhow` to typed errors.
  - **BREAKING:** `SessionError` gains variants; it is not `#[non_exhaustive]`, so exhaustive matches break.
  - **BREAKING:** `ActiveRun.run_id`, `SteeringInbox::new` and the related turn-control types take `TurnId`.
  - `TurnId: Default` is removed.
  - `ExecutionContext::turn_id_string` is removed.
- **Hook behaviour (BREAKING, documented):** `turn_id` in hook command input is always a non-empty UUID string. Before, it could be `""` for hooks fired outside a prompt turn.
- **Wire/typeshare:**
  - `ScheduleInfo.state` and `PendingInputSummary.delivery`/`state` change from `string` to string-literal union types in typeshare output. The JSON stays the same.
  - `SubmitInputResult.run_id` stays a string on the wire.
  - **UI follow-up:** querymt-desktop should regenerate its typeshare types and narrow its handling of these fields.
- **Mesh wire (BREAKING, feature `remote`):** the error arm of the `querymt::AdmissionRequest` reply changes from a string to a structured rejection. Nodes on different versions report a decode error instead of the rejection reason.
- **ACP payloads:** unchanged.
- **Config:** unchanged.
- **DB schema:** unchanged.
- **Python bindings:** unchanged.
- **Stored event JSON:** unchanged.
- **Docs:** `docs/docs/agent/hooks.md` (the `turn_id` guarantee).
