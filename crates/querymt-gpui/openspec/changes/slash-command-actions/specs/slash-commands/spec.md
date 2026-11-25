# Spec Delta

## ADDED Requirements

### Requirement: Command metadata from the agent
The app SHALL read, for each advertised command, its input arity (`none`, `optional` or `required`), whether the agent runs it natively instead of expanding it into a prompt, and which deliveries it accepts (prompt, queue, steer).

#### Scenario: Agent states an optional input
- **GIVEN** the agent advertises `compact` with an input hint and the arity `optional`
- **THEN** the app treats `/compact` as runnable without arguments

#### Scenario: Agent states the deliveries
- **GIVEN** the agent advertises `worktree` as native, accepting only the prompt delivery
- **THEN** the app does not queue or steer `/worktree`

### Requirement: Commands from agents without metadata
When the agent gives no metadata for a command, the app SHALL treat a command with an input hint as requiring input and one without as taking none, and SHALL treat it as a non-native prompt that accepts every delivery.

#### Scenario: Agent without metadata
- **GIVEN** the agent advertises `review` with the input hint "[scope]" and no metadata
- **THEN** the app treats `/review` as requiring input, as before this change

### Requirement: Palette shows what picking a command does
In the command palette, a command whose input is required SHALL end its name with `…`. A command with no input or an optional input SHALL show its name only.

#### Scenario: Required and optional input side by side
- **GIVEN** the session advertises `compact` (input optional) and `review` (input required)
- **WHEN** the user opens the command palette
- **THEN** the rows read `compact` and `review…`

### Requirement: Palette commands the session cannot run
When the open session cannot take a command now (its agent is not connected, it is loading or read-only, or the command does not accept the delivery a running turn would need), the palette SHALL show the command disabled with the reason as its second line, and picking it SHALL do nothing and keep the palette open.

#### Scenario: Session not connected
- **GIVEN** the open session's agent is disconnected
- **WHEN** the user opens the command palette
- **THEN** each slash command is shown disabled with "Agent not connected"

#### Scenario: Native command during a run
- **GIVEN** a turn is running and `/worktree` accepts only the prompt delivery
- **WHEN** the user opens the command palette
- **THEN** `worktree` is disabled with "Available when the agent is idle", and `compact`, which can be queued, is enabled

### Requirement: Commands shown as commands in the conversation
A user message that invoked an advertised command SHALL be drawn as a command row, not as a user prompt: the `/name` and its arguments in a monospace face, with a command icon and without the prompt bubble. The output of a native command SHALL be drawn under its command row as command output, distinct from an assistant reply, and SHALL be shown the same after the session is reloaded. A `/compact` invocation SHALL be shown as part of the compaction it produced rather than as its own row.

#### Scenario: Worktree command and its output
- **WHEN** the user sends `/worktree list` and the agent answers with the list
- **THEN** the conversation shows a command row `/worktree list` with the list as its output, and neither looks like a user prompt or an assistant reply

#### Scenario: Reloaded session
- **GIVEN** a session in which `/worktree list` was run
- **WHEN** the user reopens the session
- **THEN** the command row and its output are shown as they were live

#### Scenario: Compact
- **WHEN** the user runs `/compact focus on the API`
- **THEN** the conversation shows one compaction row whose details include the instructions "focus on the API", and no separate user prompt

#### Scenario: Prompt that only starts with a slash
- **WHEN** the user sends `/tmp is full, why?` and the session advertises no `tmp` command
- **THEN** it is shown as an ordinary user prompt

## MODIFIED Requirements

### Requirement: Running a command from the palette
Picking a command with no input or an optional input SHALL send `/name` to the open session as a prompt, following the same send rules as typed prompts. Picking a command whose input is required SHALL put `/name ` in the session composer, focus the composer, and show the command's hint until arguments are typed. For a command with an optional input, a secondary key (Tab) SHALL put `/name ` in the composer instead of sending it, and the row's tooltip SHALL say so.

#### Scenario: Command without input
- **WHEN** the user picks a command that takes no input from the palette
- **THEN** `/name` is sent to the session and the palette closes

#### Scenario: Command with optional input
- **GIVEN** `/compact` advertises the input hint "[optional instructions…]" with the arity `optional`
- **WHEN** the user picks `compact` from the palette with Enter or a click
- **THEN** `/compact` is sent at once and the palette closes

#### Scenario: Adding arguments to an optional-input command
- **GIVEN** the same `/compact`
- **WHEN** the user highlights `compact` in the palette and presses Tab
- **THEN** the composer contains `/compact `, has focus with the cursor at the end, and shows the hint

#### Scenario: Command with input
- **GIVEN** `/review` requires input and advertises the hint "[scope]"
- **WHEN** the user picks `review…` from the palette
- **THEN** the composer contains `/review `, has focus with the cursor at the end, and shows the hint

#### Scenario: Palette command during a running turn
- **GIVEN** a turn is running on an agent that can queue input
- **WHEN** the user picks a command without input that accepts queueing
- **THEN** the command is queued, as a typed slash command would be
