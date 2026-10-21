# Design

## Context

See `proposal.md` (Why) and `specs/session-interactions/spec.md` for the behavior contract. Paths below are relative to `crates/agent/src`.

Current state:

| Path | Local | Remote |
|---|---|---|
| Events agent → client | `EventSink` → fanout → per-connection forwarder | `EventForwarder` → `EventRelayActor` → same local fanout (`agent/remote/event_relay.rs`) |
| Control client → agent | `SessionActorRef::Local` ask | `SessionActorRef::Remote` kameo ask (same message types) |
| Permission | `ClientBridgeSender::request_permission` (`agent/execution/tool_calls.rs:903`); auto-grant when no bridge (`:848`) | no bridge reaches the remote actor (`Prompt.bridge` is `#[serde(skip)]`, `agent/messages.rs:37`) |
| Elicitation | durable `ElicitationRequested` event + host pending map (`elicitation.rs`) | event relayed, but claim/resolve search only in-process maps (`all_pending_maps`, `elicitation.rs:367`), so remote entries are never found |
| Workspace query | bridge → VS Code (`tools/builtins/language_query.rs`) | no bridge |

The WebSocket transport passes its connection bridge on every prompt (`acp/shared.rs:2141`), and the remote ref rejects it (`agent/remote/actor_ref.rs:236`). That is the reported error.

Elicitation already has the right shape (pending entry owned by the session's host, durable request event, transport-replaceable deliveries, recovery authority). Only its lookup is location-bound. Permission still uses the old callback shape.

## Goals / Non-Goals

**Goals:**
- One interaction cycle with no location branches above `SessionActorRef`.
- Reuse the two already-transparent paths (event relay out, actor messages in); no new proxy actors, no cross-node callback channels.
- Keep the existing ACP wire methods (`session/request_permission`, `elicitation/create`) and the elicitation recovery extension unchanged for clients.

**Non-Goals:**
- Workspace queries for remote sessions (reported unavailable).
- Mixed-version mesh compatibility.
- Persisting pending interactions across agent-process restarts.
- Redesigning the recovery authority model; it is reused as-is.

## Decisions

### 1. The session's host owns pending interactions; callers reach them through `SessionActorRef`
Pending entries stay in the host's `AgentConfig` map, which the tool waits on as today. The session actor exposes them through new messages: `ListPendingInteractions`, `ClaimInteractionDelivery { interaction_id, connection_id, authority }` and `ResolveInteraction { interaction_id, connection_id, delivery_generation, response }`. Each is registered as a kameo remote message (`agent/remote/remote_actor_impl.rs`), with `SessionActorRef` wrappers following the `submit_input` pattern and control timeouts. The handlers call the existing `elicitation.rs` claim/validate/resolve functions against the actor's own map, so validation and single-winner semantics stay in one place.

Callers that today iterate `all_pending_maps` (recovery registry list/attach/claim, `resolve_elicitation_from_connection`, the WebSocket and stdio forwarders, `elicitation_result`) instead resolve the session ref and call these methods. For local sessions this is a local ask with no behavior change; for remote sessions it is the same message over the mesh.

*Alternatives rejected:* a cross-node proxy that forwards bridge messages (keeps the location split and adds a second callback channel); replicating pending maps to the attaching node (two sources of truth, which conflicts with single-winner resolution); sending the answer as an event (events aren't acknowledged and can't return validation errors).

### 2. One interaction model for permission and elicitation
Generalize the pending entry with a kind: `Elicitation { form }` or `Permission { tool_call, options }`. Interaction IDs are opaque and stable across deliveries (for permission, use the tool-call ID, matching ACP). `ensure_tool_permission` keeps its order (cache → bypass → hooks); then, instead of calling the bridge, it registers a pending `Permission` interaction, emits a durable `PermissionRequested` event carrying the full ACP `RequestPermissionRequest` payload, and waits for the response or run cancellation. The no-bridge auto-grant branch is removed. Run cancel and tool exit retire permission entries through the same cleanup as elicitation (`cancel_pending_elicitations_for_session` generalised).

*Alternative rejected:* a separate permission registry. It would duplicate claim, generation and recovery logic.

### 3. Transports translate interaction events into ACP requests
The WebSocket and stdio forwarders already special-case `ElicitationRequested`. They now handle both kinds through one path: claim the delivery via `SessionActorRef`, send `elicitation/create` or `session/request_permission` with a fresh JSON-RPC ID, then resolve via `SessionActorRef` with the delivery generation. Permission requests keep a bounded per-delivery timeout, but a timeout or transport error only drops that delivery (spec: transport failure is not a decision). Recovery on reconnect re-delivers pending permission interactions alongside elicitations.

`ClientBridgeSender` becomes the transport's own outbound queue, owned by the connection, plus the agent-wide `notify_ext` used by schedules and control notifications. `ClientBridgeMessage::RequestPermission`, `Elicit` and `WorkspaceQuery` stop being produced by session code.

### 4. Sessions no longer hold bridges
Remove `Prompt.bridge`, `QueuedPrompt.bridge`, `SessionActor.bridge`, `SetBridge`/`GetBridge`/`ClearBridge`, `prompt_with_bridge`/`prompt_agent_with_bridge` and the session-registry `set_bridge` call. `session/prompt` dispatch calls plain `prompt`. The remote-ref rejection disappears along with its parameter. `Flush` is only exercised by bridge unit tests (no production caller), so prompt-response ordering relies on the existing event forwarder in both cases; this is verified by a test rather than assumed.

### 5. Workspace query is a capability of the local session only
`ExecutionContext.workspace_query_bridge` is populated only for locally hosted sessions with a VS Code connection. A remote-hosted run never has it, so `language_query` returns its existing "not available" result, with the message extended to name remote sessions.

### 6. Upgrade-required instead of compatibility shims
The new remote message IDs are absent on old nodes, so kameo remote asks fail with an unknown-message error. Map that error to `AgentError::RemoteNodeUpgradeRequired { node }`, surfaced to clients as an explicit error. No fallback path exists. Whether kameo reports unknown messages in a distinguishable way is verified first (task 1.3); if it does not, a version field on the mesh handshake or node info is the fallback detection mechanism.

## Risks / Trade-offs

- [Unattended runs now block on permission] → Intended (spec: unanswered permission waits). Document it and emit the durable request so a client shows it on next attach.
- [Mesh round-trip latency on every claim/resolve] → Interactions are human-paced; control timeouts already bound mailbox and reply time.
- [Remote node drops while a question is pending] → The pending entry dies with that node's run, as with any remote run loss. Clients see the existing remote-disconnect events; no fabricated answer.
- [Duplicate delivery from relay plus backfill] → Forwarders already dedupe by `(session_id, interaction_id)`; the claim generation makes stale answers no-ops.
- [Permission payload in durable events] → It contains tool arguments, which are already persisted in `ToolCallStart`; answers stay out of history, as with elicitation.
- [Large change] → Phased tasks; each phase keeps local behavior green before the next.

## Migration Plan

1. Land phases 1–3 without a feature flag; release one agent version.
2. Upgrade every mesh node together; old nodes return the upgrade-required error rather than misbehaving.
3. Ship the desktop companion change (`support-remote-session-interactions`) after the agent release. Older desktops keep working against new agents for elicitation, and for permission while connected.
4. Rollback means reverting the agent release on all nodes together; the desktop changes tolerate the old agent.
