# Spec Delta

## ADDED Requirements

### Requirement: Subcommands in the command palette
When a command advertises subcommands, the command palette SHALL list one entry per subcommand instead of the command itself. Each entry SHALL show `name subcommand` on the first line and the subcommand's description on the second. The search SHALL match the command name, the subcommand name and the subcommand description. Picking an entry SHALL follow the subcommand's own input arity and deliveries, as for a command.

#### Scenario: Worktree operations listed
- **GIVEN** the session advertises `worktree` with the subcommands `list`, `status` and `remove`
- **WHEN** the user opens the command palette and types `worktree`
- **THEN** `worktree list`, `worktree status` and `worktree remove` are listed, and no bare `worktree` entry

#### Scenario: Run a subcommand without input
- **GIVEN** `list` takes no input
- **WHEN** the user picks `worktree list`
- **THEN** `/worktree list` is sent and the palette closes

#### Scenario: Search by what it does
- **GIVEN** `status` is described as "Show an isolated session's state"
- **WHEN** the user types `isolated state`
- **THEN** `worktree status` is listed

### Requirement: Destructive subcommands go through the composer
A subcommand the agent marks as destructive SHALL NOT be sent from the command palette. Picking it SHALL put `/name subcommand ` in the composer, focus it and show its hint, whatever its input arity. Its palette row SHALL carry a destructive marker (an icon and colour, with the word "Destructive" in its tooltip).

#### Scenario: Remove from the palette
- **GIVEN** `worktree remove` is destructive and its input is optional
- **WHEN** the user picks `worktree remove` with Enter
- **THEN** the composer contains `/worktree remove `, has focus, shows the hint, and nothing is sent

#### Scenario: Marked in the list
- **WHEN** the palette lists `worktree archive`, which is destructive
- **THEN** its row shows the destructive marker

### Requirement: Subcommand list in the session composer
When the composer's first word is a command with subcommands followed by a space, and the cursor is in the second word, the composer SHALL list the subcommands whose names match the second word, best match first, with their descriptions and the destructive marker. Up, Down, Tab, Enter, Esc and clicks SHALL behave as in the command list, completing the second word to `subcommand ` and keeping any later arguments.

#### Scenario: Typing a subcommand
- **GIVEN** the session advertises `worktree` with subcommands
- **WHEN** the user types `/worktree st`
- **THEN** `status` is listed and `list` is not

#### Scenario: Complete a subcommand
- **GIVEN** the list is shown for `/worktree li`
- **WHEN** the user presses Tab
- **THEN** the composer text is `/worktree list ` and nothing is sent

#### Scenario: Command without subcommands
- **WHEN** the user types `/compact ` and keeps typing
- **THEN** no subcommand list is shown
