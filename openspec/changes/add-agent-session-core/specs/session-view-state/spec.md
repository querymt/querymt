# Spec Delta

## Purpose

Turns the agent's session output (load snapshot, ACP session updates, older-history pages) and client observations into one deterministic conversation view with change reports, so every QueryMT client renders sessions the same way, natively or as WebAssembly.

## ADDED Requirements

### Requirement: Session view from a load snapshot
Given a session load response that carries `querymt/sessionLoadSnapshot.v1`, the session view SHALL contain the conversation turns, tool calls with their arguments and results, compaction boundaries, usage and cost accounting, delegation state, pending inputs, the runtime status and the history paging state that the snapshot describes.

#### Scenario: Full snapshot hydrates the conversation
- **WHEN** a load response with a complete snapshot of 3 prompts, their assistant replies and 4 tool calls is applied to an empty view
- **THEN** the view has 3 turns in history order, each with its prompt, its assistant text and its tool calls with final status
- **AND** the history paging state reports no older history

#### Scenario: Windowed snapshot takes whole-session state from the summary
- **WHEN** a windowed snapshot with `history.complete = false` and a `summary` is applied
- **THEN** token totals, the current model, pending inputs and child session links come from `summary`, and totals marked partial are reported as approximate
- **AND** the history paging state carries the `before` token and marks older history as available

#### Scenario: Load response without a snapshot yields an empty view
- **WHEN** a load response without `querymt/sessionLoadSnapshot.v1` is applied
- **THEN** the view has no turns and is ready to receive replayed ACP session updates

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
Context compaction SHALL be read from the standard ACP `compaction_update` and `compaction_summary_chunk` session updates (client capability `session.compaction`, `CompactionCapabilities`), following the ACP contract: ID-addressed upsert, position fixed by the first update, patch semantics for `summary`, `error` and `_meta`, chunks appended in receive order.

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

### Requirement: Snapshot and live stream equivalence
Hydrating a snapshot SHALL produce the same turns as applying the equivalent sequence of live ACP session updates, ignoring arrival timestamps. Splitting the same text into different chunk sizes SHALL produce the same view.

#### Scenario: Snapshot and live stream produce the same turns
- **WHEN** one generated session is applied once as a load snapshot and once as its live ACP notification stream
- **THEN** both views have equal turns, tool calls, compactions and usage

#### Scenario: Chunk size does not change the view
- **WHEN** the same assistant text is streamed once in 1-character chunks and once in 64-character chunks
- **THEN** both views are equal

### Requirement: Change reports
Every applied batch SHALL return a change report that lists the ids of added, updated and removed turns, whether the session header changed, and any ignored or rejected inputs. Turns not listed SHALL be unchanged. Turn ids SHALL stay stable for the life of the view.

#### Scenario: Streaming chunk reports only the last turn
- **WHEN** an `agent_message_chunk` extends the last turn of a 500-turn view
- **THEN** the change report lists exactly that turn as updated and no other turn

#### Scenario: Unchanged turns keep their content and id
- **WHEN** a tool call update for the last turn is applied
- **THEN** every other turn has the same id and equal content before and after the update

### Requirement: Older history pages
An older-history page SHALL be merged in front of the existing turns by history position. Events already in the view SHALL NOT be duplicated, and the streaming turn, optimistic prompts and live updates received since the load SHALL be kept.

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

### Requirement: Optimistic prompts
A prompt submitted by the client SHALL appear at once as a pending user message identified by its client prompt id. The agent's echo with the same client prompt id SHALL replace it in place. A prompt reported as failed SHALL be marked failed.

#### Scenario: Agent echo replaces the optimistic prompt
- **WHEN** an optimistic prompt is followed by a `user_message_chunk` carrying the same client prompt id
- **THEN** the view holds one user message for that prompt, in the optimistic prompt's position

#### Scenario: Failed prompt is marked failed
- **WHEN** an optimistic prompt is reported as failed with a message
- **THEN** the prompt is marked failed with that message and no agent reply is attached to it

### Requirement: Replayed updates applied once
Notifications that the client marks as replayed (for example, buffered updates drained after a webview reattach) SHALL be ignored when an identical notification was already applied live in the same view. Live notifications SHALL always be applied.

#### Scenario: Drained duplicate is ignored
- **WHEN** a live tool call update is applied and the same notification arrives again marked as replayed
- **THEN** the view is unchanged and the change report counts one duplicate

#### Scenario: Repeated live chunk text is kept
- **WHEN** two live chunks with identical text and message id arrive
- **THEN** the message text contains the text twice

### Requirement: Run state
The view SHALL report the session's run state (idle, submitting, thinking, streaming, tool running, cancelled or failed), the active tool call, the last stop reason and the last error, derived from session updates, prompt outcomes and runtime observations.

#### Scenario: Running tool sets tool-running state
- **WHEN** a `tool_call` with status `in_progress` arrives during a run
- **THEN** the run state is tool running and the active tool call is that tool

#### Scenario: Completed prompt returns to idle
- **WHEN** the client reports the prompt completed with stop reason `end_turn`
- **THEN** the run state is idle and the last stop reason is `end_turn`

#### Scenario: Idle runtime observation ends a stale run
- **WHEN** a runtime observation reports the session idle while the view still shows a run
- **THEN** the run state becomes idle and tools still marked running are no longer reported as active

### Requirement: Usage and cost
The view SHALL report context usage and cumulative cost from `usage_update` notifications and from the snapshot's accounting, keeping the most recent authoritative value. Cost SHALL be taken only from accounting data, never summed from individual requests.

#### Scenario: Usage update replaces context usage
- **WHEN** a `usage_update` with `used = 12000` and `size = 200000` arrives
- **THEN** the view reports 12000 of 200000 context tokens used

#### Scenario: Snapshot accounting sets cost
- **WHEN** a snapshot with session accounting of 0.42 USD complete is applied
- **THEN** the view reports a cost of 0.42 USD marked complete

### Requirement: Session configuration, undo, inputs and delegations
The view SHALL hold the current config options, the undo stack and last undo result, pending queued and steered inputs, and delegation activity, each updated by its matching event and merged by id.

#### Scenario: Config option update merges by id
- **WHEN** a `config_option_update` carries only the model option
- **THEN** the model option is replaced and the other options keep their values

#### Scenario: Input state notification updates a pending input
- **WHEN** an input-state notification moves a queued input to started
- **THEN** that input is no longer pending and the others are unchanged

#### Scenario: Undo stack is replaced
- **WHEN** an undo stack with two frames, one with a failed restore, is applied
- **THEN** the view lists both frames in order and marks the failed one

#### Scenario: Delegation update links a child session
- **WHEN** a delegation update for a tool call carries a child session id
- **THEN** that tool call is linked to the child session

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
Unknown session update kinds and unknown agent event kinds SHALL be skipped and counted in the change report. A malformed input SHALL be rejected with an error in the change report and SHALL NOT change the view. No input SHALL abort the reducer.

#### Scenario: Unknown update kind is skipped
- **WHEN** a notification with an unknown `sessionUpdate` value arrives
- **THEN** the view is unchanged and the change report counts one unknown input

#### Scenario: Malformed input is rejected
- **WHEN** a batch contains a notification whose `update` is not an object, followed by a valid chunk
- **THEN** the change report holds an error for the first input and the valid chunk is applied

### Requirement: Bounded debug event log
The view SHALL keep a debug log of applied inputs (kind, short summary, message id) limited to the newest 1,000 entries, with a count of dropped entries.

#### Scenario: Debug log keeps the newest entries
- **WHEN** 1,500 inputs are applied
- **THEN** the debug log holds the newest 1,000 entries and reports 500 dropped

### Requirement: Published WebAssembly module
Each nightly and stable QueryMT release SHALL publish the session view reducer as a WebAssembly module with JavaScript bindings, TypeScript declarations and a schema version, as `agent-session-core-wasm-<version>.tar.gz` with a `.sha256` file, both listed in `release.json`.

#### Scenario: Release lists the module
- **WHEN** a nightly release is published
- **THEN** `release.json` lists the module archive with its sha256 digest, and the archive contains the module, its bindings, its TypeScript declarations and metadata with the version, commit and schema version

#### Scenario: Module keeps state between batches
- **WHEN** a client creates a view in the module, applies a snapshot and then a chunk batch
- **THEN** the second change report reflects the state left by the first batch without the client resending it
