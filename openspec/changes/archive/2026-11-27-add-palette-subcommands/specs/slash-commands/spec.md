## ADDED Requirements

### Requirement: Subcommands in the catalog
A catalog entry MAY carry `_meta.querymt.subcommands`: a list of objects, each with a one-word `name` and a one-line `description`. An entry without subcommands SHALL omit the field. The `worktree` entry SHALL list `create`, `handoff`, `list`, `status`, `result`, `publish`, `merge`, `archive`, `remove`, `recover`, in that order. The agent SHALL accept every advertised name as the first word of the command's arguments.

#### Scenario: Worktree subcommands are advertised
- **WHEN** the catalog of a local session on an agent with the workspace service is advertised
- **THEN** the entry `worktree` carries `subcommands` with the ten names above, in that order, each with a non-empty description

#### Scenario: Advertised subcommands are accepted
- **WHEN** `/worktree` is run with any advertised subcommand name as its first argument, followed by the arguments that subcommand needs
- **THEN** the agent does not answer with the `/worktree` usage error

#### Scenario: Command without subcommands omits the field
- **WHEN** a session's catalog is advertised
- **THEN** the entry `compact` and every user-defined command carry no `subcommands`

### Requirement: Subcommands from the agent
For each advertised command, the app SHALL read the subcommands from `_meta.querymt.subcommands`. An element without a non-empty `name`, or whose name contains whitespace, SHALL be dropped. When two elements share a name, the first SHALL be kept. A missing `description` SHALL be read as empty. A command without the field, or with a value that is not a list, SHALL have no subcommands.

#### Scenario: Malformed subcommand entries are dropped
- **GIVEN** the agent advertises `worktree` with the subcommands `list`, an entry with no name, `two words` and `list` again
- **THEN** the app treats `worktree` as having the single subcommand `list`

#### Scenario: Agent without subcommands
- **GIVEN** an agent advertises `worktree` without `subcommands`
- **THEN** the app treats `worktree` as having no subcommands, and the palette shows only the `worktree…` row

### Requirement: Subcommands in the command palette
The palette SHALL show one row per subcommand, labelled `<command> <subcommand>…`, with the subcommand's description (or the parent's unavailable reason) as its second line. With no search the rows SHALL follow the command's row in advertised order; with a search they rank like any row. The search SHALL also match the subcommand's description and the parent's source.

#### Scenario: Palette lists worktree subcommands
- **GIVEN** the open session advertises `worktree` with its ten subcommands
- **WHEN** the user opens the command palette
- **THEN** the Slash commands group shows `worktree…` followed by `worktree create…` through `worktree recover…` in the advertised order, each with its description under it

#### Scenario: Searching for a subcommand
- **GIVEN** the same session
- **WHEN** the user types `worktree status`
- **THEN** `worktree status…` is the first row

#### Scenario: Searching a subcommand's description
- **GIVEN** the same session
- **WHEN** the user types the words of the `recover` description
- **THEN** `worktree recover…` is listed

### Requirement: Picking a subcommand from the palette
Picking an available subcommand row with Enter, Tab or a click SHALL close the palette and put `/<command> <subcommand> ` in the composer, focused with the cursor at the end. Nothing SHALL be sent. A composer draft SHALL be kept as it is when a command is picked. While the session cannot take the parent command, its subcommand rows SHALL be dimmed with the parent's reason, and picking one SHALL change nothing and keep the palette open.

#### Scenario: Picking a subcommand prefills it
- **GIVEN** the same session, no turn running and an empty composer
- **WHEN** the user picks `worktree list…` with Enter
- **THEN** the palette closes, nothing is sent, and the composer contains `/worktree list ` with focus and the cursor at the end

#### Scenario: Tab on a subcommand prefills it
- **GIVEN** the same session
- **WHEN** the user highlights `worktree status…` and presses Tab
- **THEN** the palette closes, nothing is sent, and the composer contains `/worktree status `

#### Scenario: Subcommand keeps the composer draft
- **GIVEN** the composer holds the draft `feature-x`
- **WHEN** the user picks `worktree create…`
- **THEN** the composer contains `/worktree create feature-x` with the cursor at the end

#### Scenario: Subcommand during a turn
- **GIVEN** a turn is running and `worktree` accepts only the prompt submission
- **WHEN** the user opens the command palette
- **THEN** every `worktree` subcommand row is dimmed with "Available when the turn ends"
- **AND** picking one sends nothing, leaves the composer unchanged and keeps the palette open

## MODIFIED Requirements

### Requirement: Adding arguments from the palette
Tab on a highlighted, available command that takes input SHALL put it in the composer instead of submitting it, and SHALL close the palette. Tab on an available subcommand row SHALL do what picking it does. Tab on any other row SHALL do nothing. While an available command with optional input is highlighted, the palette SHALL show below the list that Enter runs it and Tab adds arguments.

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
