# Spec Delta

## Purpose
How gpui holds and shows the conversation of the open session: one shared reducer as its source, identity that survives reloads, prompts, runs, compactions, plan, usage, older history and the client-only state shown alongside.

## ADDED Requirements

### Requirement: One reducer as the transcript source
The open session's transcript SHALL be built only from the QueryMT session reducer. Every agent input for that session (load response, session updates including replayed ones, state updates, runtime and input-state notifications, older-history pages, full tool content, undo stacks, config option responses, and the client's prompt submissions and outcomes) SHALL be applied to it in the order received. Inputs for another session SHALL NOT change the transcript.

#### Scenario: Replayed updates before the load response
- **WHEN** an agent replays history as session updates before answering the load of the open session
- **THEN** the transcript shows the replayed conversation once, and the load response does not duplicate it

#### Scenario: Update for another session
- **WHEN** a session update for a different session id arrives on the same connection
- **THEN** the open session's transcript is unchanged

### Requirement: Same transcript live and after a reload
Reopening a session SHALL show the same rows, in the same order and with the same row identities, as watching it live, for every row whose content did not change. Rows whose identity is unchanged SHALL keep their expand and collapse state across a reload.

#### Scenario: Reload keeps rows and expand state
- **WHEN** a user expands a tool call in a session they watched live, then reloads the session
- **THEN** the transcript shows the same rows in the same order and the tool call is still expanded

#### Scenario: Reload during a streaming reply
- **WHEN** a session is reopened while an assistant reply is streaming
- **THEN** the reply appears once and in full when it finishes, with no repeated or missing text

### Requirement: Unchanged rows are not rebuilt
Applying agent input SHALL rebuild only the rows of turns that changed. A row whose content did not change SHALL keep its key, its revision and its tool-call object.

#### Scenario: Streaming into a long session
- **WHEN** an assistant chunk arrives in the last turn of a session with 200 turns
- **THEN** only rows of the last turn are rebuilt, and rows of the other turns keep their key, revision and tool-call object

### Requirement: Turns are agent runs
A turn SHALL be one agent run. A steering message applied to a running run SHALL appear inside that run's turn, and a compaction during a run SHALL stay in that turn. Consecutive reasoning and tool rows of a turn SHALL be shown as one work group.

#### Scenario: Steering stays in the running turn
- **WHEN** the user steers a running run and the agent applies the message
- **THEN** the message appears in the running turn after the output before it, and the turn continues with the agent's following output

#### Scenario: Run started without a prompt
- **WHEN** a scheduled run produces output in the open session
- **THEN** the transcript shows a new turn with that output and no user message

### Requirement: Prompts and their echo
A prompt submitted by the user SHALL appear at once as a pending message after the last turn. When the agent's copy of that prompt arrives, matched by the client prompt id gpui sent with it or by the message id in the prompt response, the pending message SHALL be replaced by the agent's copy in its run's turn. Prompts SHALL NOT be matched by their text.

#### Scenario: Prompt replaced by its echo
- **WHEN** the user sends a prompt with an image and the agent echoes it
- **THEN** the transcript shows the prompt once, with its text and image, in the run's turn

#### Scenario: Two identical prompts
- **WHEN** the user sends the same text twice in a row
- **THEN** each prompt is matched to its own echo and the transcript shows both

### Requirement: Failed and pending prompts
A prompt that fails to submit SHALL stay visible marked as failed, with the failure shown. Pending and failed prompts SHALL survive a reload of the session.

#### Scenario: Pending and failed prompts survive a reload
- **WHEN** the session is reloaded while one prompt is pending and another has failed
- **THEN** both still appear after the last turn with their status

#### Scenario: Failed prompt
- **WHEN** sending a prompt fails
- **THEN** the prompt stays in the transcript marked failed, with the error shown

### Requirement: Unconfirmed steer and queue inputs
A steer or queue input SHALL appear in the activity area as soon as the user sends it, before the agent confirms it, and SHALL follow the agent's input states from then on. An input state that arrives before the submit response SHALL NOT be lost. An input SHALL leave the activity area when the agent applies, starts, discards or fails it.

#### Scenario: Input state overtakes the submit response
- **WHEN** the agent reports a steer input as applied before gpui has received the response to submitting it
- **THEN** the input does not stay in the activity area as sending once the response arrives

### Requirement: Run state
The transcript's running indicator, its run id for steering and its work timer SHALL follow the agent's run state reports for any run, including runs gpui did not start. Waiting for a permission decision SHALL be shown as waiting for the user. Without run state reports (older agents), prompt outcomes and runtime notifications SHALL decide.

#### Scenario: Run started from another client
- **WHEN** another client prompts the open session
- **THEN** gpui shows the session running, steering targets that run, and the timer runs until the run ends

#### Scenario: Run ends
- **WHEN** the agent reports the run idle with stop reason `end_turn`
- **THEN** the running indicator and the timer stop

### Requirement: Compactions
A context compaction SHALL appear as one divider at the position the agent gives it, with its status, its summary, its error when it failed, and the context tokens before and after when the agent reports them. The divider SHALL look the same live and after a reload.

#### Scenario: Compaction live and after reload
- **WHEN** a compaction completes while the user watches, and the session is later reloaded
- **THEN** both show one divider at the same place with the same summary and token numbers

### Requirement: Plan
The session's plan SHALL be the agent's current plan, replaced by each plan update and cleared when the agent removes it, and SHALL be the same live and after a reload.

#### Scenario: Todo list after reload
- **WHEN** the agent updates its todo list during a run and the session is later reloaded
- **THEN** the plan shows the same entries and states

### Requirement: Usage and cost
The context indicator, token breakdown and cost SHALL come from the agent's usage and accounting data, live and after a reload, including billing scope, incompleteness reasons and budget. A session with no model calls SHALL show no cost. After the model changes, the context indicator SHALL show no usage until a request with the new model reports it.

#### Scenario: Subscription usage
- **WHEN** a session's accounting reports only subscription-excluded attempts
- **THEN** the cost panel shows subscription usage and no dollar amount, live and after a reload

#### Scenario: Model change hides stale context
- **WHEN** the user switches the model after a request
- **THEN** the context indicator shows no usage until the next request completes

### Requirement: Delegated sessions
A delegate tool call SHALL link to the child session the agent reports for it, live and after a reload, and the user SHALL be able to open that child from the tool call.

#### Scenario: Delegate links its child
- **WHEN** a delegate tool call starts a child session
- **THEN** its row links to the child and opening the link shows the child session

### Requirement: Older history
Scrolling to the start of a windowed transcript SHALL load the previous page and insert it above, keeping the rows already shown and the scroll position. A failed page load SHALL be shown and retryable.

#### Scenario: Older page keeps the view stable
- **WHEN** an older page loads while the user reads the top of the transcript
- **THEN** the page's turns appear above, earlier rows are unchanged, and the row the user was reading stays in place

### Requirement: Full tool content
A truncated tool argument or result SHALL be marked as truncated with its full size, and loading its full content SHALL replace it in place, with the loading and failure states shown on that tool call.

#### Scenario: Load a truncated result
- **WHEN** the user asks for the full result of a truncated tool call
- **THEN** the tool call shows loading, then the full result, and no other row changes

### Requirement: Client notices
Messages that only gpui produces (connection errors, stopped runs, failed submissions, notes after a load) SHALL appear in the transcript after the turn that was last when they occurred, and SHALL NOT be sent to or expected from the agent.

#### Scenario: Stopped run
- **WHEN** the user cancels a run
- **THEN** a "Stopped." note appears after that run's turn
