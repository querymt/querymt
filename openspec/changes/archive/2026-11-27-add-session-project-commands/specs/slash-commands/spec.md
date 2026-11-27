# Spec Delta

## ADDED Requirements

### Requirement: Project commands follow the session's folder
For a local session, the agent SHALL read project commands (`<root>/.qmt/commands`) from the session's project root, and SHALL advertise and expand a session's commands from that one registry. A session that runs in a managed worktree SHALL keep using its checkout. Global commands, configured paths, the scripts policy and the discovery switches SHALL follow the agent's configuration.

#### Scenario: Session in a plain folder sees its project commands
- **GIVEN** an agent started without a working directory and a folder `~/proj` that holds `.qmt/commands/explain.md`
- **WHEN** a session opens in `~/proj`
- **THEN** `explain` is in that session's advertised commands, with source `project`
- **AND** a prompt `/explain closures` in that session expands the folder's `explain.md`

#### Scenario: Sessions in different folders differ
- **GIVEN** two folders that define `deploy` with different templates
- **WHEN** a session is opened in each and `/deploy` is sent
- **THEN** each session expands its own folder's template

#### Scenario: Idle session reports the same commands as a live one
- **GIVEN** a session whose actor is not running
- **WHEN** its command catalog is requested
- **THEN** it lists the same user commands the session lists once its actor runs

#### Scenario: Agent-level commands do not leak into other folders
- **GIVEN** an agent whose startup working directory holds `.qmt/commands/agent-only.md`
- **WHEN** a session opens in a different folder that has no such file
- **THEN** `agent-only` is not advertised for that session and `/agent-only` is not expanded

#### Scenario: Managed worktree session unchanged
- **GIVEN** a session bound to an isolated checkout that holds `.qmt/commands/deploy.md` different from its source checkout's
- **WHEN** `/deploy` is sent
- **THEN** the isolated checkout's template is expanded

### Requirement: The session's project root
The project root of a session SHALL be the root of the Git checkout that contains the session's working directory, or the working directory itself when it is not inside a Git repository. The agent SHALL NOT search folders above that root.

#### Scenario: Session in a subfolder of a repository
- **GIVEN** a Git repository whose root holds `.qmt/commands/deploy.md`
- **WHEN** a session opens in `<root>/crates/api`
- **THEN** `deploy` is advertised and expands from the repository root's file

#### Scenario: Folder outside Git does not search parents
- **GIVEN** a folder `~/work` that holds `.qmt/commands/lint.md` and is not a Git repository
- **WHEN** a session opens in `~/work/scratch`
- **THEN** `lint` is not advertised and `/lint` is not expanded

### Requirement: Sessions that keep the agent-level commands
A session without a working directory, an agent with command discovery off or unconfigured, and a session hosted on another mesh node SHALL keep the agent-level commands, or none.

#### Scenario: Session without a working directory keeps the agent-level commands
- **GIVEN** an agent with startup commands
- **WHEN** a session has no working directory
- **THEN** its catalog and expansion use the agent-level commands

#### Scenario: Discovery switched off
- **GIVEN** an agent with command discovery disabled
- **WHEN** a session opens in a folder that holds `.qmt/commands/explain.md`
- **THEN** `explain` is not advertised and `/explain` is not expanded

### Requirement: Command files are read without a restart
When a command file in the user's global folder (`~/.qmt/commands`) or in a live session's `<root>/.qmt/commands` is added, edited or removed, the agent SHALL use the new set for that session's next prompt. It SHALL also send the session's connected clients an `available_commands_update` with the full catalog, built-ins included, without a restart or reopen. A running turn SHALL keep the commands it started with.

#### Scenario: New global command appears in an open session
- **GIVEN** an open session and an existing `~/.qmt/commands` folder without `standup.md`
- **WHEN** `~/.qmt/commands/standup.md` is created
- **THEN** the session's clients receive an `available_commands_update` that lists `standup` with source `global`, together with the built-ins
- **AND** `/standup` in that session expands the new file

#### Scenario: New project command appears in an open session
- **GIVEN** an open session in `~/proj` whose `.qmt/commands` is not ignored by Git
- **WHEN** `~/proj/.qmt/commands/explain.md` is created
- **THEN** the session's clients receive an `available_commands_update` that lists `explain` with source `project`
- **AND** `/explain closures` in that session expands the new file

#### Scenario: Edited template is used by the next prompt
- **GIVEN** an open session that has run `/explain` from `.qmt/commands/explain.md`
- **WHEN** the file's template is changed and `/explain` is sent again
- **THEN** the new template is expanded

#### Scenario: Deleted command disappears
- **GIVEN** an open session that advertises the project command `explain`
- **WHEN** `.qmt/commands/explain.md` is deleted
- **THEN** the session's clients receive an `available_commands_update` without `explain`
- **AND** `/explain` is sent to the model as ordinary text

#### Scenario: Session without a working directory sees global edits
- **GIVEN** an open session without a working directory
- **WHEN** a file in `~/.qmt/commands` is created
- **THEN** that session's catalog and expansion include it

#### Scenario: Unrelated file changes send no update
- **GIVEN** an open session in `~/proj`
- **WHEN** a file outside `.qmt/commands`, or a non-`.md` file inside it, changes
- **THEN** no `available_commands_update` is sent

#### Scenario: Configured paths are read at startup only
- **GIVEN** an agent with a configured `slash_commands.paths` entry
- **WHEN** a file in that path is created while a session is open
- **THEN** that change alone sends no update, and the session lists the file once it next opens or its commands are next reloaded

#### Scenario: Ignored commands folder is read on open
- **GIVEN** a repository whose `.gitignore` ignores `.qmt/`, and an open session in it
- **WHEN** `.qmt/commands/explain.md` is created
- **THEN** no update is sent, and `explain` is listed when the session next opens, loads or resumes
