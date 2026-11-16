# Proposal

## Why

Delegation only works when a runtime was built as a "quorum". The component that runs a delegation (`DelegationOrchestrator`) is created only inside `AgentQuorum`, and the quorum refuses any agent that was not built on its own event stream. Two bugs follow, both found while testing `support-mcp-skills` task 7.3, which they block:

- A standalone `[agent]` that delegates waits forever. The request is recorded and nothing runs it. Observed with a `.agents` delegation target; `[[remote_agents]]` use the same path.
- A `[quorum]` configuration with a `.agents` delegation target does not start: `agent did not use the quorum event fanout`.

The same split is why the set of agents is fixed at startup, why a user must choose "single agent" or "quorum" in advance, and why delegation wiring exists in three slightly different copies.

This change is planned on top of `split-agent-crate`. Since that change, the delegation machinery is in `querymt-agent-runtime`, but the code that builds a member agent stayed in the top package `querymt-agent`. Runtime cannot call into the top package, which is why `ProtocolTargetFactory` exists. A roster that builds members on first use, and an ACP handler in runtime that adds members, both need that build path in runtime.

## What Changes

Terms used below: a **runtime** is one **root agent** plus the **members** it can delegate to. The **roster** is the list of members.

- **One runtime shape.** Every runtime has a root agent and a roster, whatever configuration created it. A plain agent is a runtime with an empty roster. Every runtime whose root may call `delegate` runs a delegation service.
- **One event stream per runtime.** The root and every local member are built on the runtime's event stream by one build function, so a member cannot be built on a different one. The startup check that rejected `.agents` targets is removed with its cause.
- **Interactions from every member reach the user.** An approval, permission request or question raised in a delegated session is delivered to the client that owns the parent session, for every kind of member. This replaces the rule in the in-flight `support-mcp-skills` change that `.agents` targets never ask and fail instead.
- **The roster can change while the runtime is running.** Members can be added and removed between turns, from Rust and over ACP (`querymt/agents/list`, `querymt/agents/add`, `querymt/agents/remove`, notification `querymt/agents/changed`). Changes are not persisted: a restart restores the configured roster.
- **The agent list moves into the `delegate` tool.** The tool's definition lists the current members, so every turn sees the current roster. Today the list is a message added to a session's first turn only.
- **Remote agents are ordinary members.** A remote member is registered without contacting its peer, and its delegated sessions report activity and interactions like local ones.
- **Configuration.** `[[delegates]]` and a new `[agent.delegation]` table are accepted under `[agent]`. `[quorum]` and `[planner]` still load, are marked deprecated, and log a notice naming the replacement.
- **Package placement.** The roster, the member build path, the root builder function and the delegation service live in `querymt-agent-runtime`. The member build code moves there from `querymt-agent` first, with no behaviour change. `querymt-agent` keeps the `Agent`/`AgentBuilder` user API, the profile manager, the runner and the WebSocket server, and calls runtime to build the root.
- **Cleanup.** The quorum type, the three agent-registry implementations, the factory trait across the package boundary, the type-erased agent handle, the second builder, the delegation middleware, the tool-context type map that only carried the registry, and the duplicated event-source wiring are deleted, not wrapped.

Behaviour changes to existing setups:

- An agent with an unrestricted `tools` list and `.agents` delegation targets now gets those targets as members. Today it is offered a `delegate` tool that knows no agents.
- Member agents are no longer offered the `delegate` tool. It could never succeed for them.
- A configured remote agent no longer delays startup by up to 5 s per peer.

Public interface changes:

- **BREAKING** (Rust): `querymt_agent::quorum::{AgentQuorum, AgentQuorumBuilder, AgentQuorumError, DelegateAgent}`, `Agent::quorum()`, `Agent::planner()`, `Agent::multi()` and `api::QuorumBuilder` are removed. Use `Agent::single()` with `.delegate(..)`, `Agent::roster()`, `Agent::add_member()` and `Agent::remove_member()`.
- **BREAKING** (Rust): `delegation::{AgentRegistry, DefaultAgentRegistry}`, `dotagents::{DotagentsTargetRegistry, ProtocolTargetFactory}` and `api::DotagentsTargetFactory` are replaced by `delegation::AgentRoster`. `AgentInfo.id` becomes the `AgentId` type; its JSON form is unchanged.
- **BREAKING** (Rust): the `agent::AgentHandle` trait and `agent::remote::RemoteAgentHandle` are removed. Members are `delegation::MemberAgent::{Local, Remote}`. `Agent::delegate()` returns `Arc<LocalAgentHandle>`. `Agent::builder_from_quorum_config` returns `AgentBuilder`. `profile_host::ProfileRuntimeView` loses its `delegates` field; `delegate()` and `session_handle()` read the root's roster and return `Arc<LocalAgentHandle>`.
- **BREAKING** (Rust): `middleware::{DelegationMiddleware, DelegationContextMiddleware, DelegationConfig}`, `DelegationContextConfig`, `DelegationContextTiming`, `skills::ApprovalMode` and `tools::ToolContext::extensions()` are removed. `extensions()` was added by `split-agent-crate` only to carry the agent registry into the `delegate` tool.
- **BREAKING** (Rust, facade): `split-agent-crate` kept every `querymt_agent::` path. This change removes, on purpose, the facade and prelude re-exports of the removed types: `querymt_agent::{AgentHandle, AgentQuorum, AgentQuorumBuilder, AgentQuorumError, DelegateAgent, DelegationContextConfig, DelegationContextTiming}` and `querymt_agent::prelude::{AgentHandle, AgentQuorum, DelegateAgent, AgentRegistry, DelegationContextTiming, DelegationConfig}`. All other `querymt_agent::` paths stay.
- TOML: new `[[delegates]]` and `[agent.delegation]` under `[agent]` (additive). `[quorum]`/`[planner]` deprecated, behaviour unchanged.
- ACP: three new extension methods and one notification (additive). No existing method changes shape.
- Middleware: the built-in delegation middleware no longer exists; `DelegationGuardMiddleware` is unchanged.
- Rust naming: `Agent::builder()` is the entry point and `AgentBuilder::subagent(..)`, `Agent::add_subagent(..)`, `remove_subagent(..)`, `subagents()`, `has_subagents()`, `on_subagents_changed(..)` and `subscribe_subagent_changes()` are the simple API. They are aliases over the roster types (`delegate`, `add_member`, `remove_member` stay). `Agent::single()` and `is_multi()` are deprecated, as are `Agent::from_quorum_config*` and `builder_from_quorum_config`.
- Hooks: two new observe-only events, `subagent_added` and `subagent_removed`, with two new generated schemas (`subagent-change.command.{input,output}`). They are not tied to a session.
- ACP: `querymt/agents/changed` and the replies to `agents/add|remove` gain `added` / `removed` (the member ID), additively.
- Python: `Agent.create(...)` (`Agent.single(...)` deprecated), `add_subagent`, `remove_subagent`, `subagents`, `roster_generation`, `has_subagents()` (`is_multi()` deprecated), `on_delegation`, `on_tool_call`, `on_subagents_changed`, `SubagentError` with a stable `.code`.
- No change to the session database schema, the Extism plugin contract, hooks, typeshare types or log targets.

## Non-goals

- Persisting roster changes across restarts.
- Named groups of agents, or a per-session subset of the roster. The roster is designed so a per-session filter can be added later without changing it.
- Reloading `.agents` directories while running.
- Members that delegate onward (nested delegation).
- `peer` (remote provider routing) for local members added at runtime. It stays available for configured delegates.
- Removing `[quorum]`. It is only deprecated here.
- Changing delegation behaviour itself: wait policies, verification, isolation and planning summaries stay as they are.
- Changing the package layout from `split-agent-crate`, or splitting `querymt-agent-runtime` further.
- Dashboard UI. See Impact.

## Capabilities

### New Capabilities
- `agent-roster`: which agents a runtime's root can delegate to, how clients list them, how the root agent is told about them, and how the list is changed while running.
- `agent-delegation`: when delegation is enabled, that a delegation request always runs or fails, and that a delegated session's activity and questions reach the user, for local and remote members.

### Modified Capabilities
- `dotagents-protocol`: "Supported sub-agents extend the selected runtime's delegation registry" now requires that delegating to a protocol target runs in every runtime, that a runtime with configured delegates and a protocol target starts, and that a target is built on first use.
- `agent-configuration`: adds delegates and delegation settings under `[agent]`, and the deprecation of `[quorum]`.

## Impact

Paths are relative to `crates/`.

- **Sequencing:** starts after `split-agent-crate` has merged into `main` (its group 10), on a branch from `main`.
- **Packages:**
  - `querymt-agent-runtime`: most of the change (roster, member build, root builder function, delegation service, ACP transports and extension methods, `delegate` tool, skills, config, remote members). Features `remote`, `oauth`, `plugin-loaders`, `api`.
  - `querymt-agent`: `AgentBuilder`, `Agent`, the profile manager, `runner.rs`, the WebSocket transport, the facade and prelude, examples and benches. Its features (`dashboard`, `remote`, …) forward to runtime as today.
  - `querymt-agent-services`: `ToolContext::extensions()` and `AgentToolContext::with_extension` are removed. Nothing else in the package uses `http`, so that dependency goes too.
  - `querymt-agent-py` (`is_multi`), `querymt-mobile-ffi` (config-to-builder call).
  - Unchanged: `querymt-agent-core`, `-index`, `-knowledge`, `-sqlite`.
- **Code moved into runtime first:**
  - `agent/src/api/dotagents_targets.rs` → `agent-runtime/src/dotagents/target_factory.rs`.
  - `AgentConfig` and `parse_tool_specs` (`agent/src/api/config.rs`), `infer_required_capabilities` (`agent/src/api/utils.rs`) and the delegate closure body in `agent/src/api/quorum.rs` → `agent-runtime/src/delegation/member.rs`. `AgentConfig` is renamed `MemberBuildSpec` there, so it no longer shares a name with runtime's `agent::AgentConfig`.
- **Code deleted:**
  - In `agent/src`: `quorum.rs`, `api/quorum.rs`.
  - In `agent-runtime/src`: `middleware/delegation.rs`, `agent/remote/remote_handle.rs`, `agent/remote/remote_agent_stub_tests.rs`, most of `dotagents/target_registry.rs`, the `AgentHandle` trait in `agent/handle/mod.rs`, and `collect_event_sources` in `acp/shared.rs`.
- **Code changed:**
  - In `agent-runtime/src`: `delegation/` (new `roster.rs`, `member.rs`), `agent/agent_config_builder.rs`, `agent/execution_context.rs`, `agent/builtin_tools/`, `agent/handle/`, `agent/remote/remote_setup.rs`, `acp/`, `control/capabilities.rs`, `profile_host.rs`, `skills/`, `elicitation.rs`, `config/` (new `delegates.rs`).
  - In `agent/src`: `api/agent.rs`, `api/config.rs`, `api/mod.rs`, `acp/websocket.rs`, `profiles.rs`, `runner.rs`, `lib.rs`, `prelude.rs`.
  - In `agent-services/src`: `tools/context.rs`, `tools/context_impl.rs`.
- **Docs and examples:**
  - `docs/docs/agent/{configuration,delegation,mesh,middleware,api_reference,examples,profiles,agent_modes,worktrees}.md`.
  - The four `[quorum]` profiles in `agent/examples/confs` are rewritten as `[agent]`.
  - `agent/examples/{closure_builder,auto_delegation_example}.rs` and `agent/benches/agent_startup.rs` are ported.
- **In-flight changes:** `support-mcp-skills` (its requirement "Protocol targets do not wait for skill approval", design §7 and task 7.3) and `refine-mcp-skill-approval` (design wording) are updated by tasks in this change.
- **Build cost:** after `split-agent-crate`, a one-line runtime edit costs about 70 s per test build (its task 9.3). Since most of this change is in runtime, each task runs its tests in the narrowest package first.
- **`querymt-desktop` follow-up, not planned here:** controls to add and remove agents, showing a member's origin and location, and reacting to `querymt/agents/changed`.
