# Proposal

## Why

Remote mesh sessions cannot be prompted from a client such as the desktop: every WebSocket `session/prompt` carries a per-connection client bridge, and remote session references reject any bridge with `client bridges are not supported for remote session prompts`. The root cause is that agent-to-user interactions (tool permission, form elicitation, editor workspace queries) travel over an in-process `ClientBridgeSender` or an in-process pending-map lookup, while every other session path (events out, control messages in) is already location-transparent. Local and remote sessions therefore run different interaction cycles, and remote sessions either fail or would silently bypass interactive approval.

## What Changes

- Make every user interaction a session-owned pending interaction: requested via a durable session event and resolved via a `SessionActorRef` message, so the identical cycle works whether the session actor is local or on a remote mesh node.
- Route elicitation discovery, delivery claims, recovery and answers through the owning session actor instead of the caller's in-process pending maps, so remote elicitations behave exactly like local ones, including reconnect recovery.
- Move tool permission onto the same pending-interaction path, delivered to clients as standard ACP `session/request_permission`.
- **BREAKING**: A permission-required tool call with no attached or answering client now waits for a user answer instead of being auto-granted. Unattended runs that hit an unanswered permission stay paused until a user answers or the run is cancelled.
- **BREAKING**: Remove client bridges from session prompts and session actors (`Prompt.bridge`, `SetBridge`/`GetBridge`/`ClearBridge`, `prompt_with_bridge`). ACP transports keep a per-connection outbound queue only. Remote prompts no longer fail with the bridge error.
- Editor workspace queries (`language_query`) report themselves unavailable for sessions running on a remote node.
- Remove the unused `permission_result` extension method and its unpopulated pending-permission map.
- **BREAKING**: The mesh session wire contract changes; all mesh nodes must run the upgraded agent. Mixed-version meshes are not supported and fail with an explicit upgrade-required error.

## Capabilities

### New Capabilities
- `session-interactions`: Location-transparent lifecycle for agent questions that need a user answer (tool permission, form elicitation), covering request, delivery, single-winner resolution, waiting without an attached client, recovery after reconnect for local and remote-hosted sessions, and remote-session prompting without a client bridge.

### Modified Capabilities

None.

## Impact

- **Agent (`crates/agent`)**: `agent/session_actor.rs`, `agent/messages.rs`, `agent/remote/actor_ref.rs`, `agent/remote/remote_actor_impl.rs`, `agent/execution/tool_calls.rs`, `elicitation.rs`, `control/elicitation_recovery.rs`, `acp/websocket.rs`, `acp/stdio.rs`, `acp/shared.rs`, `acp/client_bridge.rs`, `agent/session_registry.rs`, `agent/handle/*`, `tools/builtins/language_query.rs`, `events.rs`, and their tests (notably `session_actor_ref_remote_tests.rs` E.11).
- **Clients**: the QueryMT desktop needs a companion change so its inbox keeps permission requests across reconnects and surfaces the upgrade-required error. It is tracked in the desktop repository as `support-remote-session-interactions` and depends on this change being released.
- **Operations**: coordinated upgrade of every mesh node; scheduled or headless runs that relied on implicit auto-grant must configure tool policy or hooks explicitly.
