# Tasks

## 1. Session-owned interaction access

- [x] 1.1 Add `ListPendingInteractions`, `ClaimInteractionDelivery` and `ResolveInteraction` session-actor messages whose handlers call the existing `elicitation.rs` snapshot/claim/validate/resolve functions on the actor's own pending map; verify unit tests in `agent/session_actor.rs` cover claim generation bump, stale generation, wrong connection, invalid form and single-winner resolve.
- [x] 1.2 Register the three messages as kameo remote messages in `agent/remote/remote_actor_impl.rs` and add `SessionActorRef` wrappers with control mailbox/reply timeouts (pattern: `submit_input`); verify a `session_actor_ref_remote_tests.rs` case resolves a pending elicitation on a remote ref.
- [x] 1.3 First verify how kameo reports an unregistered remote message/actor; map that failure (or a mesh version check if it is indistinguishable) to an explicit `RemoteNodeUpgradeRequired { node }` error surfaced to clients as an ACP error; verify with a test that simulates an unregistered message ID.
- [x] 1.4 Switch `ElicitationRecoveryRegistry` (issue/list/attach/claim), `resolve_elicitation_from_connection`, the `elicitation_result` extension and both WebSocket and stdio forwarders from `all_pending_maps` to session-ref calls, keeping profile/delegate routing; verify all existing elicitation and recovery tests pass unchanged.
- [x] 1.5 Add a two-node integration test: a remote-hosted tool elicitation is delivered over the local WebSocket, an invalid answer is re-delivered, a valid answer resumes the remote tool exactly once, and reconnect re-delivers a still-pending remote question.

## 2. Permission as a pending interaction

- [x] 2.1 Generalize the pending entry with an interaction kind (`Elicitation { form }` / `Permission { request }`) and generalize run-cancel/tool-exit cleanup to retire both kinds; verify cleanup unit tests for both kinds.
- [ ] 2.2 Extend the durable `PermissionRequested` event with the full ACP permission request payload (tool-call update, options, tool-call ID) and regenerate typeshare types; verify event serialization and durability classification tests.
- [ ] 2.3 Change `ensure_tool_permission` to register a pending permission interaction, emit the durable event and await the answer or cancellation, preserving cache, bypass and hook order and the "always" cache writes, and remove the no-bridge auto-grant; verify tests for no client attached (waits), policy/hook pre-decision (no prompt), cancel while waiting (not executed), and "always allow" (no later prompt).
- [ ] 2.4 Teach the WebSocket and stdio forwarders and the recovery attach path to deliver permission interactions as `session/request_permission` and resolve answers through `ResolveInteraction`; treat timeout, disconnect and malformed answers as undelivered; verify tests for disconnect-then-redeliver, malformed answer stays pending, and two connections answering (first wins, second stale).
- [ ] 2.5 Remove the unused `permission_result` extension method and its `PermissionMap` plumbing from `acp/shared.rs`, `acp/websocket.rs` and `acp/mod.rs`; verify `cargo build` and the ACP dispatch tests pass.
- [ ] 2.6 Add a two-node integration test where a remote-hosted permission-required tool call prompts the attached WebSocket client and the selected option decides the remote call.

## 3. Remove bridges from sessions

- [ ] 3.1 Remove `bridge` from `Prompt`, `QueuedPrompt` and `SessionActor`, delete `SetBridge`/`GetBridge`/`ClearBridge`, `prompt_with_bridge`/`prompt_agent_with_bridge` and the session-registry `set_bridge` call, and make `session/prompt` call plain `prompt`; verify `cargo build --all-features` and that no session code produces `RequestPermission`, `Elicit` or `WorkspaceQuery` bridge messages.
- [ ] 3.2 Replace test E.11 in `agent/remote/session_actor_ref_remote_tests.rs` with "remote prompt over WebSocket dispatch succeeds and streams updates", and add a test that update notifications for a prompt are sent before its prompt response on both local and remote sessions.
- [ ] 3.3 Reduce `ClientBridgeSender`/`ClientBridgeMessage` to transport-owned outbound messages plus agent-wide `notify_ext`, and update the module docs in `acp/client_bridge.rs` to describe the new role; verify bridge unit tests and schedule/control notification tests pass.

## 4. Workspace query availability

- [ ] 4.1 Populate the workspace-query capability only for locally hosted sessions with a supporting connection and extend the `language_query` unavailable message to name remote sessions; verify a unit test that a remote-hosted execution context returns the unavailable result and the run continues.

## 5. Documentation and verification

- [ ] 5.1 Document the interaction lifecycle, the wait-on-unanswered-permission behavior for unattended runs, and the all-nodes-upgrade requirement in the agent docs/changelog; verify the docs render and mention the `RemoteNodeUpgradeRequired` error.
- [ ] 5.2 Run the full agent suite (`cargo test --all-features`, clippy, fmt) and verify it is green with the two-node integration tests included.

## Workflow follow-up

- Release the agent before the desktop companion change (`support-remote-session-interactions`) is applied end to end.
- Archive this change after the release and the desktop two-node verification are recorded.
