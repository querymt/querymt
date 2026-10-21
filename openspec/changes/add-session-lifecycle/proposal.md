# Proposal

## Why

Sessions have no explicit indication that their purpose is finished or that they should be put aside. Add reversible resolution and archiving so clients can query the relevant sessions without deleting history. Related issue: https://github.com/querymt/querymt/issues/934.

## What Changes

- Persist independent resolved and archived states for each session.
- Add explicit resolve/reopen and archive/restore operations through one metadata update API.
- Automatically resolve a one-shot delegated child when its attempt reaches a terminal success, failure (including verification failure), or cancellation, without changing its parent or archive state.
- Include lifecycle metadata in session listings whenever authoritative state is available.
- Add optional server-side resolution/archive filters to session listing and search, including unresolved-only queries.
- Preserve existing sessions, unfiltered listing defaults, and execution behavior.
- Provide one consistent contract for all clients, including remote-session ownership.

## Capabilities

### New Capabilities

- `session-lifecycle`: Persistent, reversible session organization and backward-compatible filtering.

### Modified Capabilities

None.

## Impact

Backend session storage, delegation completion handling, list queries, QueryMT ACP extensions, and existing remote-session APIs under `crates/agent`. No new dependency or changes to task execution or scheduling are required. Client support is required across all clients; UI implementation will be handled separately in the UI repository.

This is a specification for review, not authorization to implement it.
