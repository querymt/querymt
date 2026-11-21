# Proposal

## Why

The state of one agent run in `querymt-agent-runtime` is spread over six places with different lifetimes: the immutable `ExecutionState` and `ConversationContext`, eight loop-local counters, `ExecutionContext`, mutex fields on the session-wide `SessionRuntime`, fields on middleware objects that every session of an agent shares, and `SessionActor` fields. Only the first is threaded through the loop. The rest is ambient, and it has already produced defects that were found by reading the code:

- **Duplicate-code review runs once per process.** Its "already reviewed this turn" flag is cleared only by `MiddlewareDriver::reset`, which no production code calls. After the first review, no later run of any session is reviewed.
- **The review can continue a run with another session's conversation.** It rebuilds the conversation from the last context any session stored on the shared middleware object.
- **Context-usage warnings repeat.** One "last model" slot is shared by all sessions, so two sessions on different models clear each other's "already warned" flag on every step.
- **Changed-file tracking is never cleared** unless the review above runs, so it grows for the life of a session.
- **Scheduled runs record no run start.** The run launch sequence exists twice, and only the interactive copy emits `run_started`; both emit `run_completed`.
- **Five middleware types are dead weight.** `DelegationGuardMiddleware` reads the last stored message while the reply it should judge is not stored yet, so by reading it cannot fire; no preset or config factory builds it or `DuplicateToolCallMiddleware`. `MaxStepsMiddleware`, `TurnLimitMiddleware` and `PriceLimitMiddleware` repeat what `LimitsMiddleware` does and are used by one test file.
- **History is copied whole several times per model request** (4 to 7 deep copies of every message, counted in design "Context"), and each transition writes it twice: once to the store and once to an in-memory copy patched by hand.
- **The run holds its history in the provider's message type, not the stored one.** What only the harness needs (which notice is the mode reminder, which tool list a request starts from) cannot be read from a provider message, so it is kept in fields beside the messages that three sites refresh by hand, and it once leaked into `querymt`'s public tool-change type, where every provider and the plugin boundary saw it (removed in `reduce-agent-harness-overhead` task 10.9). The same split gives four token estimates that count different parts of one history, and a second message projection for compaction.

The same structure makes the loop hard to change safely: `ExecutionState` is at once loop position, middleware verdict and run outcome, so the loop needs an `unreachable!`, a placeholder state swap and five pass-through match arms.

## What Changes

The groups can land separately, in this order.

**A. Defects (no API change)**

- Duplicate-code review runs once in every run that changed files, on that run's changes and conversation.
- A context-usage warning is stored at most once per run. **Behaviour change**: today a single-session process warns once until the model changes, and a multi-session process can warn on every step.
- A scheduled run records `run_started` with `origin = "scheduled"` before its other run events. **Behaviour change** (event stream): one more durable event per scheduled run.
- Changed files are tracked per run and dropped when the run ends.

**B. One owner for run state**

- Run state gets one owner with exclusive access. Read-only run inputs are separated from it. Nothing run-scoped stays on `SessionRuntime`, and the run state holds no lock, atomic or cell.
- **BREAKING** (public Rust API): the public fields `SessionRuntime::turn_snapshot`, `pre_turn_snapshot_task` and `turn_diffs` are removed, and `querymt_agent_services::accounting::BudgetWarnings::take_new` takes `&mut self`.
- The loop's position and its outcome become separate types, so a finished run cannot be resumed by accident and every loop arm handles exactly the positions it can be in.
- History has one writer. A message is appended to the store and to the run's history by one operation, existing messages are never copied to append one, and a model request copies the messages at most once.
- The run's history is the stored messages. One projection turns them into what a provider is sent, for a model request and for a compaction request alike, and it is the only place that decides how a notice, a tool baseline, a tool change and provider-native output are sent. The tool baseline and the mode reminder are read from the history when needed, not kept in fields beside it.
- One token estimate. **Behaviour change** (numbers in events): the token counts of `compaction_start` and `compaction_end` and the context size recorded after a compaction are computed by the estimate the request budget already uses, so they differ from today for histories with steering, reasoning or tool changes.
- **Behaviour change** (stored order): when steering arrives before a tool batch, the skipped tool results are stored before the steering message, which is the order the model is sent. Today the store holds the opposite order.

**C. Middleware contract**

- **BREAKING** (public Rust API, `querymt_agent::middleware`): `MiddlewareDriver` is replaced by an agent-scoped `Middleware` that creates one `TurnMiddleware` per run. Hooks take `&mut self`, read a borrowed view of the run instead of receiving and returning `ExecutionState`, and return a verdict type that lists only what that hook may decide. `reset`, `on_processing_tool_calls` (no production implementation) and the `ExecutionState` enum are removed from the public API. The six hooks that remain are `on_turn_start`, `on_step_start`, `on_after_llm`, `on_prepared_tool_calls`, `on_turn_end` and `on_stopped`. `CompositeDriver` and `MiddlewareFactory` change signature accordingly.
- **BREAKING** (public Rust API): `DelegationGuardMiddleware`, `DuplicateToolCallMiddleware`, `MaxStepsMiddleware`, `TurnLimitMiddleware` and `PriceLimitMiddleware` are deleted without replacement. `LimitsMiddleware` keeps enforcing step, turn and price limits. Delegation runs through the `delegate` tool and the orchestrator and never used the guard.
- The remaining built-in middleware is ported.

**D. Session actor**

- The three encodings of "a run is active" become one, and interactive, queued and scheduled runs start through one function.
- Phase reporting uses a single-value channel instead of an unbounded one.

**E. Errors and names**

- Run failures use a typed error with a source chain instead of strings wrapped in `anyhow`. `AgentError` values returned to callers do not change.
- Steering boundaries, context-hook triggers, run origins and stop-hook reasons become enums internally. Wire strings do not change.

**F. Less code**

- The non-test line count of the run loop and middleware goes down. When this was written it was 16,968 lines in `middleware/`, `agent/execution/`, `agent/execution_context.rs` and `agent/session_actor.rs`, plus 551 lines of test drivers. The five deleted types alone are 608 of them. The change is not complete while the first number is at or above its baseline (design "D12" has the counting rule).
- The eleven test middleware in `test_utils/drivers.rs` are replaced by one scripted one.

**Dependencies.** `strum` becomes a direct dependency of `querymt-agent-runtime`; it is already compiled for the agent through `agent-client-protocol-schema`. One criterion benchmark is added to `querymt-agent`, which already depends on criterion. Design "D8" lists the crates that were considered and rejected, and why most of the line count comes out without one.

## Non-goals

- A generic monad or effect type. Design "D2" explains why exclusive borrowing of one owned value gives the same guarantee in Rust at lower cost.
- Changing how a model request, a tool call or a compaction works. Their decisions move; their behaviour does not. The compaction request is built by the shared projection with the options it has today (no tool records, no native replay), so its messages do not change. Its retries are not touched here: `unify-model-call-path` moves them to the shared retry path.
- Making `ChatMessage` cheap to clone inside `querymt`. It would remove the last per-request copy and is left to a follow-up decided by the benchmark in this change.
- Changing ACP payloads, config keys, the session DB schema, the Extism contract or Python bindings.
- Splitting `SessionActor` message handling, or the mesh and ACP code in the same file.
- A duplicate-delegation or retry-limit check to replace the deleted guard. If one is wanted it belongs in the `delegate` tool as an error result the model can react to, not in a middleware that stops the run.
- Removing `StopType::DelegationBlocked` or `NoticeSource::DuplicateToolCall`. Both are serialized (events, stored notices) and stay in their enums with no producer.
- Reducing the test line count. Fixtures replace hand-built states, but the change also adds a test per scenario; test lines are recorded, not bounded.
- A compatibility shim for `MiddlewareDriver` (AGENTS.md: no compatibility layers unless asked).

## Capabilities

### New Capabilities

- `agent-middleware`: how middleware takes part in a run: run-scoped state, what each hook may decide, the order of decisions, and the built-in context-usage warning.
- `duplicate-code-review`: when the end-of-run duplicate-code review runs, which changes it reviews and how it continues a run.
- `run-history-consistency`: the history a run sends to the model matches what a load of the stored session would send; one projection builds every request from the stored messages; token estimates of a history agree.

### Modified Capabilities

- `session-run-lifecycle`: scheduled runs record a run start.

## Impact

**Packages**

- `querymt-agent-runtime`: `agent/execution/*`, `agent/execution_context.rs`, `agent/core.rs` (`SessionRuntime`), `agent/session_actor.rs`, `agent/turn_control.rs`, `middleware/*`, `test_utils/drivers.rs`, `delegation/root.rs`. Features whose code changes: none of the gated paths (`remote`, `oauth`, `plugin-loaders`, `api`) contain run-loop code; `test-support` changes because the test drivers are replaced.
- `querymt-agent`: `api/agent.rs` (`AgentBuilder::middleware` bound), the facade re-exports of `middleware`, a new bench `benches/turn_step.rs`. No feature-gated code changes.
- `querymt-agent-services`: `accounting::BudgetWarnings::take_new` (design "D1").
- `querymt-agent-py`, `querymt-mobile-ffi`: build check only; neither exposes middleware.

**Contracts**

- Public Rust API: **BREAKING** in `querymt_agent::middleware` as listed under C, and the two removals listed under B.
- Middleware behaviour: the three behaviour changes under A (review, warning, scheduled run start) and the stored order under B.
- typeshare / ACP: no type changes. `run_started.origin` gains the value `scheduled`; the field is already a string. The stop type `delegation_blocked` is no longer produced.
- Config, hooks, provider wire shapes, Extism contract, session DB schema, Python bindings: no change.

**querymt-desktop follow-up (not planned here):** confirm nothing assumes `run_started.origin` is always `interactive`, and that a scheduled run with a `run_started` event renders as a run.

**Docs:** `docs/docs/agent/middleware.md` (its trait listing already does not match the code), `docs/docs/agent/api_reference.md`, `crates/agent-runtime/knowledge/execution.md`, CHANGELOG "Unreleased".

**In-flight changes to coordinate with:** `unify-model-call-path` resolves a run's provider once, before the first hook, and reads its capabilities at that point; this change's call stage relies on that (design "D2") and does it itself if that change has not landed. `reduce-agent-harness-overhead` edits the same loop. Its task 7.2 removes the progress record in the after-reply step, and group 3 changes cancelled tool-call events. Its background commands (a run origin for wake-ups and a notice drained before each request) moved to a separate change, `add-background-commands`, which should build on the single run-start function from D. Group A here is independent of it. Groups B to D should start after its group 3 lands.
