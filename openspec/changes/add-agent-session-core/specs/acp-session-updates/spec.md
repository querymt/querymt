## ADDED Requirements

### Requirement: Run state notifications
QueryMT SHALL send a `querymt/session/stateUpdate` extension notification to every connection on which a session is active, for each change of the session's foreground work. It SHALL carry the session id, the state (`running`, `requires_action` or `idle`, as in ACP v2 `state_update`) and the run id; `idle` SHALL also carry the run's ACP stop reason. Runs started by a prompt and runs started otherwise, such as scheduled runs, SHALL be reported alike.

#### Scenario: Prompted run reports running and idle
- **WHEN** a client prompts a session and the run ends normally
- **THEN** the client receives `running` with the run id, then `idle` with the same run id and stop reason `end_turn`

#### Scenario: Scheduled run is reported
- **WHEN** a scheduled run starts and ends in a session that is active on a connection
- **THEN** that connection receives `running` and `idle` for the run, with no prompt involved

#### Scenario: Waiting for permission
- **WHEN** a run waits for a permission decision and then continues
- **THEN** the client receives `requires_action` and then `running` for that run

### Requirement: Run state delivery
Every run state transition SHALL be delivered in order, and none SHALL be skipped because a later one followed quickly. State notifications are live-only and SHALL NOT be replayed. After a load, QueryMT SHALL send the session's current state once, after the load response and before any other live update for the session.

#### Scenario: Short run is not coalesced
- **WHEN** a run starts and ends before the client reads its notifications
- **THEN** the client still receives both `running` and `idle` for the run, in that order

#### Scenario: Current state after a load
- **WHEN** a client loads a session while a run is active
- **THEN** after the load response it receives one `running` notification for that run, before any other live update for the session
- **AND** it receives no historical state notification

### Requirement: Run identity on session updates
Message, thought, tool call and compaction session updates that belong to a run SHALL carry that run's id in `_meta.querymt.runId`, both live and in history replay. A user message inserted into a running run, such as a steering prompt, SHALL carry the id of the run it was inserted into. Updates that belong to no run SHALL carry no run id.

#### Scenario: Live updates carry the run id
- **WHEN** a run streams an assistant message and runs a tool
- **THEN** the message chunks, the `tool_call` and its `tool_call_update`s each carry that run's id

#### Scenario: Replay carries the same run id
- **WHEN** the same session is later replayed to a client
- **THEN** the replayed user message, assistant message, thought and tool call updates carry the run ids they carried live

#### Scenario: Steering prompt joins the running run
- **WHEN** a steering prompt is applied to run `r1`
- **THEN** its `user_message_chunk` carries run id `r1`

### Requirement: Prompt response names the inserted message
The `session/prompt` response SHALL carry, in `_meta.querymt.messageId`, the id of the user message that the prompt inserted. That id SHALL be the `messageId` of the prompt's `user_message_chunk` updates and SHALL stay the same in history replay and in load snapshots. A prompt that was rejected before insertion SHALL have no message id.

#### Scenario: Response matches the echo
- **WHEN** a prompt is inserted and its run completes
- **THEN** the prompt response's `_meta.querymt.messageId` equals the `messageId` of the prompt's `user_message_chunk` updates

#### Scenario: Replay keeps the id
- **WHEN** the session is loaded later
- **THEN** the prompt's user message has that same message id in the replay and in the load snapshot

### Requirement: Gap-free session loads
A connection that loads a session SHALL receive every session update for the events after the load's cut point exactly once, and none for the events at or before it. The cut point is the last event the load returns: the load snapshot's stream cursor, or the last replayed event. This SHALL hold for local and remote-hosted sessions.

#### Scenario: Event before the cut point is not delivered again
- **WHEN** an event is contained in the load snapshot
- **THEN** the client receives no live update for that event on the loading connection

### Requirement: Live updates held during a load
Live updates produced while a load is in progress SHALL be held and sent after the load response, and after the replayed history for clients that receive history as session updates.

#### Scenario: Event during the load is delivered once
- **WHEN** a run persists an event after the agent has read the load's history and before it answers the load
- **THEN** the client receives that event's updates once, after the load response

#### Scenario: Remote session load has no gap
- **WHEN** a client loads a remote-hosted session while its run persists events
- **THEN** the client receives each event after the cut point exactly once, as for a local session

### Requirement: Load during a streaming message
When a session is loaded while an assistant message or a reasoning part is streaming and not yet stored, the loading connection SHALL receive the text streamed up to the cut point as one chunk with that message's id (and reasoning part id), after the load response and before the later chunks of the same message. The message's stored form SHALL NOT then be delivered to that connection in addition to its chunks.

#### Scenario: Message streaming at the load arrives in full
- **WHEN** a client loads a session while an assistant message has streamed 400 characters, and the message then finishes with 300 more
- **THEN** the client receives one chunk with the first 400 characters, then the remaining chunks, and the text it holds is the whole message once

#### Scenario: Reasoning streaming at the load arrives in full
- **WHEN** a client loads a session while a reasoning part is streaming
- **THEN** the client receives the reasoning text streamed so far as one thought chunk with the part's id, followed by the later chunks

### Requirement: Cursors in load snapshots and history pages
For a client that advertises `session.cursors`, a load snapshot and an older-history page SHALL carry, in `cursorUpdates`, the `cursor_update` of every cursor anchored to an entity they return, with the same cursor ids that history replay announces. For a client that does not advertise it, they SHALL carry no cursors.

#### Scenario: Snapshot carries the window's cursors
- **WHEN** a cursor-capable client loads a session window that contains two forkable agent messages
- **THEN** the snapshot's `cursorUpdates` holds the cursors anchored after those two messages, with the ids a replay of the same session announces

#### Scenario: Older page carries its cursors
- **WHEN** a cursor-capable client requests an older-history page
- **THEN** the page's `cursorUpdates` holds the cursors anchored to the entities in that page only

#### Scenario: Client without cursor support
- **WHEN** a client that does not advertise `session.cursors` loads a session
- **THEN** the snapshot has no `cursorUpdates`

### Requirement: Context invalidation on model change
When a session switches to a different model, QueryMT SHALL tell every live client that the last context measurement no longer applies, through a `session_info_update` whose `_meta.querymt.contextInvalidated` is `true`, together with the model it switched to in `_meta.querymt.model`. The update SHALL change neither the title nor the activity timestamp. It SHALL be sent whether or not the client advertises notice support, and it SHALL NOT be replayed.

#### Scenario: Model change after a request
- **WHEN** a session's model changes after a request reported context usage
- **THEN** each live client receives a `session_info_update` with `_meta.querymt.contextInvalidated` `true`, the new model in `_meta.querymt.model` (`provider`, `model`, and when known `node` and `contextLimit`), and no title or timestamp

#### Scenario: Replay does not repeat the signal
- **WHEN** a session with a model change is replayed
- **THEN** the replay carries no `contextInvalidated` signal and reports no context usage taken before the change

### Requirement: One stale-context rule on every path
A model is the same model when its provider, model name and context limit are unchanged: selecting it again with another configuration or on another node SHALL NOT send the invalidation signal and SHALL keep the measurement. The same rule SHALL decide whether a measurement is stale in live delivery, history replay and the load snapshot's summary.

#### Scenario: Same model selected again
- **WHEN** a session selects the model it already uses with another configuration
- **THEN** no `contextInvalidated` signal is sent and the last context measurement stays valid on every path

#### Scenario: Compaction reports the context
- **WHEN** a compaction finishes and reports the context the session holds afterwards
- **THEN** that value is the last context measurement on every path: live delivery, history replay, the load snapshot's summary, and a connection seeded from a load

### Requirement: Load snapshots carry whole-session state
Every load snapshot SHALL carry the whole-session summary (token usage totals, the last context measurement, the current model and provider with its context limit, pending inputs and child-session links), whether or not the client requested a load window. For the same session and the same locally held history, the summary SHALL be identical with and without a window.

#### Scenario: Load without a window has a summary
- **WHEN** a client that does not request a load window loads a session with completed model calls
- **THEN** the snapshot carries a summary with the session's token totals and last context measurement

#### Scenario: Same summary with and without a window
- **WHEN** a session is loaded once without a window and once with a window that covers the whole session
- **THEN** both snapshots carry the same summary

### Requirement: Compaction token counts
Compaction updates SHALL carry `_meta.querymt.compaction` with `tokensBefore` (the session's last context measurement before the compaction started, or the compaction's token estimate when there is none) and, once known, `tokensAfter` (the context tokens after the compaction). The values SHALL be the same live, in history replay and in a load snapshot's projection of the same events. A count that is not known SHALL be omitted.

#### Scenario: Live compaction reports its counts
- **WHEN** a request reports 150000 context tokens, then a compaction starts and ends with 20000 context tokens
- **THEN** the `in_progress` update carries `tokensBefore` 150000 and the `completed` update carries `tokensBefore` 150000 and `tokensAfter` 20000

#### Scenario: Replay reports the same counts
- **WHEN** the same session is replayed
- **THEN** the replayed `completed` compaction update carries `tokensBefore` 150000 and `tokensAfter` 20000

#### Scenario: No measurement before the compaction
- **WHEN** a compaction starts with a token estimate of 90000 and no earlier context measurement
- **THEN** its updates carry `tokensBefore` 90000

### Requirement: Replayed compactions keep their place
History replay SHALL send each finished compaction at the position of the event that finished it, between the conversation updates around it, and not before the whole conversation. A compaction recorded without an id SHALL be replayed as one compaction from its start to its end.

#### Scenario: Compaction between two prompts
- **WHEN** a session with a prompt, a completed compaction and a second prompt is replayed
- **THEN** the client receives the first user message, then the compaction, then the second user message

#### Scenario: Compaction without an id
- **WHEN** a session recorded before compactions carried ids is replayed
- **THEN** each compaction start and its end are replayed as one compaction
