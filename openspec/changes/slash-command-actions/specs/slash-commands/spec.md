# Spec Delta

## ADDED Requirements

### Requirement: Command descriptor from the agent
For each advertised command the app SHALL read the agent's descriptor: its source, its effect (a model prompt or an action the agent runs), its input need (`none`, `optional`, `required`) and the submissions it accepts (prompt, queue). A list of accepted submissions that is present but empty SHALL mean the command accepts no submission.

#### Scenario: Descriptor states an optional input
- **GIVEN** the agent advertises `compact` with an input hint and the input need `optional`
- **THEN** the app treats `/compact` as runnable without arguments

#### Scenario: Empty list of accepted submissions
- **GIVEN** the agent advertises `freeze` with the accepted submissions `[]`
- **THEN** the app treats `/freeze` as accepting neither prompt nor queue, not as a command without a descriptor

### Requirement: Fields missing from the descriptor
Each descriptor field the agent did not send SHALL fall back on its own. Without an input need, an input hint SHALL mean required input and no hint no input. Without accepted submissions, the command SHALL accept prompt and queue. Without a source or effect, none SHALL be shown, and the command SHALL be treated as neither a model prompt nor an action.

#### Scenario: Command without a descriptor
- **GIVEN** an agent advertises `review` with the input hint "[scope]" and no descriptor
- **THEN** the app treats `/review` as requiring input and as accepting prompt and queue, as before this change
- **AND** the app treats it as neither a model prompt nor an action

### Requirement: Recognising a command in typed text
Text SHALL be treated as a slash command only when its first word is `/` followed by the exact name of a command the session advertises, including case, and the name ends the text or is followed by whitespace. Any other text SHALL be an ordinary message, whether or not it starts with `/`.

#### Scenario: Path is not a command
- **GIVEN** a turn is running, the composer's delivery choice is steer, and the session advertises no `tmp` command
- **WHEN** the user sends `/tmp/build.log is empty`
- **THEN** the message is steered into the running turn

#### Scenario: Name case must match
- **GIVEN** the session advertises `compact`
- **WHEN** the user sends `/COMPACT`
- **THEN** it is sent as an ordinary message

### Requirement: Submitting a command
A recognised command SHALL be sent as a prompt while no turn runs. While a turn runs it SHALL be queued, never steered, when it accepts queueing. A command that does not accept queueing SHALL NOT be sent while a turn runs: the composer SHALL keep its text and SHALL show why under it. The same rule SHALL apply to a command picked in the palette and to one typed in the composer.

#### Scenario: Typed command is queued, not steered
- **GIVEN** a turn is running and the composer's delivery choice is steer
- **WHEN** the user sends `/compact keep the plan`
- **THEN** the command is queued

#### Scenario: Typed prompt-only command during a turn
- **GIVEN** a turn is running and `worktree` accepts only the prompt submission
- **WHEN** the user sends `/worktree list`
- **THEN** nothing is sent, the composer still holds `/worktree list`, and "Available when the turn ends" is shown under it

#### Scenario: Typed prompt-only command when idle
- **GIVEN** no turn is running and the same `worktree`
- **WHEN** the user sends `/worktree list`
- **THEN** it is sent as a prompt

### Requirement: Palette shows what picking a command does
In the command palette, a command whose input is required SHALL end its name with `…`. A command whose effect is a model prompt SHALL be marked `Prompt` at the end of its row. A command with no descriptor SHALL show neither a source nor the mark.

#### Scenario: Required and optional input side by side
- **GIVEN** the session advertises `compact` (input optional) and `explain` (input required)
- **WHEN** the user opens the command palette
- **THEN** the rows read `compact` and `explain…`

#### Scenario: Prompt command is marked
- **GIVEN** the session advertises the built-in `compact` (an action) and the project command `explain` (a prompt)
- **WHEN** the user opens the command palette
- **THEN** the `explain…` row is marked `Prompt` and the `compact` row is not

### Requirement: Palette commands the session cannot take
When the open session cannot take a command now, the palette SHALL show the command dimmed, with the reason as its second line in place of the description. Picking it SHALL send nothing and SHALL keep the palette open. The row SHALL keep its place in the list, so the first row stays the best match. The reasons are:
- "Agent not connected";
- "Session is loading";
- "Session could not be loaded";
- "Available when the turn ends".

#### Scenario: Session not connected
- **GIVEN** the open session's agent is disconnected
- **WHEN** the user opens the command palette
- **THEN** each slash command is dimmed and shows "Agent not connected"

#### Scenario: Prompt-only command during a turn
- **GIVEN** a turn is running, `worktree` accepts only the prompt submission and `compact` accepts queueing
- **WHEN** the user opens the command palette
- **THEN** `worktree…` is dimmed with "Available when the turn ends" and `compact` is not dimmed

#### Scenario: Picking an unavailable command
- **GIVEN** the same session and palette
- **WHEN** the user types `worktree` and presses Enter
- **THEN** nothing is sent, the composer is unchanged and the palette stays open with `worktree…` as its first row

### Requirement: Adding arguments from the palette
Tab on a highlighted, available command that takes input SHALL put it in the composer instead of submitting it, and SHALL close the palette. Tab on any other row SHALL do nothing. While an available command with optional input is highlighted, the palette SHALL show below the list that Enter runs it and Tab adds arguments.

#### Scenario: Adding arguments to an optional-input command
- **GIVEN** `compact` advertises the input hint "[optional instructions…]" and the input need `optional`
- **WHEN** the user highlights `compact` in the palette and presses Tab
- **THEN** the palette closes, nothing is sent, and the composer contains `/compact `, has focus with the cursor at the end, and shows the hint

#### Scenario: Key hint for an optional-input command
- **GIVEN** the same `compact`
- **WHEN** `compact` is the highlighted row
- **THEN** the palette shows "Enter to run · Tab to add arguments" below the list
- **AND** the hint is not shown while a row of another group is highlighted

#### Scenario: Tab on a command without input
- **WHEN** the user highlights a command that takes no input and presses Tab
- **THEN** nothing is sent, the composer is unchanged and the palette stays open

## MODIFIED Requirements

### Requirement: Command names with or without a slash
The app SHALL accept advertised names with or without a leading `/`. It SHALL send every command as `/` followed by its name. The session composer's list SHALL show it as `/name`; the command palette, which lists commands rather than completing text, SHALL show the bare name, followed by `…` when the command's input is required.

#### Scenario: Spec-style name
- **WHEN** the agent advertises a command named `web`
- **THEN** it is shown as `/web` in the composer list, as `web` in the command palette, and sent as `/web`

#### Scenario: Name that already has a slash
- **WHEN** the agent advertises a command named `/compact`
- **THEN** it is shown and sent as `/compact` (shown as `compact` in the command palette), not `//compact`

### Requirement: Slash commands in the command palette
While a session is open and its agent advertises commands, the command palette SHALL list them in their own group. Each entry SHALL be a two-line row: the command's name without a `/` on the first line and, dimmed on the second, the agent's description of the command or the reason the session cannot take it now. The search SHALL match the name, the description, the input hint, the source and, for a prompt command, the word `prompt`.

#### Scenario: Palette lists the session's commands
- **GIVEN** the open session advertises `/compact` with the description "Summarize the conversation now to free up context"
- **WHEN** the user opens the command palette
- **THEN** the entry shows `compact` with "Summarize the conversation now to free up context" under it

#### Scenario: Palette searches the description
- **GIVEN** the same session
- **WHEN** the user types `free context`
- **THEN** `compact` is listed

#### Scenario: Palette searches the source
- **GIVEN** the session advertises the built-in `compact` and the project command `explain`
- **WHEN** the user types `project`
- **THEN** `explain…` is listed and `compact` is not

#### Scenario: Command without a description
- **WHEN** the agent advertises a command with an empty description
- **THEN** its entry shows the name only

#### Scenario: The best match is the first row
- **GIVEN** the open session advertises `compact`, and the palette also offers the action "Schedule a prompt…"
- **WHEN** the user types `comp`
- **THEN** `compact` is the first row and the one Enter runs, and "Schedule a prompt…" is still listed below it

### Requirement: Running a command from the palette
Picking an available command whose input is `none` or `optional` SHALL submit `/name` to the open session and close the palette. Picking one whose input is `required` SHALL put it in the session composer, focus the composer with the cursor at the end, and show the command's hint until arguments are typed. A draft already in the composer SHALL be kept as the command's arguments.

#### Scenario: Command without input
- **WHEN** the user picks a command that takes no input from the palette
- **THEN** `/name` is sent to the session and the palette closes

#### Scenario: Command with optional input
- **GIVEN** `compact` advertises the input hint "[optional instructions…]" and the input need `optional`
- **WHEN** the user picks `compact` from the palette with Enter or a click
- **THEN** `/compact` is sent at once and the palette closes

#### Scenario: Command with input
- **GIVEN** `explain` requires input and advertises the hint "[topic]"
- **WHEN** the user picks `explain…` from the palette
- **THEN** the composer contains `/explain `, has focus with the cursor at the end, and shows the hint

#### Scenario: Composer draft becomes the arguments
- **GIVEN** the composer holds the draft `closures in Rust`
- **WHEN** the user picks `explain…` from the palette
- **THEN** the composer contains `/explain closures in Rust` with the cursor at the end

#### Scenario: Palette command during a running turn
- **GIVEN** a turn is running on an agent that can queue input
- **WHEN** the user picks a command without input that accepts queueing
- **THEN** the command is queued, as a typed slash command would be
