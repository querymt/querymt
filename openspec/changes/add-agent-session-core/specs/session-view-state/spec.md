# Spec Delta

## Purpose

Turns the agent's session output (load snapshot, ACP session updates, older-history pages) and client observations into one deterministic conversation view with change reports, so every QueryMT client renders sessions the same way, as a native Rust library or as WebAssembly.

## ADDED Requirements

### Requirement: Session view from a load snapshot
Given a session load response that carries `querymt/sessionLoadSnapshot.v1`, the session view SHALL contain the conversation turns, tool calls with their arguments and results, compaction boundaries, usage and cost accounting, subagents, session cursors, pending inputs, the runtime status and the history paging state that the snapshot describes. A load response SHALL count as the end of history replay: updates applied after it continue from the state it describes.

#### Scenario: Full snapshot hydrates the conversation
- **WHEN** a load response with a complete snapshot of 3 prompts, their assistant replies and 4 tool calls is applied to an empty view
- **THEN** the view has 3 turns in history order, each with its prompt, its assistant text and its tool calls with final status
- **AND** the history paging state reports no older history

#### Scenario: Windowed snapshot takes whole-session state from the summary
- **WHEN** a windowed snapshot with `history.complete = false` and a `summary` is applied
- **THEN** token totals, the current model, pending inputs and child session links come from `summary`, and totals marked partial are reported as approximate
- **AND** the history paging state carries the `before` token and marks older history as available

#### Scenario: Replayed updates before a load response without a snapshot are kept
- **WHEN** replayed ACP session updates for 2 turns are applied, followed by a load response without `querymt/sessionLoadSnapshot.v1`
- **THEN** the view keeps the 2 replayed turns and takes config options and modes from the response

#### Scenario: Snapshot replaces replayed turns
- **WHEN** replayed ACP session updates are applied, followed by a load response whose snapshot describes the same session
- **THEN** the view's turns are the ones the snapshot describes, each appearing once

#### Scenario: Snapshot with an idle runtime keeps unfinished tool status
- **WHEN** a snapshot whose runtime status is idle contains a tool call with no result
- **THEN** the tool call keeps its last reported status and is not reported as the active tool call

### Requirement: Turns are agent runs
A turn SHALL be one agent run, as the agent defines it (`session-history-loading` / Turn boundaries), identified by the run's id. Content SHALL join the turn of the run id it carries, or, without one, the turn of the run the session's state reports as running. Steering prompts and compactions that happen during a run SHALL stay in that run's turn. When the agent reports no run ids at all (an older agent), a turn SHALL start at each user message and at each compaction outside a turn.

#### Scenario: Steering stays in the running turn
- **WHEN** a run's updates are followed by a steering user message carrying the same run id, then more agent output
- **THEN** the view holds one turn for the run, containing both user messages and all agent output in order

#### Scenario: Compaction during a run stays in the turn
- **WHEN** a `compaction_update` carrying a run id arrives while that run is the last turn
- **THEN** the compaction is an entry of that turn and no new turn starts

#### Scenario: Run without a prompt is its own turn
- **WHEN** the session reports a new run running and that run's agent output arrives with no user message
- **THEN** the view holds a new turn for that run containing the output

#### Scenario: Older agent without run ids
- **WHEN** updates without run ids are applied and no run state is reported
- **THEN** each user message starts a new turn

### Requirement: Live ACP session updates
ACP `session/update` notifications SHALL be applied in arrival order. Notifications for a different session id SHALL be ignored and reported as ignored.

#### Scenario: Streamed chunks merge into one assistant message
- **WHEN** five `agent_message_chunk` notifications with the same `messageId` arrive
- **THEN** the last turn holds one assistant message whose text is the concatenation of the five chunks

#### Scenario: Tool call update replaces the tool state
- **WHEN** a `tool_call` with status `in_progress` is followed by a `tool_call_update` with the same `toolCallId` and status `completed`
- **THEN** the view holds one tool call with that id, status `completed` and the updated output

#### Scenario: Update for another session is ignored
- **WHEN** a notification whose `sessionId` differs from the view's session arrives
- **THEN** the view is unchanged and the change report lists the notification as ignored

### Requirement: Standard ACP context compaction
Context compaction SHALL be read from the standard ACP `compaction_update` and `compaction_summary_chunk` session updates (client capability `session.compaction`, `CompactionCapabilities`), following the ACP contract: ID-addressed upsert, position fixed by the first update, patch semantics for `summary`, `error` and `_meta`, chunks appended in receive order. A compaction whose status is completed, failed or cancelled SHALL NOT return to in progress; later patches to its other fields still apply.

#### Scenario: First compaction update fixes the timeline position
- **WHEN** a `compaction_update` with a new `compactionId` and status `in_progress` arrives after turn 3, and a later update with the same id and status `completed` arrives after turn 5 has started
- **THEN** the view holds one compaction boundary after turn 3, with status `completed`

#### Scenario: Summary chunks append and create a compaction
- **WHEN** two `compaction_summary_chunk` updates for a first-seen `compactionId` arrive
- **THEN** the view holds one in-progress compaction whose summary is the two content blocks in receive order

#### Scenario: Compaction patch semantics
- **WHEN** a `compaction_update` omits `summary`, then another sets `error` to `null`, then another sets `summary` to `[]`
- **THEN** the first keeps the stored summary, the second clears the error, and the third clears the summary

#### Scenario: Unknown compaction status is not completed
- **WHEN** a `compaction_update` carries status `_vendor_paused`
- **THEN** the compaction is kept with an other status and is not shown as completed

#### Scenario: Settled compaction is not reopened
- **WHEN** a compaction with status `completed` receives a `compaction_update` with status `in_progress` and a new `summary`
- **THEN** the compaction keeps status `completed` and takes the new summary

### Requirement: Snapshot and live stream equivalence
Hydrating a snapshot SHALL produce the same turns as applying the session's live stream (ACP session updates and state updates as the agent sends them live, including streamed deltas and structured prompt blocks), and the same turns as applying the agent's history replay, ignoring arrival timestamps.

#### Scenario: Snapshot and live stream produce the same turns
- **WHEN** one generated session is applied once as a load snapshot and once as its live stream
- **THEN** both views have equal turns with equal ids, entries, tool calls, compactions, plan and usage

#### Scenario: Snapshot and replay produce the same turns
- **WHEN** the same session is applied once as a load snapshot and once as the agent's history replay before a load response without a snapshot
- **THEN** both views have equal turns, tool calls, compactions and plan

### Requirement: Load during a run
A view built from a load taken during a run, followed by the updates the agent sends after that load, SHALL equal the view built from the whole live stream.

#### Scenario: Load during a run continues seamlessly
- **WHEN** a session is loaded while an assistant message is streaming, and the updates the agent sends after the load are applied
- **THEN** the view equals the view of a client that received the whole live stream, and the assistant message's text appears once and in full

### Requirement: Chunking invariance
Splitting the same text into different chunk sizes SHALL produce the same view.

#### Scenario: Chunk size does not change the view
- **WHEN** the same assistant text is streamed once in 1-character chunks and once in 64-character chunks
- **THEN** both views are equal

### Requirement: Change reports
Every applied batch SHALL return a change report that lists the ids of added, updated and removed turns, whether the session header changed, and any ignored or rejected inputs. Turns not listed SHALL be unchanged. Turn ids SHALL stay stable for the life of the view. After a load rebuilds the view, a turn with the same id and equal content as before SHALL NOT be reported.

#### Scenario: Streaming chunk reports only the last turn
- **WHEN** an `agent_message_chunk` extends the last turn of a 500-turn view
- **THEN** the change report lists exactly that turn as updated and no other turn

#### Scenario: Unchanged turns keep their content and id
- **WHEN** a tool call update for the last turn is applied
- **THEN** every other turn has the same id and equal content before and after the update

### Requirement: Turn entries with stable identity
Each turn SHALL hold its content as ordered entries (user message, assistant message, reasoning part, tool call, compaction boundary). Each entry SHALL have an id that is stable for the life of the view and a revision that grows whenever that entry changes. A turn's revision SHALL grow whenever any of its entries changes.

#### Scenario: Streaming chunk changes one entry
- **WHEN** an `agent_message_chunk` extends the assistant message of a turn that also holds 3 tool calls
- **THEN** only the assistant message entry gets a new revision, the 3 tool call entries keep their ids and revisions, and the turn's revision grows

### Requirement: Identity from the agent
Entries SHALL be keyed by entity type together with the agent's id for that entity (message id, tool call id, compaction id), as ACP keys entities, and turns by run id. Turn and entry ids SHALL NOT come from client-made ids, so the same session has the same ids live and after a reload.

#### Scenario: Reload keeps turn and entry ids
- **WHEN** a session watched live, including a prompt sent optimistically by this client, is loaded again from a snapshot
- **THEN** every turn and entry has the same id as before, and the change report lists no turn whose content is unchanged

#### Scenario: Same id on different entity types
- **WHEN** an agent message and an agent thought arrive with the same `messageId`
- **THEN** the view holds two entries, one assistant message and one reasoning entry

### Requirement: Older history pages
An older-history page SHALL be applied from the agent's raw older-history response, including its structured user prompts and truncation entries. It SHALL be merged in front of the existing turns by history position. Events already in the view SHALL NOT be duplicated, and the streaming turn, optimistic prompts and live updates received since the load SHALL be kept.

#### Scenario: Older page is prepended
- **WHEN** an older-history page with 10 turns is applied to a view loaded from a window
- **THEN** the 10 turns come before the existing turns and the change report lists only them as added
- **AND** the history paging state takes the page's `before` token or reports no older history

#### Scenario: Prepending during a streaming turn keeps live state
- **WHEN** an older-history page is applied while the last turn is still receiving chunks
- **THEN** the last turn keeps all chunks received so far and later chunks keep merging into it

#### Scenario: Overlapping page events are not duplicated
- **WHEN** a page contains events that the view already holds
- **THEN** each event appears once in the view

#### Scenario: Older page restores prompt attachments
- **WHEN** an older-history page carries a structured user prompt with a text block and an image block
- **THEN** that turn's user message holds the text and the image attachment

### Requirement: Optimistic prompts
A prompt submitted by the client SHALL appear at once as a pending prompt after the last turn, identified by its client prompt id. When the agent's user message for it arrives (matched by the client prompt id it carries, or by the message id the prompt response names), the pending prompt SHALL be removed and the agent's user message SHALL stand for it in its run's turn. A prompt SHALL NOT be matched by content or arrival order.

#### Scenario: Agent echo replaces the optimistic prompt
- **WHEN** an optimistic prompt is followed by a `user_message_chunk` carrying the same client prompt id, a message id and a run id
- **THEN** the view holds one user message for that prompt, in the run's turn and with an id from the agent's message id, and no pending prompt

#### Scenario: Prompt response links the prompt
- **WHEN** an optimistic prompt's user message arrived without a client prompt id, and the prompt then completes with a response naming that message id
- **THEN** the pending prompt is removed and the agent's user message stands for it

### Requirement: Client-local prompts
A prompt reported as failed SHALL stay as a failed prompt. Pending and failed prompts are client-local and SHALL survive a load.

#### Scenario: Failed prompt is marked failed
- **WHEN** an optimistic prompt is reported as failed with a message
- **THEN** the prompt is marked failed with that message and no agent reply is attached to it

#### Scenario: Snapshot keeps client-local prompts
- **WHEN** a load response with a snapshot is applied while the view holds one pending and one failed prompt that the snapshot does not contain
- **THEN** the view still holds both prompts with their status, after the snapshot's turns

### Requirement: Replayed updates applied once
Notifications that the client marks as replayed (for example, buffered updates drained after a webview reattach) SHALL be ignored when an identical notification was already applied live in the same view. Live notifications SHALL always be applied.

#### Scenario: Drained duplicate is ignored
- **WHEN** a live tool call update is applied and the same notification arrives again marked as replayed
- **THEN** the view is unchanged and the change report counts one duplicate

#### Scenario: Repeated live chunk text is kept
- **WHEN** two live chunks with identical text and message id arrive
- **THEN** the message text contains the text twice

### Requirement: Run state
The view SHALL report the session's run state (idle, submitting, thinking, streaming, tool running, waiting for the user, cancelled or failed), the running run's id, the active tool call, the last stop reason and the last error. Whether a run is active SHALL come from the agent's state updates (`running`, `requires_action`, `idle` with its stop reason), for any run. Session updates SHALL only refine an active run (thinking, streaming, tool running).

#### Scenario: State update starts and ends a run
- **WHEN** a state update reports run `r1` running, then a state update reports idle with stop reason `end_turn`
- **THEN** the run state is active with run id `r1`, then idle with last stop reason `end_turn`

#### Scenario: Run started elsewhere is shown
- **WHEN** a state update reports a run running that this client did not prompt
- **THEN** the run state is active with that run's id

#### Scenario: Waiting for the user
- **WHEN** a state update reports `requires_action` during a run
- **THEN** the run state is waiting for the user, and a later `running` returns it to an active state

#### Scenario: Running tool sets tool-running state
- **WHEN** a `tool_call` with status `in_progress` arrives during a run
- **THEN** the run state is tool running and the active tool call is that tool

### Requirement: Run state without state updates
When the agent sends no state updates, prompt outcomes and runtime observations SHALL decide whether a run is active. A runtime observation SHALL count any non-idle phase as an active run.

#### Scenario: Completed prompt returns to idle
- **WHEN** the agent sends no state updates and the client reports the prompt completed with stop reason `end_turn`
- **THEN** the run state is idle and the last stop reason is `end_turn`

#### Scenario: Busy runtime observation is an active run
- **WHEN** a runtime observation reports phase tools, waiting or closing on an idle view
- **THEN** the run state is active

#### Scenario: Idle runtime observation ends a stale run
- **WHEN** a runtime observation reports the session idle while the view still shows a run
- **THEN** the run state becomes idle and tools still marked running are no longer reported as active

### Requirement: Usage and cost
The view SHALL report context usage, the token breakdown (latest request and session totals, with a partial flag) and cumulative cost from `usage_update` notifications and from the snapshot's accounting and summary, keeping the most recent authoritative value. Cost SHALL be taken only from accounting data, never summed from individual requests.

#### Scenario: Usage update replaces context usage
- **WHEN** a `usage_update` with `used = 12000` and `size = 200000` arrives
- **THEN** the view reports 12000 of 200000 context tokens used

#### Scenario: Snapshot accounting sets cost
- **WHEN** a snapshot with session accounting of 0.42 USD complete is applied
- **THEN** the view reports a cost of 0.42 USD marked complete

#### Scenario: Usage update carries the token breakdown
- **WHEN** a `usage_update` carries `_meta.querymt.usage` with latest-request and session-total tokens marked partial
- **THEN** the view reports both token breakdowns and marks the totals as approximate

### Requirement: Plans and reasoning parts
The view SHALL hold the session's current plan, replaced by `plan` and `plan_update` and cleared by `plan_removed`. Reasoning SHALL be kept as separate parts when updates carry a reasoning part id (`_meta.querymt.reasoning_part_id`); chunks of the same part SHALL merge.

#### Scenario: Plan update replaces and plan removal clears
- **WHEN** a `plan_update` with 3 entries arrives, then a `plan_update` with 2 entries, then a `plan_removed` for the same plan
- **THEN** the view's plan has 3 entries, then the 2 new entries, then no plan

#### Scenario: Reasoning parts stay separate
- **WHEN** `agent_thought_chunk` updates for one message arrive with reasoning part ids `p0`, `p0`, `p1`
- **THEN** that message holds two reasoning parts, `p0` with both of its chunks and `p1` with one

### Requirement: Session configuration, undo, inputs and subagents
The view SHALL hold the current config options, the undo stack and last undo result, pending queued and steered inputs, and the session's subagents, each updated by its matching event and merged by id. Subagents SHALL come from ACP `subagent_update` updates (an upsert per child session id) and from the snapshot's `subagentUpdates`. A tool call whose content references a child session SHALL be linked to that subagent.

#### Scenario: Config option update merges by id
- **WHEN** a `config_option_update` carries only the model option
- **THEN** the model option is replaced and the other options keep their values

#### Scenario: Input state notification updates a pending input
- **WHEN** an input-state notification moves a queued input to started
- **THEN** that input is no longer pending and the others are unchanged

#### Scenario: Undo stack is replaced
- **WHEN** an undo stack with two frames, one with a failed restore, is applied
- **THEN** the view lists both frames in order and marks the failed one

#### Scenario: Subagent update patches the child
- **WHEN** a `subagent_update` announces a child session as running, then another for the same child reports it idle
- **THEN** the view holds one subagent for that child, with state idle and the fields of the first update that the second did not change

#### Scenario: Tool call links a child session
- **WHEN** a `tool_call_update` carries content that references a child session announced by `subagent_update`
- **THEN** that tool call is linked to the child's subagent

#### Scenario: Snapshot restores subagents
- **WHEN** a snapshot whose `subagentUpdates` lists two children is applied
- **THEN** the view holds both subagents with their latest state

### Requirement: Session cursors
The view SHALL hold the session cursors the agent announces with ACP `cursor_update`, keyed by cursor id, each with its anchor and capabilities. The first anchor received for a cursor SHALL be kept; later updates SHALL only patch its capabilities. A cursor whose anchored entity is not in the view SHALL be kept but reported as unplaced, never placed by guess.

#### Scenario: Cursor is placed at its anchor
- **WHEN** a `cursor_update` with a fork capability is anchored after an agent message the view holds
- **THEN** the view lists the cursor with the fork capability, placed after that message's entry

#### Scenario: Capabilities are patched, the anchor is kept
- **WHEN** a later `cursor_update` for the same cursor clears its capabilities and names a different anchor
- **THEN** the cursor keeps its first anchor and has no capabilities

#### Scenario: Cursor without its entity is unplaced
- **WHEN** a `cursor_update` is anchored to a message the view does not hold
- **THEN** the cursor is listed as unplaced

### Requirement: Cursors across loads
A load SHALL discard the cursors held for the session and rebuild them from the cursors it carries. An older-history page SHALL add its cursors to the ones held.

#### Scenario: Load rebuilds cursors
- **WHEN** a view holding three cursors applies a load whose snapshot carries one cursor
- **THEN** the view holds only that one cursor

### Requirement: Truncated tool content
Tool arguments or results marked as truncated in a windowed snapshot SHALL be reported as truncated with their full size. Full content supplied later by the client SHALL replace the truncated value.

#### Scenario: Truncated result is marked
- **WHEN** a snapshot marks a tool result as truncated with a full size of 400000 bytes
- **THEN** that tool call reports a truncated result and 400000 full bytes

#### Scenario: Full content replaces the prefix
- **WHEN** the client supplies the full result for that tool call
- **THEN** the tool call reports the full result and is no longer truncated, and only its turn is reported as updated

### Requirement: Deterministic output
The same sequence of inputs SHALL produce the same view and change reports, byte for byte when serialized, natively and in WebAssembly. Times in the view SHALL come only from inputs.

#### Scenario: Repeated reduction is identical
- **WHEN** one recorded input sequence is applied twice to new views
- **THEN** the serialized views and change reports are identical

#### Scenario: WebAssembly matches native
- **WHEN** the conformance traces are applied by the native build and by the WebAssembly module
- **THEN** the serialized views and change reports are identical

### Requirement: Tolerant input handling
Unknown session update kinds and unknown agent event kinds SHALL be skipped and counted in the change report. A malformed input SHALL be rejected with an error in the change report and SHALL NOT change the view. Inside a snapshot or an older-history page, decoding SHALL be per event: an event that fails to decode SHALL be skipped and reported with its history position, and the other events SHALL be applied. No input SHALL abort the reducer.

#### Scenario: Unknown update kind is skipped
- **WHEN** a notification with an unknown `sessionUpdate` value arrives
- **THEN** the view is unchanged and the change report counts one unknown input

#### Scenario: Malformed input is rejected
- **WHEN** a batch contains a notification whose `update` is not an object, followed by a valid chunk
- **THEN** the change report holds an error for the first input and the valid chunk is applied

#### Scenario: Bad event inside a snapshot is skipped
- **WHEN** a snapshot of 200 events contains one event of a known kind whose fields fail to decode
- **THEN** the other 199 events are applied and the change report holds one error naming that event's history position

### Requirement: Bounded debug event log
The view SHALL keep a debug log of applied inputs (kind, short summary, message id) limited to the newest 1,000 entries, with a count of dropped entries.

#### Scenario: Debug log keeps the newest entries
- **WHEN** 1,500 inputs are applied
- **THEN** the debug log holds the newest 1,000 entries and reports 500 dropped

### Requirement: Native Rust library
The session view reducer SHALL be usable as a native Rust library by clients outside the QueryMT workspace. Its dependency tree SHALL contain no async runtime, no database driver and no workspace-internal helper crate, and a view SHALL be movable between threads.

#### Scenario: Out-of-workspace client builds
- **WHEN** a crate outside the workspace depends on the reducer through a git dependency pinned to a revision
- **THEN** it builds natively and for `wasm32-unknown-unknown`, and its dependency tree contains no tokio, rusqlite or `querymt-workspace-hack`

#### Scenario: Snapshot hydrated on a background thread
- **WHEN** a native client applies a large snapshot to a view on a background thread and then moves the view to its UI thread
- **THEN** the view on the UI thread equals a view that applied the same snapshot on the UI thread

### Requirement: Published WebAssembly module
Each nightly and stable QueryMT release SHALL publish the session view reducer as a WebAssembly module with JavaScript bindings, TypeScript declarations and a schema version, as `agent-session-core-wasm-<version>.tar.gz` with a `.sha256` file, both listed in `release.json`.

#### Scenario: Release lists the module
- **WHEN** a nightly release is published
- **THEN** `release.json` lists the module archive with its sha256 digest, and the archive contains the module, its bindings, its TypeScript declarations and metadata with the version, commit and schema version

#### Scenario: Module keeps state between batches
- **WHEN** a client creates a view in the module, applies a snapshot and then a chunk batch
- **THEN** the second change report reflects the state left by the first batch without the client resending it
