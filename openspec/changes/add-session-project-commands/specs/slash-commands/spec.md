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
