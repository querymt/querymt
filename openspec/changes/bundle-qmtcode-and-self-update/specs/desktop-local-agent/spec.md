# Spec Delta

## Purpose

Defines how the QueryMT Desktop app finds and starts a local qmtcode agent: which copy a bare `qmtcode` command runs, when the "Local qmtcode" agent is added, and how a user removes it for good.

## ADDED Requirements

### Requirement: Bare qmtcode resolves to the bundled copy first
When a process agent's command line starts with the bare program name `qmtcode` (or `qmtcode.exe` on Windows), the app SHALL run the qmtcode bundled with the app if one exists, otherwise the first `qmtcode` found on PATH. The bundled copy is the one next to the app executable or in `../lib/querymt-gpui/` relative to it.

#### Scenario: Bundled copy wins over PATH
- **WHEN** a process agent with command `qmtcode --acp` starts, and both a bundled qmtcode and a qmtcode on PATH exist
- **THEN** the bundled qmtcode is started with the argument `--acp`

#### Scenario: PATH copy is used without a bundled copy
- **WHEN** a process agent with command `qmtcode --acp` starts, no bundled qmtcode exists and qmtcode is on PATH
- **THEN** the qmtcode found on PATH is started

#### Scenario: Linux lib layout is found
- **WHEN** the app runs from `<root>/usr/bin/` and qmtcode exists at `<root>/usr/lib/querymt-gpui/qmtcode`
- **THEN** a bare `qmtcode` command starts that copy

#### Scenario: Windows finds qmtcode.exe
- **WHEN** the app runs on Windows and `qmtcode.exe` is next to the app executable
- **THEN** a bare `qmtcode` command starts that `qmtcode.exe`

#### Scenario: No qmtcode anywhere
- **WHEN** a process agent with command `qmtcode --acp` starts, and qmtcode is neither bundled nor on PATH
- **THEN** the agent fails to start with an error that names `qmtcode` and says it was not found

### Requirement: Explicit commands run as written
A process agent command whose program is not exactly the bare name `qmtcode` SHALL be started as written. This covers a program given with a path and any other program name. The bundled copy SHALL NOT be substituted.

#### Scenario: Path to qmtcode is not replaced
- **WHEN** a process agent with command `/opt/qmt/qmtcode --acp` starts, and a bundled qmtcode exists
- **THEN** `/opt/qmt/qmtcode` is started, not the bundled copy

#### Scenario: Other programs are unaffected
- **WHEN** a process agent with command `other-agent --stdio` starts
- **THEN** `other-agent` is looked up and started as before

### Requirement: Agent log names the started copy
When a process agent starts, the agent log SHALL record which copy started: "bundled" or "PATH" for a resolved bare `qmtcode`, or "as written" otherwise. The entry SHALL include the full program path and the process id.

#### Scenario: Log shows bundled copy
- **WHEN** a bare `qmtcode` command starts the bundled copy
- **THEN** the agent log has an entry that says the bundled qmtcode started, with its full path and the process id

#### Scenario: Log shows PATH copy
- **WHEN** a bare `qmtcode` command starts the copy found on PATH
- **THEN** the agent log has an entry that says the PATH qmtcode started, with its full path and the process id

### Requirement: Local qmtcode agent is seeded when qmtcode is available
On desktop, the app SHALL add a "Local qmtcode" process agent with command `qmtcode --acp` and auto-start on, when a bundled qmtcode exists or qmtcode is on PATH. This applies on first start and on later starts. The app SHALL NOT add it when an agent with the same id or command line already exists, or when the user dismissed it.

#### Scenario: Bundled qmtcode seeds the local agent
- **WHEN** the app starts with a bundled qmtcode, no qmtcode on PATH and no "Local qmtcode" agent
- **THEN** a "Local qmtcode" agent with command `qmtcode --acp` and auto-start on is added

#### Scenario: Windows PATH qmtcode seeds the local agent
- **WHEN** the app starts on Windows without a bundled qmtcode, and `qmtcode.exe` is in a PATH directory
- **THEN** a "Local qmtcode" agent is added

#### Scenario: Nothing to seed without qmtcode
- **WHEN** the app starts with no bundled qmtcode and no qmtcode on PATH
- **THEN** no "Local qmtcode" agent is added

#### Scenario: Existing installs gain the local agent
- **WHEN** a user whose saved agents have no "Local qmtcode" agent and no dismissal starts a build with a bundled qmtcode
- **THEN** the "Local qmtcode" agent is added to their saved agents

### Requirement: Deleted seeded agents stay deleted
Deleting the "Local qmtcode" agent SHALL record it as dismissed in the persisted app state, and the app SHALL NOT add it again on later starts. Reset app data SHALL clear the dismissals. Agents requested at start with `--agent` or `QMT_AGENT` are not dismissible: they are added whenever requested.

#### Scenario: Deleted local agent does not return
- **WHEN** the user deletes the "Local qmtcode" agent and restarts the app, and a bundled qmtcode exists
- **THEN** no "Local qmtcode" agent is added

#### Scenario: Reset app data restores the local agent
- **WHEN** the user dismissed the "Local qmtcode" agent and then resets app data, and a bundled qmtcode exists
- **THEN** the "Local qmtcode" agent is present again

#### Scenario: Requested agent returns after deletion
- **WHEN** the user deletes an agent that was added by `--agent host:port` and starts the app again with `--agent host:port`
- **THEN** that agent is added again

#### Scenario: Older state files load without dismissals
- **WHEN** the app loads a saved state file written before dismissals existed
- **THEN** it loads without error and treats no seeded agent as dismissed

### Requirement: Agent processes start without a console window on Windows
On Windows, process agents SHALL be started without creating a console window.

#### Scenario: No console window for a local agent
- **WHEN** the app starts a process agent on Windows
- **THEN** the process is created with no console window, and its stdin, stdout and stderr stay connected to the app
