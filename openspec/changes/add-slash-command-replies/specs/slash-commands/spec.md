# Spec Delta

## ADDED Requirements

### Requirement: Record on a native command reply
A native command that answers with an agent message update SHALL carry the record on that update. When the command was refused or failed, the update SHALL also carry `_meta.querymt.slashCommandError` with the stable error code. When the outcome reaches the issuing session as a durable ownership-transferred or archived announcement, that announcement SHALL carry the record and no separate reply SHALL be sent.

#### Scenario: Worktree list reply carries the record
- **WHEN** a client submits `/worktree list` and it succeeds
- **THEN** the agent message update with the output carries `slashCommand` with `name` `worktree`, `arguments` `list` and `effect` `action`, and no `slashCommandError`

#### Scenario: Worktree reply carries the client prompt id
- **WHEN** a client submits `/worktree list` with `_meta.querymt.client_prompt_id` `c1`
- **THEN** the agent message update with the output has `slashCommand.clientPromptId` `c1`

#### Scenario: Refused worktree command carries its error code
- **WHEN** a client submits `/worktree archive` and it is refused because the owner is busy
- **THEN** the agent message update with the refusal carries the record and `slashCommandError` `owner_active`

#### Scenario: Handoff announcement carries the record
- **WHEN** a client submits `/worktree handoff review` and the handoff succeeds
- **THEN** the ownership-transferred announcement on that session's stream carries `slashCommand` with `arguments` `handoff review`
- **AND** no other agent message update is sent for the command

#### Scenario: Archive issued from another session
- **WHEN** session A submits `/worktree archive <id>` for a workspace owned by session B
- **THEN** A's reply carries the record
- **AND** the archived announcement on B's stream carries no `slashCommand`

### Requirement: Command reply when nothing else reports the outcome
An `action` command whose prompt completes without a compaction, an announcement or another reply SHALL be answered by one transient agent message update that carries the record and the outcome text. For a command submitted with `session/prompt`, the update SHALL be delivered before the prompt response. The agent SHALL NOT report these outcomes as a `notice`.

#### Scenario: Compact on an empty conversation replies
- **WHEN** a client submits `/compact` to a session with no messages
- **THEN** one agent message update says there is nothing to compact and carries the record, before the prompt response
- **AND** no `notice` and no `compaction_update` is sent for it

#### Scenario: Compact blocked by a hook replies
- **WHEN** a pre-compaction hook blocks a `/compact`
- **THEN** one agent message update carries the block reason, the record and `slashCommandError` `blocked_by_hook`
- **AND** no `compaction_update` is sent for it

### Requirement: A failed command prompt carries no record
A command whose prompt request fails with an error SHALL produce no record.

#### Scenario: Failed command prompt carries no record
- **WHEN** a client submits `/compact` together with an image block and the request fails with an invalid-prompt error
- **THEN** no session update carries a `slashCommand` for that prompt

### Requirement: Native command replies in replayed history
A durable native command reply SHALL be replayed with the record it carried live. A transient reply SHALL NOT be replayed.

#### Scenario: Replayed worktree publish reply keeps its record
- **WHEN** a session in which `/worktree publish` was run is replayed
- **THEN** the replayed reply carries `slashCommand` with `arguments` `publish`

#### Scenario: Transient reply is not replayed
- **WHEN** a session in which `/worktree list` was run is replayed
- **THEN** no update for that command is replayed

### Requirement: Action commands in the conversation
A command reply SHALL be drawn as a command row: a command icon, `/name` and its arguments in a monospace face, no prompt bubble, aligned with the agent's column. The reply text SHALL be drawn under the row as command output, not as an assistant reply. Output that is a JSON document SHALL be shown indented. A reply with an error code SHALL mark the row as failed.

#### Scenario: Worktree list and its output
- **WHEN** the user sends `/worktree list` and the agent answers with the list
- **THEN** the conversation shows a command row `/worktree list` with the list under it as output, and neither a prompt bubble nor an assistant reply

#### Scenario: Refused command
- **WHEN** the user sends `/worktree archive` and the agent refuses it with the code `owner_active`
- **THEN** the command row is marked as failed and the refusal message is shown under it

#### Scenario: Reply stored by the agent survives a reload
- **GIVEN** a session in which `/worktree publish` was run
- **WHEN** the user reopens the session
- **THEN** the command row and its output are shown as they were live

#### Scenario: Reply the agent does not store is gone after a reload
- **GIVEN** a session in which `/worktree list` was run
- **WHEN** the user reopens the session
- **THEN** no row is shown for that command

#### Scenario: Compact on an empty conversation
- **WHEN** the user runs `/compact` in a session with no messages and the agent replies that there is nothing to compact
- **THEN** the conversation shows a command row `/compact` with that reply under it

### Requirement: A manual compaction replaces its prompt
When the user runs `/compact` and a compaction starts for it, the conversation SHALL show the compaction row and no prompt bubble for the command.

#### Scenario: Compact shows no prompt bubble
- **WHEN** the user runs `/compact keep the API decisions`
- **THEN** the conversation shows one compaction row for it and no prompt bubble reading `/compact keep the API decisions`

### Requirement: A command being sent
A prompt the agent has not answered yet SHALL be drawn as a command row in progress when it names an advertised command whose descriptor states the effect `action`, and as a prompt bubble otherwise, including when the command has no descriptor. When the agent's entry for it arrives, that entry SHALL take its place. If sending fails, the row SHALL be marked as failed and show the error.

#### Scenario: Pending action command
- **WHEN** the user sends `/worktree list` and the agent has not answered yet
- **THEN** the conversation shows a command row `/worktree list` in progress, not a prompt bubble

#### Scenario: Pending prompt command
- **WHEN** the user sends `/explain closures` and the agent has not answered yet
- **THEN** the conversation shows a prompt bubble reading `/explain closures`

#### Scenario: Failed action command
- **WHEN** the user sends `/worktree list` and the request fails
- **THEN** the command row is marked as failed and shows the error

#### Scenario: Pending command without a descriptor
- **GIVEN** an agent that advertises `worktree` without a descriptor
- **WHEN** the user sends `/worktree list` and the agent has not answered yet
- **THEN** the conversation shows a prompt bubble reading `/worktree list`, not a command row

### Requirement: Command rows start a turn for activity
A command row SHALL count as the start of a turn for the session's activity indicators. Errors and streamed output that precede it SHALL NOT be reported as current.

#### Scenario: Error before a command row is not current
- **GIVEN** a conversation whose last turn ended with an error notice
- **WHEN** the user runs `/worktree list` and its command row is added
- **THEN** the session's activity reports no current error
