# Tasks

Implementation starts only after review of this change.

## 1. Persistence

- [x] 1.1 Add the lifecycle table and additive migration; verify existing/new sessions default to open/unarchived and deleting a session removes its metadata.
- [x] 1.2 Add atomic read/patch storage operations; test persistence across reopen, independent fields, repeated updates, inverse operations, invalid input, and unknown sessions using fresh databases.
- [x] 1.3 Resolve a created one-shot delegation child after the first successful transition to Complete, Failed (including verification failure), or Cancelled; preserve delegation results/errors on lifecycle-write failure, leave pre-creation failures and transient interruptions untouched, and test parent/archive independence, explicit reopen replay safety, and fresh retry children.

## 2. Backend queries and API

- [x] 2.1 Add shared SQL filters to lifecycle-aware listing/search paths; extend existing pagination/count tests with mixed states, unresolved-only queries with/without archived sessions, workspace and relationship scopes, and matches beyond the first unfiltered page.
- [x] 2.2 Expose capability-gated get/set extensions, listing metadata, and optional filters; document request/response shapes and test serialization, unloaded-session updates, mandatory available metadata on filtered/unfiltered listings, known defaults, unavailable metadata, and unchanged legacy visibility.
- [x] 2.3 Preserve existing Rust entry points and unsupported-store behavior; verify existing callers compile and unsupported lifecycle requests fail explicitly.

## 3. Remote ownership

- [x] 3.1 Forward lifecycle operations and filters to the owning node; test owner persistence including completed remote delegates, filtering before remote pagination, unavailable/unsupported errors, and unchanged unfiltered fallback behavior.

## 4. Integration verification

- [x] 4.1 Exercise update/read/list through two independent ACP clients; verify shared state, reversible actions, and that lifecycle changes do not alter execution, history, activity timestamps, or related sessions.
- [x] 4.2 Run focused agent tests and `cargo check -p querymt-agent` for affected default/remote feature configurations, then formatting checks; run `openspec validate add-session-lifecycle --type change --strict --no-interactive`.

## Verification evidence

- Reproduce the terminal/reopen regressions with `cargo test -p querymt-agent --lib delegation_loop_tests` and `cargo test -p querymt-agent --lib delegation::core::tests`, both with default features and with `--features remote`. The actual one-second wait-timeout test exercises cancellation cleanup and replay; the owner one-shot test verifies exact-child ownership and repeated finalization after explicit reopen. Storage coverage includes upgrading a pre-lifecycle database without backfill.
- Final cancellation-fixed runtime gate: separate fresh Iroh owner/planner processes, actual OpenAI WASM provider against a held local HTTP fixture, five-second wait timeout and one-second cancellation grace. Authoritative owner RPC state changes from open to resolved with a timestamp after terminal cancellation, preserving archive and parent lifecycle; no planner-local child exists. Explicit reopen survives repeated load/list; a distinct fresh child starts open and resolves on successful completion. Two-client local lifecycle/filter and observer-to-owner routing checks also pass. This supersedes the earlier cancelled-but-unresolved blocker.
- Broad verification commands: `cargo test -p querymt-agent --lib`; `cargo test -p querymt-agent --lib --tests --features dashboard,oauth,remote`; default/remote `cargo check -p querymt-agent`; CI-feature all-target Clippy with `-D warnings`; `cargo fmt --all -- --check`; strict OpenSpec 1.14.3 validation. Backend dashboard checks use a minimal `QMT_UI_DIST` fixture, and test HOME/XDG/TMPDIR are isolated outside the workspace.
- Local-only detailed evidence and historical failures: `stuff/session-lifecycle-cancel-final-20261018T045003Z/README.md`, `stuff/session-lifecycle-cancel-final-20261018T044643Z/`, and `stuff/session-lifecycle-final-20261018T025401Z/README.md`. Earlier-candidate multipage/restart/offline and provider-failure evidence is not claimed as rerun on the cancellation-fixed binary. No cross-host, UI, durable crash reconciliation, or ACP cancellation-replay coverage is claimed.

## Workflow follow-up

- Implement support in all clients separately against this contract; client-specific work is not part of these backend tasks.
