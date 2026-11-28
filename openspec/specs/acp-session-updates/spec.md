# acp-session-updates Specification

## Purpose

Defines the ACP v1 session notifications through which QueryMT keeps clients synchronized with dynamic session state, resource usage, plans, advisory notices, and context compaction.

## Requirements

### Requirement: Mode and configuration synchronization
QueryMT SHALL publish the current session mode and the complete current configuration-option set whenever an active session's mode, model, profile, or reasoning effort changes. Mode changes SHALL produce both `current_mode_update` and `config_option_update`; other configuration changes SHALL produce `config_option_update`. Each configuration update SHALL contain a self-consistent full replacement rather than a partial option patch.

#### Scenario: Session mode changes
- **WHEN** an active ACP session changes from one supported mode to another
- **THEN** QueryMT emits `current_mode_update` with the new mode
- **AND** QueryMT emits `config_option_update` containing the complete option set with the new mode selected

#### Scenario: Non-mode configuration changes
- **WHEN** the active model, profile, or reasoning effort changes
- **THEN** QueryMT emits `config_option_update` containing the complete option set with every current value reflected

#### Scenario: No effective configuration change
- **WHEN** a configuration request leaves all effective configuration values unchanged
- **THEN** QueryMT does not emit a redundant configuration update

### Requirement: Session metadata synchronization
QueryMT SHALL emit `session_info_update` when mutable, persisted session metadata changes. The update SHALL use ACP patch semantics, SHALL include the changed title or activity timestamp fields, and subsequent session-list results SHALL reflect the same persisted values.

#### Scenario: Session title is assigned or changed
- **WHEN** QueryMT persists a new title for an active session
- **THEN** it emits `session_info_update` with that title
- **AND** a later session-list response reports the same title

#### Scenario: Session activity timestamp changes
- **WHEN** QueryMT persists a new last-activity time for an active session
- **THEN** it emits `session_info_update` with an RFC 3339 `updatedAt` value representing that time

#### Scenario: Metadata is cleared
- **WHEN** QueryMT intentionally clears a mutable nullable metadata field
- **THEN** it emits that field as `null` rather than omitting it

#### Scenario: Todo plan changes on an active session
- **WHEN** QueryMT persists a new non-empty todo plan for a session active on a connection
- **THEN** it emits `session_info_update` on that connection whose `_meta` sets only the top-level key `querymt/plans`, to the value a later session-list response reports
- **AND** it does not send title, `updatedAt` or other `_meta` keys in that update

#### Scenario: Todo plan is emptied on an active session
- **WHEN** a session active on a connection empties its todo plan
- **THEN** QueryMT emits `session_info_update` on that connection with `_meta: {"querymt/plans": null}`

### Requirement: Context usage and cumulative cost reporting
QueryMT SHALL emit `usage_update` whenever reliable current context utilization becomes available or materially changes. `used` SHALL represent tokens currently occupying model context, including cached tokens, and `size` SHALL represent the active model's effective context limit. QueryMT SHALL include cumulative session USD cost only for a complete supported token-cost estimate and SHALL omit standard cost otherwise. Session cost SHALL span execution cycles and remain independent of context occupancy.

#### Scenario: Provider request reports usage
- **WHEN** a provider request completes with token usage and the active model has a known context limit
- **THEN** QueryMT emits `usage_update` with the current context tokens as `used` and the effective context limit as `size`
- **AND** it includes cumulative USD cost when session accounting provides a complete supported estimate

#### Scenario: Active model changes
- **WHEN** the active model changes to one with a different known context limit
- **THEN** the next usage update uses the new model's effective limit
- **AND** prior charges remain priced using their recorded basis rather than the new model's rates

#### Scenario: Context size is unknown
- **WHEN** QueryMT cannot determine a meaningful effective context limit
- **THEN** it does not emit a malformed or estimated `usage_update`
- **AND** it emits replacement accounting through metadata-only `session_info_update._meta.querymt.accounting` and includes accounting directly in load snapshots without fabricated context utilization

#### Scenario: Compaction reduces context
- **WHEN** compaction completes and QueryMT can determine the resulting context utilization
- **THEN** it emits a new `usage_update` reflecting the reduced context usage
- **AND** accounting includes attributable compaction charges without reducing already incurred spend

#### Scenario: New cycle in a retained session
- **WHEN** a later execution cycle completes requests in a session with recorded earlier spend
- **THEN** cumulative session cost includes recorded charges from both cycles
- **AND** its value agrees with a restored snapshot of the same durable accounting

### Requirement: Plan lifecycle and compatibility
QueryMT SHALL report its current task plan through ACP. When the client advertises plan-operations support, QueryMT SHALL use a stable plan identity with `plan_update` full replacements and `plan_removed` lifecycle notifications. Otherwise, QueryMT SHALL retain the legacy `plan` update behavior and SHALL NOT send unsupported plan-operation variants.

#### Scenario: Capable client receives a plan
- **WHEN** a client advertises plan-operations support and QueryMT creates or changes its task plan
- **THEN** QueryMT emits an item-based `plan_update` using the same stable plan ID for that plan's lifetime
- **AND** the update contains the complete current entries

#### Scenario: Capable client sees plan removal
- **WHEN** a previously reported plan no longer exists or its lifecycle is explicitly dismissed
- **THEN** QueryMT emits `plan_removed` once for that plan ID

#### Scenario: Legacy client receives a plan
- **WHEN** a client does not advertise plan-operations support and QueryMT creates or changes its task plan
- **THEN** QueryMT emits the legacy `plan` update
- **AND** it does not emit `plan_update` or `plan_removed`

### Requirement: Advisory session notices
QueryMT SHALL emit ACP `notice` updates only for user-relevant advisory conditions that do not require acknowledgement and do not determine protocol correctness. On ACP v1 connections, QueryMT SHALL send notices only when the client advertises session-notice support. Notices SHALL remain separate from conversation history and SHALL NOT replace errors, permission requests, or other response-bearing interactions.

#### Scenario: Advisory event for a capable client
- **WHEN** a user-relevant advisory event occurs and the ACP v1 client advertises notice support
- **THEN** QueryMT emits a `notice` with an appropriate severity, non-empty title, and optional description

#### Scenario: Client lacks notice support
- **WHEN** an advisory event occurs and the ACP v1 client does not advertise notice support
- **THEN** QueryMT does not emit a `notice`
- **AND** it uses an existing user-visible fallback only when the advisory information must still be surfaced

#### Scenario: Condition requires user action
- **WHEN** continued execution depends on a user decision or authorization
- **THEN** QueryMT uses the applicable response-bearing ACP primitive instead of a notice

### Requirement: Compaction lifecycle reporting
QueryMT SHALL represent each context compaction as one ID-addressed ACP entity. On ACP v1 connections, it SHALL emit compaction updates only when the client advertises compaction support. QueryMT SHALL send an initial `in_progress` update before summary chunks, reuse the same unique compaction ID through one terminal status, and expose only unencrypted user-displayable summary content.

#### Scenario: Successful compaction with a final summary
- **WHEN** a capable client is connected and context compaction starts
- **THEN** QueryMT emits `compaction_update` with a new session-unique ID and `in_progress` status
- **AND WHEN** compaction succeeds with a safe user-displayable summary
- **THEN** QueryMT emits `compaction_update` for the same ID with `completed` status and the authoritative summary

#### Scenario: Incremental compaction summary
- **WHEN** safe summary content becomes available incrementally during compaction
- **THEN** QueryMT emits each `compaction_summary_chunk` after the initial update and before the terminal update using the same compaction ID

#### Scenario: Compaction fails or is cancelled
- **WHEN** compaction terminates unsuccessfully after its initial update
- **THEN** QueryMT emits exactly one terminal `failed` or `cancelled` update for the same compaction ID
- **AND** a failure description is included only for the `failed` state

#### Scenario: Summary is unsafe or unavailable
- **WHEN** compaction output is encrypted, contains internal framing, or has no user-displayable projection
- **THEN** QueryMT omits the summary while still reporting the lifecycle status

#### Scenario: Client lacks compaction support
- **WHEN** an ACP v1 client does not advertise compaction support
- **THEN** QueryMT emits neither `compaction_update` nor `compaction_summary_chunk`

### Requirement: Capability isolation by connection
QueryMT SHALL derive Preview update support from each ACP connection's negotiated client capabilities and SHALL apply those capabilities only to notifications delivered through that connection.

#### Scenario: Clients advertise different Preview capabilities
- **WHEN** two clients connected to QueryMT advertise different plan, notice, or compaction capabilities
- **THEN** each client receives only the update variants it advertised plus ungated stable updates

#### Scenario: Capability is omitted or null
- **WHEN** a client omits a Preview capability or supplies it as `null`
- **THEN** QueryMT treats that capability as unsupported

### Requirement: Replay semantics
QueryMT SHALL replay durable session-update state in materialized form when session history is replayed. Replayed plans and compactions SHALL retain their original identities and final content without duplicating streamed chunks. QueryMT SHALL NOT replay live-only notices or transient in-progress compaction states. For a remote-hosted session, replay SHALL be built from the local copy of the session's events and SHALL NOT cause events already transferred for the load to be transferred again.

#### Scenario: Replay includes a current plan
- **WHEN** a capable client loads a session with a retained plan
- **THEN** QueryMT replays one materialized `plan_update` using the retained plan ID and current complete content

#### Scenario: Replay includes completed compaction
- **WHEN** a capable client loads a session containing a completed compaction
- **THEN** QueryMT replays one terminal `compaction_update` with its original ID and materialized summary
- **AND** it does not also replay historical summary chunks that would duplicate the summary

#### Scenario: Replay excludes notices and transient progress
- **WHEN** QueryMT replays retained session history
- **THEN** it does not replay prior notices
- **AND** it does not replay an obsolete `in_progress` compaction state before a retained terminal state

#### Scenario: Replay of a remote session after attach
- **WHEN** a client that receives history as session updates loads a remote-hosted session whose events were transferred during attach
- **THEN** it receives the same updates as for a local session with the same events
- **AND** no event crosses the mesh a second time for the replay

### Requirement: Run stops are reported to clients
When a prompt's run ends on a stop rather than normal completion, QueryMT SHALL return the matching ACP stop reason in the prompt response. Clients that advertise notice support SHALL also receive a live warning `notice` naming the cause. Stop notices are live-only and SHALL NOT be replayed. The prompt response SHALL NOT carry QueryMT-specific stop metadata: the ACP stop reason is the protocol-level signal, and the cause is conveyed by the notice and by QueryMT's own event stream.

#### Scenario: Output-limit stop with a notice-capable client
- **WHEN** a run ends because the model hit its output-token limit and the client advertises notice support
- **THEN** the client receives a warning `notice` whose title names the output-token limit
- **AND** the prompt response has stop reason `max_tokens`

#### Scenario: Client without notice support
- **WHEN** the same stop occurs on a client that does not advertise notice support
- **THEN** no `notice` is sent and the prompt response has stop reason `max_tokens`

#### Scenario: Replay after a stop
- **WHEN** a client loads a session whose last run ended on a stop
- **THEN** QueryMT does not replay the stop notice

### Requirement: Accounting uncertainty replaces stale cost
Accounting updates and snapshots SHALL use the same versioned DTO with scoped known subtotals, nullable complete totals, completeness reasons, and coverage/exclusion counts. A present payload SHALL atomically replace accounting, including explicit nulls; absence SHALL mean no accounting update. Partial amounts SHALL NOT appear as complete standard cost. Live delivery and replay SHALL produce equivalent accounting for the same durable records.

#### Scenario: Replacement payload describes monetary scopes
- **WHEN** QueryMT publishes accounting
- **THEN** it provides session API-spend and explicitly identified active-cycle scopes, each with completeness state, reasons, and unresolved/incomplete/excluded counts and historical coverage
- **AND** it represents no active cycle explicitly and keeps cycle budget valuation and reasons separate from API pricing

#### Scenario: Message has no accounting payload
- **WHEN** a message omits accounting metadata
- **THEN** it does not update QueryMT accounting state
- **AND** omission of standard ACP `cost` alone does not define QueryMT clearing semantics

#### Scenario: Missing cache rate after a complete estimate
- **WHEN** a session previously reported complete cost and a later request has an unpriced nonzero cache category
- **THEN** the new accounting metadata reports the known subtotal and the missing cache-rate reason
- **AND** the standard complete cost is omitted and QueryMT's stored complete-total state is cleared rather than retaining the previous value

#### Scenario: Replay includes excluded billing
- **WHEN** accounting contains subscription-excluded attempts alongside metered API attempts
- **THEN** replay describes the excluded scope and reports only applicable metered charges
- **AND** an entirely excluded session is not represented as a known zero-dollar provider invoice

#### Scenario: Duplicate remote events
- **WHEN** the same accounting record is observed through replay and remote live delivery
- **THEN** the resulting metadata and session total match single delivery

### Requirement: Existing token metadata retains its scope
QueryMT SHALL preserve `_meta.querymt.usage` v1's JSON shape and main-request telemetry scope alongside accounting metadata. Retry and auxiliary accounting SHALL NOT silently broaden its latest-request or session token totals. Token telemetry and all-attempt monetary accounting SHALL remain independently scoped.

#### Scenario: Auxiliary settlement changes accounting
- **WHEN** compaction or delegation-summary charges settle without a new main request completing
- **THEN** accounting includes those charges
- **AND** existing usage v1 latest-request and session token totals are not replaced with all-attempt totals

### Requirement: Accounting delivery does not require context or notices
QueryMT SHALL carry `_meta.querymt.accounting` on `usage_update` when reliable main-context occupancy and the active model's effective limit are available. Otherwise it SHALL emit the same DTO through metadata-only `session_info_update`, independently of notice capability. Accounting delivery SHALL NOT fabricate context or change session-summary fields solely to carry accounting.

#### Scenario: Auxiliary cost changes with valid main context
- **WHEN** auxiliary charges change accounting and a reliable main-context measurement and active-model limit are available
- **THEN** QueryMT emits a usage update with that measurement and the updated accounting
- **AND** auxiliary token usage does not overwrite main-context occupancy

#### Scenario: Unknown context and unsupported notices
- **WHEN** accounting changes without a reliable context measurement/limit pair and the client has not negotiated notices
- **THEN** QueryMT emits metadata-only `session_info_update` carrying replacement accounting without changing session-summary fields
- **AND** it sends neither a fabricated `usage_update` nor an unsupported notice

#### Scenario: Model switch invalidates a prior measurement
- **WHEN** the active model changes and the previous context measurement has not been validated for the new model
- **THEN** accounting remains available through the metadata-only fallback
- **AND** QueryMT does not pair a stale measurement or obsolete limit with the new model merely to emit a usage update

### Requirement: Load snapshots contain authoritative ordered accounting
`SessionLoadSnapshot.accounting` SHALL contain the shared reducer's authoritative replacement DTO at the existing snapshot cursor, without requiring historical replay or known context limits. Snapshot and captured-live application SHALL respect existing local/remote ordering and deduplication. Older state SHALL NOT overwrite newer accounting or restore an invalidated complete total. When a client requests a load window, `accounting` SHALL still cover the entire session, not only the events in the window. When older history of a remote-hosted session is not yet held locally, `accounting` SHALL cover the locally held events and SHALL report the missing history through the existing completeness metadata: `complete` false, `completeTotalUsd` null and a reason identifying untransferred history. Once that history is local, the reason SHALL be absent.

#### Scenario: Desktop follow-up consumes backend accounting
- **WHEN** the backend client contract is documented for desktop implementation
- **THEN** snapshot and notification handling use the same authoritative accounting DTO instead of reconstructing monetary totals from request-end history
- **AND** metadata-only accounting updates do not require catalog refreshes unless actual session-summary fields change

#### Scenario: Snapshot hydration without historical replay
- **WHEN** a client hydrates from a load snapshot without historical notification replay and context size is unknown
- **THEN** the snapshot supplies accounting including failed and auxiliary attempts
- **AND** its totals and reasons agree with reducer-backed live state at that cursor

#### Scenario: Newer partial accounting survives load merging
- **WHEN** an older snapshot reports a complete amount and newer captured-live accounting explicitly clears that amount
- **THEN** the newer accounting wins according to the existing cursor ordering
- **AND** merging candidates does not refill the explicit null from the older complete amount

#### Scenario: Manual budget pricing differs from API pricing
- **WHEN** manual budget rates cannot price a used cache category but API pricing can
- **THEN** accounting preserves the complete API estimate separately from the incomplete budget valuation and budget-specific reason
- **AND** ordinary in-flight attempts alone do not trigger a missing-price budget warning

#### Scenario: Windowed load reports whole-session accounting
- **WHEN** the same session is loaded once with a load window that excludes older turns and once without a window, and the same history is held locally for both
- **THEN** both snapshots carry identical accounting values and completeness metadata

#### Scenario: Remote older history not yet local
- **WHEN** a windowed load of a remote-hosted session is answered before its older history is held locally
- **THEN** `accounting` reports `complete` false, a null `completeTotalUsd` and a reason identifying untransferred history
- **AND** a later load after the history is local reports the exact accounting without that reason

### Requirement: Budget uncertainty is advisory and visible
A configured USD budget with incomplete cycle pricing SHALL surface a best-effort warning identifying unavailable pricing or usage. Execution SHALL continue against the known subtotal unless it reaches the cap. Notices SHALL follow negotiated capability rules and SHALL NOT require acknowledgement. Missing context size SHALL NOT suppress this warning, and repeated usage snapshots SHALL NOT flood clients with duplicate warnings.

#### Scenario: Capable client receives a partial-budget warning
- **WHEN** a cycle's configured cap cannot include a used cache category because its rate is missing
- **THEN** a notice-capable client receives an advisory explaining best-effort enforcement
- **AND** execution continues while known spend remains below the cap

#### Scenario: Client does not support notices
- **WHEN** a client lacks notice support and budget accounting becomes incomplete
- **THEN** QueryMT preserves the condition in accounting metadata and snapshots and uses the existing user-visible warning fallback
- **AND** it does not send an unsupported notice or block waiting for a user response

#### Scenario: Known subtotal exhausts an incomplete budget
- **WHEN** the known cycle subtotal reaches the cap despite unresolved charges
- **THEN** the next paid attempt is stopped with the existing price-limit outcome
- **AND** the reported accounting remains explicitly incomplete

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

### Requirement: Active time in the whole-session summary
The whole-session summary SHALL carry the session's active time in `activeMs`: the sum, over every run of the session that has ended, of the time from its start to its end, as recorded in the journal. A run that has not ended SHALL NOT be counted; its start is reported by the runtime status (`run_started_at_ms`). The value SHALL cover the whole session, not only the loaded window.

#### Scenario: Ended runs are summed
- **WHEN** a session has two ended runs of 30 and 90 seconds
- **THEN** its summary carries `activeMs` 120000, with and without a load window

#### Scenario: Running run is not counted
- **WHEN** a session is loaded while its third run is active
- **THEN** the summary's `activeMs` covers the two ended runs, and the runtime status names the active run's start

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
