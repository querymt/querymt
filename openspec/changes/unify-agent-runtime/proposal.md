# Proposal

## Why

Delegation only works when a runtime was built as a "quorum". The component that runs a delegation (`DelegationOrchestrator`) is created only inside `AgentQuorum`, and the quorum refuses any agent that was not built on its own event stream. Two bugs follow, both found while testing `support-mcp-skills` task 7.3, which they block:

- A standalone `[agent]` that delegates waits forever. The request is recorded and nothing runs it. Observed with a `.agents` delegation target; `[[remote_agents]]` use the same path.
- A `[quorum]` configuration with a `.agents` delegation target does not start: `agent did not use the quorum event fanout`.

The same split is why the set of agents is fixed at startup, why a user must choose "single agent" or "quorum" in advance, and why delegation wiring exists in three slightly different copies.

## What Changes

Terms used below: a **runtime** is one **root agent** plus the **members** it can delegate to. The **roster** is the list of members.

- **One runtime shape.** Every runtime has a root agent and a roster, whatever configuration created it. A plain agent is a runtime with an empty roster. Every runtime whose root may call `delegate` runs a delegation service.
- **One event stream per runtime.** The root and every local member are built on the runtime's event stream by one build function, so a member cannot be built on a different one. The startup check that rejected `.agents` targets is removed with its cause.
- **Interactions from every member reach the user.** An approval, permission request or question raised in a delegated session is delivered to the client that owns the parent session, for every kind of member. This replaces the rule in the in-flight `support-mcp-skills` change that `.agents` targets never ask and fail instead.
- **The roster can change while the runtime is running.** Members can be added and removed between turns, from Rust and over ACP (`querymt/agents/list`, `querymt/agents/add`, `querymt/agents/remove`, notification `querymt/agents/changed`). Changes are not persisted: a restart restores the configured roster.
- **The agent list moves into the `delegate` tool.** The tool's definition lists the current members, so every turn sees the current roster. Today the list is a message added to a session's first turn only.
- **Remote agents are ordinary members.** A remote member is registered without contacting its peer, and its delegated sessions report activity and interactions like local ones.
- **Configuration.** `[[delegates]]` and a new `[agent.delegation]` table are accepted under `[agent]`. `[quorum]` and `[planner]` still load, are marked deprecated, and log a notice naming the replacement.
- **Cleanup.** The quorum type, the three agent-registry implementations, the type-erased agent handle, the second builder, the delegation middleware and the duplicated event-source wiring are deleted, not wrapped.

Behaviour changes to existing setups:

- An agent with an unrestricted `tools` list and `.agents` delegation targets now gets those targets as members. Today it is offered a `delegate` tool that knows no agents.
- Member agents are no longer offered the `delegate` tool. It could never succeed for them.
- A configured remote agent no longer delays startup by up to 5 s per peer.

Public interface changes:

- **BREAKING** (Rust): `querymt_agent::quorum::{AgentQuorum, AgentQuorumBuilder, AgentQuorumError, DelegateAgent}`, `Agent::quorum()`, `Agent::planner()`, `Agent::multi()` and `api::QuorumBuilder` are removed. Use `Agent::single()` with `.delegate(..)`, `Agent::roster()`, `Agent::add_member()` and `Agent::remove_member()`.
- **BREAKING** (Rust): `delegation::{AgentRegistry, DefaultAgentRegistry}` and `dotagents::{DotagentsTargetRegistry, ProtocolTargetFactory}` are replaced by `delegation::AgentRoster`. `AgentInfo.id` becomes the `AgentId` type; its JSON form is unchanged.
- **BREAKING** (Rust): the `agent::AgentHandle` trait and `agent::remote::RemoteAgentHandle` are removed. Members are `delegation::MemberAgent::{Local, Remote}`. `Agent::delegate()` returns `Arc<LocalAgentHandle>`. `Agent::builder_from_quorum_config` returns `AgentBuilder`.
- **BREAKING** (Rust): `middleware::{DelegationMiddleware, DelegationContextMiddleware, DelegationConfig}`, `DelegationContextConfig`, `DelegationContextTiming`, `skills::ApprovalMode` and `ToolContext::agent_registry` are removed.
- TOML: new `[[delegates]]` and `[agent.delegation]` under `[agent]` (additive). `[quorum]`/`[planner]` deprecated, behaviour unchanged.
- ACP: three new extension methods and one notification (additive). No existing method changes shape.
- Middleware: the built-in delegation middleware no longer exists; `DelegationGuardMiddleware` is unchanged.
- Python: `is_multi()` reports whether the roster currently has members.
- No change to the session database schema, the Extism plugin contract, hooks or typeshare types.

## Non-goals

- Persisting roster changes across restarts.
- Named groups of agents, or a per-session subset of the roster. The roster is designed so a per-session filter can be added later without changing it.
- Reloading `.agents` directories while running.
- Members that delegate onward (nested delegation).
- `peer` (remote provider routing) for local members added at runtime. It stays available for configured delegates.
- Removing `[quorum]`. It is only deprecated here.
- Changing delegation behaviour itself: wait policies, verification, isolation and planning summaries stay as they are.
- Dashboard UI. See Impact.

## Capabilities

### New Capabilities
- `agent-roster`: which agents a runtime's root can delegate to, how clients list them, how the root agent is told about them, and how the list is changed while running.
- `agent-delegation`: when delegation is enabled, that a delegation request always runs or fails, and that a delegated session's activity and questions reach the user, for local and remote members.

### Modified Capabilities
- `dotagents-protocol`: "Supported sub-agents extend the selected runtime's delegation registry" now requires that delegating to a protocol target runs in every runtime, that a runtime with configured delegates and a protocol target starts, and that a target is built on first use.
- `agent-configuration`: adds delegates and delegation settings under `[agent]`, and the deprecation of `[quorum]`.

## Impact

- Packages: `querymt-agent` (default features and `remote`; ACP code is also built under `api` and `dashboard`), `querymt-agent-py` (`is_multi`), `querymt-mobile-ffi` (config-to-builder call).
- Code deleted: `crates/agent/src/quorum.rs`, `middleware/delegation.rs`, `agent/remote/remote_handle.rs`, most of `dotagents/target_registry.rs`, the `AgentHandle` trait in `agent/handle/`, `QuorumBuilder` in `api/quorum.rs`.
- Code changed: `delegation/` (new `roster.rs`), `api/agent.rs`, `api/dotagents_targets.rs`, `agent/remote/remote_setup.rs`, `acp/`, `tools/builtins/delegate.rs`, `skills/`, `elicitation.rs`, `config/`, `profiles.rs`, `runner.rs`.
- Docs and examples: `docs/docs/agent/{configuration,delegation,mesh,middleware,api_reference,examples,profiles,agent_modes,worktrees}.md`; the four `[quorum]` profiles in `crates/agent/examples/confs` are rewritten as `[agent]`.
- In-flight changes on this branch: `support-mcp-skills` (its requirement "Protocol targets do not wait for skill approval", design §7 and task 7.3) and `refine-mcp-skill-approval` (design wording) are updated by tasks in this change.
- `querymt-desktop` follow-up, not planned here: controls to add and remove agents, showing a member's origin and location, and reacting to `querymt/agents/changed`.
