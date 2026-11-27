# Proposal

## Why

An orchestrating agent can send a coder to an isolated worktree and later accept its result, but it cannot send a reviewer into that same worktree. The `delegate` tool always allocates a fresh worktree (or shares the orchestrator's checkout), so a reviewer sees neither the coder's work nor a place to fix it. `/worktree handoff` already moves ownership of a worktree from one session to another, but only users can run it, and it always creates a blank successor session instead of a delegate.

## What Changes

- **Delegation by handoff.** The `delegate` tool gains an optional `handoff_from` argument naming an earlier delegation of the same session. The new delegate runs in that delegation's worktree: the previous owner is stopped, ownership moves to the new delegate session with the same compare-and-swap transfer `/worktree handoff` uses, and the previous owner becomes read-only (`owner_transferred`). No new worktree is allocated and no files, branch, index or baseline change. Refusals (not the delegating session, no isolated worktree for that delegation, owner still running, workspace not ready, combined with `isolation: "shared"`) fail the delegation and leave ownership unchanged.
- **One handoff core, two kinds of successor.** The ordering of `WorktreeOps::handoff_announced` (quiesce owner, exclusive activity lease, pending successor behind a startup barrier, transfer, release, announce) is reused for a delegate successor. The successor of a delegation handoff is a delegate session: parent is the delegating session, fork origin `delegation`, hidden from listing like every delegate. The previous owner's stream still records `WorktreeOwnershipTransferred`.
- **Results keep one name.** The worktree stays recorded under the delegation that created it. A delegate that ran by handoff publishes a new result on that worktree when it completes, and its completion text tells the orchestrator to accept it with the worktree's original delegation id. `accept_delegation_result` is unchanged.
- **Event and hook carry the request.** `DelegationRequested` gains an optional `handoff_from` field (additive; older events deserialize without it). The `pre_delegation` hook input gains the same read-only field.
- **Delegate owner lookup outside profiles.** Stopping the previous owner must find delegate sessions in config-file runs, not only through profile bindings.
- **New example profile `coder-team`** (`crates/agent/examples/confs/coder_team.toml`, embedded next to `coder-delegate`): an orchestrator in plan mode that explores, delegates, and merges; isolated `coder` delegates; a `reviewer` that takes over a coder's worktree with `handoff_from`, reviews and fixes; a shared read-only `explorer`. `.agents` support (`[dotagents]`) and managed worktrees (`[agent.workspace]`) are enabled, and every merge asks the user (`always_ask = ["accept_delegation_result"]`).
- **BREAKING (tool and wire shape, additive):** the `delegate` tool schema, the `DelegationRequested` event (`#[typeshare]`) and the `pre_delegation` hook input each gain an optional field. Existing callers are unaffected; models see a new argument.

## Non-goals

- No new storage: no SQLite column or migration. The handoff reuses `transfer_owner` and the existing handoff barrier.
- No handoff from or to remote-hosted sessions (refused as `owner_not_local`, as for `/worktree handoff`).
- No handoff summary for delegates: the orchestrator passes the context in `context`, as for any delegation.
- No change to `accept_delegation_result` semantics, result capture, or `/worktree handoff` itself.
- No automatic review loop or merge policy in the runtime; the `coder-team` workflow lives in the profile's prompts.
- No querymt-gpui UI for the new field.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `isolated-delegation`: a delegation may run in an earlier delegation's worktree by ownership handoff; its result is accepted under the worktree's original delegation.
- `git-worktree-workspaces`: a new requirement lets a delegate session be the successor of an ownership handoff (same checks, transfer and announcement as "Ownership handoff", which is unchanged), and "Worktree session lineage" gives that successor delegation lineage instead of `worktree` lineage.

## Impact

- `querymt-agent-protocol-types`: `AgentEventKind::DelegationRequested.handoff_from` (`#[typeshare]`, additive).
- `querymt-agent-services`: `worktree/ops.rs` (handoff core shared with delegation), `HandoffHost`/`OwnerHost` successor creation.
- `querymt-agent-runtime` (features `remote`, `api` paths unchanged in shape): `agent/builtin_tools/delegate.rs`, `agent/execution/tool_calls.rs`, `hooks/engine.rs` (`PreDelegationRequest`), `delegation/core.rs`, `agent/handle/agent_handle_impl.rs`, `agent/session_materializer.rs`, `agent/handle/session_control.rs`, `agent/handle/ext_worktree_ops.rs`, `delegation/roster.rs`.
- `querymt-agent`: `examples/confs/coder_team.toml`, `profiles.rs` (embedded profile), tests.
- `querymt-gpui`: must still build and pass `cargo t-gpui` (protocol type change); no UI work.
- querymt-desktop follow-up (report only): optionally show "continues delegation X" for a `delegation_requested` event with `handoff_from`.
- Docs: `docs/docs/agent/delegation.md`, `worktrees.md`, `examples.md`, `hooks` reference for `pre_delegation`; `CHANGELOG.md` Unreleased.
- No SQLite migration, no config key, no Extism contract, no Python binding change.
