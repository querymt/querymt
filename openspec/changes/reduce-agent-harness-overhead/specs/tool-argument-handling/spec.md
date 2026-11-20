# Spec Delta

## ADDED Requirements

### Requirement: Path arguments expand a leading tilde
Every built-in tool argument that names a file or directory, including `workdir` of `shell`, SHALL treat a value of `~` or a value starting with `~/` as relative to the home directory of the user running the agent process on the session's host. Any other value, including `~name` forms and a `~` that is not the first character, SHALL be resolved as before. When the home directory cannot be determined, the call SHALL fail with an error naming the argument, and the tool SHALL NOT run.

#### Scenario: Directory under the home directory
- **WHEN** the model calls `ls` with `path = "~/.local/share"` and the agent runs as a user whose home is `/Users/wiking`
- **THEN** the listing is of `/Users/wiking/.local/share`, not of a `~` directory under the session's working directory

#### Scenario: Bare tilde
- **WHEN** the model calls `glob` with `path = "~"`
- **THEN** the search runs in the user's home directory

#### Scenario: Shell working directory
- **WHEN** the model calls `shell` with `command = "pwd"` and `workdir = "~/src"`
- **THEN** the output is the `src` directory under the user's home directory

#### Scenario: Tilde with a user name is not expanded
- **WHEN** the model calls `read_tool` with `path = "~alice/notes.md"`
- **THEN** the path is resolved against the session's working directory, as any other relative path

#### Scenario: Tilde inside a path is literal
- **WHEN** the model calls `read_tool` with `path = "backup/~old/notes.md"`
- **THEN** the path is resolved against the session's working directory with the `~old` segment unchanged

#### Scenario: Home directory unknown
- **WHEN** the model calls `ls` with `path = "~/data"` and the agent process has no determinable home directory
- **THEN** the call fails with an error naming `path` and nothing is listed

#### Scenario: Effective arguments show the expanded path
- **WHEN** a tool call with `path = "~/data"` is approved, scheduled and recorded
- **THEN** approval prompts and file-conflict detection use the expanded path, and the recorded assistant arguments still read `~/data`
