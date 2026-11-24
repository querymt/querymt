# Spec Delta

## Purpose

Lets the user discover and run the slash commands an agent advertises for the open session (ACP `available_commands_update`), from the command palette and from the session composer.

## ADDED Requirements

### Requirement: Advertised commands follow the agent
The app SHALL hold, for the open session, the slash commands of the agent's latest `available_commands_update` for that session. Each update SHALL replace the whole list, and an empty update SHALL clear it. Malformed entries SHALL be skipped without discarding the rest. Commands of one session SHALL NOT appear in another session.

#### Scenario: Agent replaces its commands
- **GIVEN** the open session advertises `/compact` and `/worktree`
- **WHEN** the agent sends an update listing only `/compact`
- **THEN** only `/compact` is offered

#### Scenario: Agent clears its commands
- **WHEN** the agent sends an update with an empty list
- **THEN** no slash commands are offered for the session

#### Scenario: Commands of another session
- **WHEN** an update arrives for a session other than the open one
- **THEN** the commands offered for the open session do not change

### Requirement: Command names with or without a slash
The app SHALL accept advertised names with or without a leading `/`. It SHALL send every command as `/` followed by its name. The session composer's list SHALL show it as `/name`; the command palette, which lists commands rather than completing text, SHALL show the bare name.

#### Scenario: Spec-style name
- **WHEN** the agent advertises a command named `web`
- **THEN** it is shown as `/web` in the composer list, as `web` in the command palette, and sent as `/web`

#### Scenario: Name that already has a slash
- **WHEN** the agent advertises a command named `/compact`
- **THEN** it is shown and sent as `/compact` (shown as `compact` in the command palette), not `//compact`

### Requirement: Commands advertised before the session is shown
Commands the agent advertises for a session between creating it (new session, remote creation, fork) and the app showing it SHALL be offered once the session is shown.

#### Scenario: Commands sent right after creation
- **GIVEN** the user starts a new session
- **WHEN** the agent advertises `/compact` right after it answers the creation request, before the session view opens
- **THEN** `/compact` is offered once the session is open

### Requirement: Loading a session keeps its commands
The `session/load` response, with or without a snapshot, SHALL NOT clear or change the session's commands. Only an `available_commands_update` SHALL change them, whether it arrives before or after the load response.

#### Scenario: Advertisement before the load response
- **GIVEN** the user opens a session
- **WHEN** the agent advertises `/compact` and then answers the load request with a snapshot
- **THEN** `/compact` is offered once the session has loaded

#### Scenario: Large session loaded in the background
- **GIVEN** the user opens a session whose load response is large enough to be decoded off the UI thread
- **WHEN** the agent advertises `/compact` before the response, or while it is being decoded
- **THEN** `/compact` is offered once the session has loaded, exactly as for a small session

#### Scenario: Commands known before the session is opened
- **GIVEN** the agent advertised `/compact` for a session before the user opened it
- **WHEN** the user opens that session and its load is decoded in the background
- **THEN** `/compact` is offered once the session has loaded

#### Scenario: Advertisement after the load response
- **WHEN** the agent answers the load request and then advertises `/compact`
- **THEN** `/compact` is offered

### Requirement: Slash commands in the command palette
While a session is open and its agent advertises commands, the command palette SHALL list them in their own group. Each entry SHALL be a two-line row: the command's name without a `/` on the first line and the agent's description of the command, dimmed, on the second. The search SHALL match the name, the description and the input hint.

#### Scenario: Palette lists the session's commands
- **GIVEN** the open session advertises `/compact` with the description "Summarize the conversation now to free up context"
- **WHEN** the user opens the command palette
- **THEN** the entry shows `compact` with "Summarize the conversation now to free up context" under it

#### Scenario: Palette searches the description
- **GIVEN** the same session
- **WHEN** the user types `free context`
- **THEN** `compact` is listed

#### Scenario: Command without a description
- **WHEN** the agent advertises a command with an empty description
- **THEN** its entry shows the name only

#### Scenario: The best match is the first row
- **GIVEN** the open session advertises `compact`, and the palette also offers the action "Schedule a prompt…"
- **WHEN** the user types `comp`
- **THEN** `compact` is the first row and the one Enter runs, and "Schedule a prompt…" is still listed below it

### Requirement: Palette groups follow the best match
While the command palette has a search, its groups SHALL be ordered by the best match each contains, best first, so the first row is the best match of the whole palette. Groups whose best matches are equal SHALL keep their usual order (Actions, Slash commands, Go to, Sessions). With no search, the usual order applies.

#### Scenario: A command outranks an action
- **GIVEN** a session that advertises `compact`
- **WHEN** the user types `comp`
- **THEN** the Slash commands group is listed before the Actions group

#### Scenario: An action outranks the commands
- **GIVEN** a session that advertises `compact` and `worktree`
- **WHEN** the user types `sidebar`
- **THEN** the Actions group is listed first, and no slash command is listed

#### Scenario: No search
- **WHEN** the command palette opens
- **THEN** the groups are in their usual order

#### Scenario: No session open
- **WHEN** the command palette is opened on the Start page
- **THEN** no slash command group is shown

### Requirement: Running a command from the palette
Picking a command without an input specification SHALL send `/name` to the open session as a prompt, following the same send rules as typed prompts. Picking a command with an input specification SHALL put `/name ` in the session composer, focus the composer, and show the command's hint until arguments are typed.

#### Scenario: Command without input
- **WHEN** the user picks a command that takes no input from the palette
- **THEN** `/name` is sent to the session and the palette closes

#### Scenario: Command with input
- **GIVEN** `/compact` advertises the input hint "[optional instructions…]"
- **WHEN** the user picks `/compact` from the palette
- **THEN** the composer contains `/compact `, has focus with the cursor at the end, and shows the hint

#### Scenario: Palette command during a running turn
- **GIVEN** a turn is running on an agent that can queue input
- **WHEN** the user picks a command without input from the palette
- **THEN** the command is queued, as a typed slash command would be

### Requirement: Command list in the session composer
When the session composer's text starts with `/` and the cursor is still in the first word, the composer SHALL show the session's commands whose names match the text typed after `/`, best match first. Descriptions SHALL NOT be matched here. The list SHALL NOT be shown when the session advertises no commands or nothing matches.

#### Scenario: Typing a slash
- **GIVEN** the open session advertises `/compact` and `/worktree`
- **WHEN** the user types `/` in the empty composer
- **THEN** both commands are listed with their descriptions

#### Scenario: Narrowing the list
- **WHEN** the user types `/wt`
- **THEN** `/worktree` is listed and `/compact` is not

#### Scenario: Slash inside a message
- **WHEN** the composer text is `see a/b`
- **THEN** no command list is shown

### Requirement: Keyboard in the composer command list
While the command list is shown, Up and Down SHALL move the highlighted command. Tab and Enter SHALL replace the first word with `/name ` and keep any arguments already typed, without sending. Esc SHALL close the list without changing the text and without counting toward the double Esc that stops a running turn. Once the list is closed, these keys SHALL behave as they do without it.

#### Scenario: Complete with Tab
- **GIVEN** the list is shown for `/com` with `/compact` highlighted
- **WHEN** the user presses Tab
- **THEN** the composer text is `/compact `, nothing is sent, and the session mode does not change

#### Scenario: Enter completes instead of sending
- **GIVEN** the list is shown for `/wor`
- **WHEN** the user presses Enter
- **THEN** the composer text is `/worktree ` and no prompt is sent

#### Scenario: Esc closes the list only
- **GIVEN** a turn is running and the list is shown
- **WHEN** the user presses Esc once, then Esc again within 700 ms
- **THEN** the first Esc closes the list and the second does not stop the turn

#### Scenario: Sending after completion
- **GIVEN** the composer text is `/compact focus on the API` and the list is closed
- **WHEN** the user presses the send shortcut
- **THEN** `/compact focus on the API` is sent as the prompt

### Requirement: Mouse in the composer command list
Clicking a command in the composer list SHALL complete it as Tab does.

#### Scenario: Click a command
- **WHEN** the user clicks `/worktree` in the list
- **THEN** the composer text is `/worktree ` and the composer keeps focus
