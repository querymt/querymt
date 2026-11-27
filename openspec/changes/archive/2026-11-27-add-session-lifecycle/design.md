# Design

## Context

See `proposal.md` for motivation. Sessions already have SQL-backed listing/search and ACP extension APIs. Lifecycle organization needs no changes to the agent run loop.

## Goals / Non-Goals

**Goal:** a small metadata-and-query feature with backward-compatible defaults.

**Non-goals:** arbitrary labels, automatic transitions other than terminal one-shot delegation resolution, bulk actions, inherited/cascading state, an undo history, new notification infrastructure, or a pagination redesign.

## Decisions

### Storage: two nullable timestamps

Add a `session_lifecycle` table keyed by the existing session ID, with `resolved_at` and `archived_at`. A missing row or null timestamp means false; timestamps use the existing storage format. Deleting a session removes its lifecycle row through a foreign key. No backlog rewrite is required.

Derive the public resolved/archived flags from these timestamps rather than storing redundant booleans or an additional status enum. The separate table avoids changing conversation activity timestamps and the existing public `Session` struct. Dedicated columns on `sessions` are possible, but less isolated.

An atomic patch sets or clears only supplied fields. Repeating `true` retains the original timestamp. Concurrent updates are serialized by storage; the last successful write to each supplied field wins. Reversal uses the same API with the opposite value, not a separate undo subsystem.

### API: reuse QueryMT extensions

Advertise `session_lifecycle` support through the existing capabilities response. Add logical methods:

- `querymt/session/getLifecycle`: `{ session_id }`.
- `querymt/session/setLifecycle`: `{ session_id, resolved?, archived? }`; at least one boolean is required.

Return `{ session_id, resolved, archived, resolved_at, archived_at }`, with nullable RFC 3339 timestamps. Include the same state in `SessionInfo._meta.querymt.lifecycle` for every listed session with authoritative lifecycle state, including unfiltered listings. The field is optional only for unsupported backends or unavailable remote state, never a reason to omit known defaults. Accept optional `{ resolution, archive }` filters in `session/list` under `_meta.querymt.sessionFilters`. Use the existing ACP extension wire-prefix adapter.

For unresolved sessions, request `{ "resolution": "open" }`; add `"archive": "excluded"` to omit archived sessions. Metadata is returned regardless of whether either filter was supplied.

Reuse the same types and storage operations for backend callers and remote-owner requests. Preserve existing public Rust entry points and constructible structs; add lifecycle-aware query/result types where needed instead of requiring existing callers or custom stores to change. Advertise support only when the backend implements the contract. Reads and updates operate on stored sessions without starting a session actor.

### Queries and remote sessions

Share bound SQL lifecycle predicates across flat listing, grouped browsing, workspace pages, and search, including their counts and group activity. Filtered variants of child listings use the same predicates; unfiltered child/history access stays unchanged. Keep existing ordering and cursor formats. A caller starts fresh after changing filters or mutating list membership.

Forward remote reads, patches, and filters to the existing authoritative owner and include lifecycle metadata in its responses. Do not write lifecycle state into a local bookmark. An explicitly lifecycle-filtered request fails if a requested owner cannot supply it; unfiltered remote listing retains its existing fallback behavior. No offline synchronization or conflict-resolution subsystem is added.

### State is per session

Roots, user forks, and delegates retain independent state. There is no parent traversal or cascading write; existing relationship filtering still determines which sessions are listed.

The automatic transition is resolving a one-shot delegated child after its delegation's first successful expected-state transition to `Complete`, `Failed`, or `Cancelled`. Resolution means the attempt's purpose is finished, not that execution succeeded; an agent may retry by creating another delegation with its own child. Apply the lifecycle write only after a real child session has been created, using the target `AgentHandle` that owns that child rather than the planner's store or a local remote bookmark. Change only resolution; do not archive the child or resolve its parent. Failures before child creation have no child to resolve.

Do not attach this behavior to generic turn-end events, listing, history replay, or session loading. A transient interruption or a merely running state is not terminal and does not resolve the child. Failed execution, including verification failure, and terminal cancellation do resolve it. The first successful expected-state transition gates the lifecycle write, so replay after an explicit reopen cannot resolve the child again. If the lifecycle write fails or is unsupported, log the delegation and child context but preserve the existing delegation status, error/result exposure, and execution outcome; do not claim the lifecycle update persisted. Existing terminal delegates are not retroactively backfilled by this migration.

## Risks / Trade-offs

- Existing offset cursors can shift after concurrent mutations: callers restart listing after their own updates; stable snapshot pagination is not promised by this change.
- Other clients see changes on their next fetch: no new real-time catalog subscription is required.
- Reversing a field can replace another writer's latest value: this initial contract is last-write-wins, not revision-guarded undo.

## Migration Plan

Apply an additive migration on normal backend upgrade. Old sessions remain open/unarchived without backfill; old clients keep unfiltered behavior. Verify migration and behavior using fresh test databases only. Older code can ignore the additional table; downgrading need not remove it. Client support follows separately using the shared contract.
