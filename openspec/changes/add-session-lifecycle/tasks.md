# Tasks

Implementation starts only after review of this change.

## 1. Persistence

- [ ] 1.1 Add the lifecycle table and additive migration; verify existing/new sessions default to open/unarchived and deleting a session removes its metadata.
- [ ] 1.2 Add atomic read/patch storage operations; test persistence across reopen, independent fields, repeated updates, inverse operations, invalid input, and unknown sessions using fresh databases.
- [ ] 1.3 Resolve the child in successful one-shot delegation completion handling; test that failures/cancellation/verification failures and ordinary turns do not trigger resolution, parent/archive states stay unchanged, and reloading or replaying completion does not override an explicit reopen.

## 2. Backend queries and API

- [ ] 2.1 Add shared SQL filters to lifecycle-aware listing/search paths; extend existing pagination/count tests with mixed states, unresolved-only queries with/without archived sessions, workspace and relationship scopes, and matches beyond the first unfiltered page.
- [ ] 2.2 Expose capability-gated get/set extensions, listing metadata, and optional filters; document request/response shapes and test serialization, unloaded-session updates, mandatory available metadata on filtered/unfiltered listings, known defaults, unavailable metadata, and unchanged legacy visibility.
- [ ] 2.3 Preserve existing Rust entry points and unsupported-store behavior; verify existing callers compile and unsupported lifecycle requests fail explicitly.

## 3. Remote ownership

- [ ] 3.1 Forward lifecycle operations and filters to the owning node; test owner persistence including completed remote delegates, filtering before remote pagination, unavailable/unsupported errors, and unchanged unfiltered fallback behavior.

## 4. Integration verification

- [ ] 4.1 Exercise update/read/list through two independent ACP clients; verify shared state, reversible actions, and that lifecycle changes do not alter execution, history, activity timestamps, or related sessions.
- [ ] 4.2 Run focused agent tests and `cargo check -p querymt-agent` for affected default/remote feature configurations, then formatting checks; run `openspec validate add-session-lifecycle --type change --strict --no-interactive`.

## Workflow follow-up

- Implement support in all clients separately against this contract; client-specific work is not part of these backend tasks.
