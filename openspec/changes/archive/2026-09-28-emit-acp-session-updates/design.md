# Design

## Context

See `proposal.md` for motivation and `specs/acp-session-updates/spec.md` for the behavioral contract.

ACP output is projected primarily in `crates/agent/src/acp/shared.rs`. Live translation is stateful but currently returns at most one notification per internal event, while replay translates persisted `AgentEvent` values independently. The current translator emits message/tool updates and the legacy `plan` variant for `todowrite`, but ignores the session events that already carry mode, provider, request-usage, hook-notice, and compaction data.

The active mode and model are persisted through session-control transitions. Configuration-option construction already has one canonical builder. Request completion events carry context-token and cumulative-cost values, and provider-change events carry the effective context limit. Persisted sessions already have title and update-time fields. Compaction events currently identify no lifecycle instance, report no terminal failure/cancellation, and emit completion before the post-compaction token estimate is calculated.

ACP v1 stable updates may be sent without feature negotiation. Plan operations, notices, and compaction are Preview surfaces with distinct v1 client capabilities and must be gated per connection. The dependency currently enables the ACP crate's broad `unstable` feature, so implementation must verify that the selected crate revision exposes all required v1 types and capability fields.

## Goals / Non-Goals

**Goals:**

- Keep protocol projection centralized and shared by stdio and WebSocket transports.
- Derive every update from authoritative persisted or runtime state rather than estimates invented by the ACP layer.
- Preserve compatibility with v1 clients that understand only stable updates and the legacy plan shape.
- Make stateful entities deterministic or persist their IDs so replay and live traffic agree.
- Guarantee valid compaction ordering and one terminal outcome after a reported start.

**Non-Goals:**

- Add ACP v2 negotiation or alter QueryMT's v1 prompt lifecycle.
- Add client controls for compaction, session renaming, or plan editing.
- Infer usage when either current context occupancy or the effective model limit is unknown.
- Expose hidden prompt framing, encrypted provider state, or other non-user-displayable compaction content.
- Persist notices as conversation history.

## Decisions

### 1. Represent translation as zero-to-many notifications

Change the shared live projection API from one optional notification to an ordered list. Both stdio and WebSocket delivery loops will send every projected notification in order. Replay will use the same update-building helpers where semantics overlap.

This allows one mode transition to produce `current_mode_update` followed by `config_option_update`, and allows a non-streaming compaction result to produce a summary chunk followed by a terminal update. A pending-notification queue inside the existing one-result API was rejected because it complicates transport loops and can strand updates if no later event arrives.

### 2. Carry an immutable capability snapshot in each connection's projector

At successful initialization, derive a small `AcpSessionUpdateCapabilities` value from `ClientCapabilities`: plan operations, notices, and compaction. Store that snapshot in the connection-local translation/delivery context and pass it to live and replay projection. Omitted and null capability fields map to `false`.

The existing agent-level `ClientState` remains useful for request handling, but projection must not consult mutable process-global capability state. Explicit connection-local state avoids one client's Preview support affecting another client. Stable mode, config, metadata, and usage updates bypass these gates.

### 3. Use canonical state snapshots for mode and config updates

Extend internal configuration-change events, or add a single snapshot event, so the ACP projector receives the complete post-commit option set needed for `config_option_update`. A mode transition projects two ordered updates: legacy `current_mode_update` first, then the complete `config_option_update`. Model, profile, and reasoning-effort transitions project only the complete config update.

Building options at the mutation boundary using the existing canonical option builder is preferred to asynchronously querying session state in the translator. The translator is synchronous, and delayed reads could combine values from different revisions. Emitting only low-level change events and reconstructing all state in the projector was rejected for the same consistency reason.

### 4. Emit metadata only after persistence succeeds

Introduce a persisted session-metadata event carrying patch semantics for title and `updated_at`. Emit it only after the history store confirms the mutation. Translate its fields directly to `session_info_update`, including explicit null only for intentional clears.

This keeps live notifications and later `session/list` results consistent. Generating a title in the ACP layer is out of scope; existing title-generation or persistence paths remain authoritative.

### 5. Maintain per-session usage projection state

The live projector keeps the latest known effective context limit by session, updated from `ProviderChanged`. On `LlmRequestEnd`, it emits `usage_update` only when both the event's current context-token count and the remembered positive limit are available. Cumulative cost maps to ACP cost with currency `USD`; request-only cost is not reported as cumulative session cost.

Compaction completion events will carry the newly calculated post-compaction context-token count. The projector combines that count with the same effective limit and latest known cumulative cost to emit a second usage snapshot after compaction. Reordering compaction completion to occur after token recalculation is preferred to estimating the new occupancy from summary length.

Replay materializes the latest valid usage snapshot rather than replaying every historical meter movement. If replay infrastructure cannot cheaply fold all events before delivery, a dedicated projection pass will collect the last provider limit, usage, cost, and post-compaction count before emitting one update.

### 6. Give QueryMT's todo plan a deterministic identity

Treat the `todowrite` plan as one plan per session with a deterministic ID such as `querymt-todos`. For clients advertising plan operations, non-empty todo snapshots become item-based `plan_update` replacements and an empty snapshot removes the active plan exactly once with `plan_removed`. The live projector tracks whether it has announced the plan to suppress duplicate removals.

For clients without the capability, retain the current legacy `plan` conversion. An empty legacy plan remains an empty replacement because v1 legacy ACP has no removal primitive. Using tool-call IDs as plan IDs was rejected because every `todowrite` invocation has a new call ID and would create multiple plans instead of updating one logical plan.

### 7. Map hook advisories narrowly to notices

`HookNotice` is the initial notice source. Map `is_error` to `error`; otherwise use `info`, with the hook event name as a compact non-empty title and its message as description. Do not convert execution failures, blocked hooks, permission decisions, or protocol errors into notices.

When notice support is absent, preserve any existing fallback behavior at the event source; the ACP projector simply omits the unsupported variant. Notices are never replayed.

### 8. Persist compaction identity and terminal state in events

Generate an opaque compaction ID before emitting `CompactionStart`, and include it in every related internal event. Add an internal summary-chunk event and terminal event carrying `completed`, `failed`, or `cancelled`, optional safe summary/error, and post-compaction context tokens when known.

For the current non-streaming compactor, emit one text summary chunk after the final user-displayable summary is known and before the completed update; omit the summary snapshot from that live terminal update to avoid transmitting it twice. Future streaming compactors can emit multiple chunks through the same event. Persist the materialized final summary on completion so replay emits one completed update with the authoritative summary and no historical chunks.

Restructure the compaction operation so any error or cancellation after the start event emits exactly one matching terminal event before propagating control. If the session or connection disappears before delivery, no stronger guarantee is possible. Events before compaction starts, including a pre-hook block, do not create a compaction entity.

### 9. Separate live streams from materialized replay

Replay folds durable stateful events before creating notifications:

- Keep the latest current mode/config snapshot and session metadata patch state.
- Emit only the latest valid usage snapshot.
- Reconstruct the current todo plan and use its deterministic ID.
- Emit terminal compactions with persisted IDs and final summaries.
- Drop notice events, summary chunks, and obsolete in-progress compaction updates.

Capability gates are applied after materialization so unsupported clients neither receive Preview variants nor influence retained history. Replaying raw events in sequence was rejected because it duplicates chunk content, flashes stale progress, and needlessly animates historical meter/config changes.

### 10. Keep protocol feature use explicit and serialization-tested

Confirm the exact `agent-client-protocol` v1 API for each update and capability at the locked dependency revision. Prefer named Cargo features for the four Preview surfaces if available; otherwise retain the crate's umbrella `unstable` feature and document which types depend on it. Add wire-format tests against serialized JSON discriminators and field names, in addition to typed translator tests.

This protects against compiling against a similarly named v2 type or an RFD revision whose wire shape differs from the locked crate.

## Risks / Trade-offs

- [Preview ACP schemas may change] -> Isolate Preview construction and capability parsing behind small adapters and pin behavior with serialization tests.
- [Adding IDs or variants changes persisted event serialization] -> Use backward-compatible optional/defaulted fields or versioned variants, and retain replay handling for legacy compaction events without IDs.
- [Zero-to-many translation touches both transports] -> Share one projection API and add parity tests for stdio and WebSocket delivery ordering.
- [Configuration snapshots can be expensive to rebuild] -> Build only after successful effective transitions and suppress unchanged revisions.
- [Per-connection projector state can grow with abandoned sessions] -> Clear session entries on close and clear all state when the connection ends.
- [A completed compaction may have no safe summary] -> Always report lifecycle status but omit summary content.
- [Historical data may lack stable plan/compaction IDs] -> Use the deterministic todo-plan ID and derive a deterministic legacy compaction ID from persisted event identity while never reusing it for distinct compactions.

## Migration Plan

1. Enable or confirm the required ACP v1 schema features without changing emitted behavior.
2. Add backward-compatible internal event fields/variants and replay decoding for old records.
3. Introduce connection-scoped capability snapshots and the zero-to-many projector API across both transports.
4. Add stable mode/config, metadata, and usage projection.
5. Add capability-gated plan, notice, and compaction projection with legacy plan fallback.
6. Enable materialized replay for the new stateful entities and validate old stored sessions.
7. Roll back by disabling the new projection branches; additive event fields remain readable and legacy plan emission remains available.
