# Proposal

## Why

Implementing an OpenSpec change is currently a manual loop. The user opens a fresh session for each `openspec-apply-change` chunk, watches context size, steers the agent to stop starting new tasks at roughly 250k tokens, pastes the previous summary into the next session, and then runs review, fix and archive sessions by hand. Session history for `output-limit-recovery`, `harden-tool-argument-handling` and `add-portable-repository-knowledge` shows this 4–8 times per change, often for several changes at once in separate worktrees.

This change automates that loop with an external, deterministic flow driver. The driver is a standalone program that talks to agents only as an ACP client. It uses QueryMT's ACP extensions (`querymt/worktree/*`, `querymt/session/steer`, `querymt/session/runtimeState`) when the agent offers them, and portable fallbacks when it does not. The workflow then works for any repository with OpenSpec and for any ACP agent, it runs independently of qmtcode releases, and a driver crash cannot affect the agent.

This change is an alternative to `add-native-change-workflows`, which builds the same workflow inside QueryMT. Only one of the two is expected to land.

## What Changes

- **Driver.** Add a standalone `flow` program (new crate `crates/flow-driver`, a separate binary). Its only dependencies on QueryMT are the ACP wire protocol and the documented QueryMT extension methods. It does not depend on `crates/agent`.
- **Workflow definitions.** Add portable, versioned files that define a change workflow:
  - the repository workflow file (`openspec/flow.yaml`): graph of steps, gates, resources and integration settings
  - per-machine agent and role settings: ACP endpoints (WebSocket URL or stdio command), role-to-profile mapping, context budgets and limits
  - pi-compatible `openspec/changes/<change>/dependencies.yaml`, used unchanged
- **Deterministic engine.** The driver's engine makes every transition from `openspec ... --json`, exit codes, schema-valid outputs, repository state and human answers. Every loop has a hard cap, and the run journal can be replayed exactly.
- **Agent capabilities by feature detection.** The driver calls `querymt/capabilities` and records the result:
  - When the QueryMT extensions are present, workspaces are QueryMT-managed worktrees (gix, `current_working_tree` baseline, handoff, publish), wrap-up is delivered with `querymt/session/steer`, and run IDs come from `querymt/session/runtimeState`.
  - Otherwise the driver manages plain git worktrees itself, and wrap-up becomes cancel plus a follow-up prompt.
- **Context budget.** Budgets are read from standard ACP `usage_update` notifications (`used`, `size`): steer at `wrap_up`, cancel at `stop`.
- **Permissions and questions.** As the ACP client, the driver answers permission requests: edits are allowed only inside the step's workspace, and read-only roles are denied any mutation. Questions and elicitations from the agent go to the driver's inbox.
- **Parallel changes and a local merge queue.** One workspace per change, in-memory merge forecasts, and serialized integration. Integration is performed by the driver as one commit containing only the change's baseline-to-checkpoint delta, applied on the base branch.
- **Control.** The CLI offers `flow run|plan|status|approve|answer|resume|abort`. Every session is a normal agent session, so the QueryMT desktop app shows the sessions live and the user can step in.

## Capabilities

### New Capabilities
- `acp-flow-definitions`: Repository workflow file, machine agent and role settings, dependency files, validation and the dry-run plan.
- `acp-flow-engine`: Deterministic scheduling across changes, the per-change state machine, loop caps, progress rules, the journal and replay.
- `acp-flow-agent-sessions`: How the driver drives ACP agents: capability detection, sessions per step, context budgets via `usage_update`, wrap-up through steer or fallback, permission and question handling, and reattach after a driver restart.
- `acp-flow-integration`: Per-change workspaces (QueryMT-managed or driver-managed), checkpoints, merge forecasts, archive and the serialized local merge queue.

### Modified Capabilities
<!-- None. The driver uses existing QueryMT ACP extensions as published; no QueryMT requirement changes. -->

## Impact

- New crate and binary `crates/flow-driver`. Dependencies: `agent-client-protocol`, `tokio-tungstenite`, `gix` (for driver-managed worktrees, forecasts and integration) and serde. No dependency on `crates/agent`.
- No QueryMT code changes are needed for the minimal version. Optional follow-ups in QueryMT are listed in design.md: a read-only mode enforced server-side, and workflow-owned integration over ACP.
- qmacp is not required. Its WebSocket client may be reused as a library only if it stops dropping `usage_update` fields.
- Running against qmtcode needs its ACP WebSocket endpoint (`qmtcode --acp-ws` or the dashboard server) and `[agent.workspace] enabled = true` with `baseline = "current_working_tree"`.
