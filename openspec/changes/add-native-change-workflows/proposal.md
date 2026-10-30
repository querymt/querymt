# Proposal

## Why

Implementing an OpenSpec change is currently a manual loop. The user opens a fresh session for each chunk of `openspec-apply-change`, watches context size, steers the agent to stop starting new tasks at roughly 250k tokens, pastes the previous session's summary into the next session, and then runs review, fix and archive sessions by hand. The session history for `output-limit-recovery`, `harden-tool-argument-handling` and `add-portable-repository-knowledge` shows this pattern 4–8 times per change, with steers sent late (287k–320k tokens) and several changes being implemented at once.

QueryMT already ships most of what this loop needs:
- gix-based managed worktrees with baseline capture, published result refs, handoff and crash-safe integration
- steering and run state
- per-request context-token accounting
- an event journal
- run leases

It lacks one deterministic orchestrator. This change adds that orchestrator inside QueryMT. All decisions are made by a pure engine core that needs no I/O, so the core can later be hosted as a WASM engine without being redesigned.

This change is an alternative to `add-acp-flow-driver`, which builds the same workflow as an external ACP client. Only one of the two is expected to land.

## What Changes

- **Workflow definitions.** Add portable, versioned files that define a change workflow:
  - a repository workflow file (`openspec/flow.yaml`) containing the graph of steps, gates, resources and integration settings
  - per-machine role settings: which profile or model plays implementer and reviewer, context budgets and concurrency limits
  - pi-compatible `openspec/changes/<change>/dependencies.yaml` files, used unchanged, that define ordering between changes
- **Deterministic engine core.** Add a new crate with no dependency on `crates/agent`. It turns `(state, event)` into `(state, effects)`. It owns:
  - the dependency graph across changes
  - the per-change state machine (preflight → workspace → implement chunks → verify → review → triage → fix → archive → integrate)
  - hard loop caps, progress rules and scheduling limits
  - deterministic replay from a recorded event log
- **Workflow runs in QueryMT.** Add a workflow actor that executes the engine's effects with existing QueryMT services:
  - managed worktrees and handoff
  - sessions with profiles
  - context budgets enforced through steering, then cancellation
  - gate commands with resource counters
  - a human inbox
  - a durable journal per workflow run
- **Control surfaces.** Add the ACP extension methods `querymt/workflow/{start,status,plan,approve,answer,resume,abort}`, a `qmt flow` CLI command, and workflow events for the desktop app.
- **Read-only roles.** Review and critique roles cannot mutate files. Mutating tools are denied for them, and the checkout is checked for changes after every read-only step.
- **Parallel changes and a local merge queue:**
  - one managed worktree per change
  - an in-memory gix merge forecast after every integration
  - serialized integration of each change's published checkpoint into the base checkout, using the same baseline-relative, local-work-protecting integration path that delegation results already use, extended to workspaces owned by a workflow
- **Unattended questions.** Questions raised in unattended sessions go to the workflow inbox instead of being cancelled.

## Capabilities

### New Capabilities
- `change-workflow-definitions`: The repository workflow file, machine role settings and cross-change dependency files, including how they are validated and the dry-run plan.
- `change-workflow-engine`: The deterministic decision core: scheduling across changes, the per-change state machine, loop caps, progress rules and replay.
- `change-workflow-runs`: How QueryMT executes a workflow run: sessions and context budgets, gates, read-only roles, the human inbox, the durable journal, restart recovery and the control surfaces.
- `change-workflow-integration`: Per-change worktrees, checkpoints, merge forecasting, the serialized local merge queue and archive.

### Modified Capabilities
<!-- None in openspec/specs. The worktree capabilities this builds on are still in the unarchived change add-git-worktree-isolation; see dependencies.yaml. -->

## Impact

- New crate `crates/flow-core`. It depends only on serde and has no tokio, filesystem or clock access.
- New module `crates/agent/src/workflow/` behind a `workflow` feature. Mobile builds are unaffected.
- Changes to:
  - `crates/agent/src/worktree/integrate.rs`: integration of workspaces owned by a workflow
  - `crates/agent/src/worktree/repo_ops.rs`: in-memory merge forecast
  - permission checks in `crates/agent/src/agent/execution/tool_calls.rs`: read-only roles
- New ACP extension methods, listed in `querymt/capabilities`, plus workflow event kinds in `events.rs`. These events are exported through typeshare, so the desktop types must be regenerated.
- New `qmt flow` subcommand in `crates/cli`.
- The work depends on `add-git-worktree-isolation` (managed workspaces, publish, handoff, integration) landing first.
