# Spec Delta

## MODIFIED Requirements

### Requirement: Failed refresh preserves the known skill set

A refresh SHALL publish its replacement skill set atomically. WHEN a configured source folder that is selected for discovery exists but cannot be read, and a skill set was already published for the workspace, THEN the refresh SHALL fail, the agent SHALL retain the previously published skill set unchanged, and the skill tool SHALL remain usable. A missing source directory SHALL count as an empty source rather than a refresh failure.

#### Scenario: Unreadable source does not publish a partial snapshot

- **WHEN** a refresh that follows an earlier successful one encounters an unreadable skill source while other sources are intact
- **THEN** no partial replacement is published
- **AND** previously discovered skills remain listed and loadable

#### Scenario: Missing source is treated as empty

- **WHEN** a previously present configured source directory is deleted
- **THEN** refresh succeeds and removes skills that came only from that source

#### Scenario: Invalid skill does not block valid skills

- **WHEN** a traversable source contains both a malformed skill definition and valid skill definitions
- **THEN** refresh succeeds with the malformed skill excluded and the valid skills available

## ADDED Requirements

### Requirement: Unreadable entries do not fail a refresh

An entry inside a readable source that cannot be read or followed, and an invalid individual skill definition, SHALL be reported as a problem and excluded. It SHALL NOT fail the refresh and SHALL NOT hide the other skills of that source.

#### Scenario: Unreadable entry inside a source does not fail the refresh

- **WHEN** a readable source contains one skill directory that cannot be read and one valid skill
- **THEN** refresh succeeds and the valid skill is listed
- **AND** the unreadable directory is recorded as a problem of kind `entry_unreadable`

### Requirement: First discovery offers the readable sources

WHEN no skill set has been published yet for a workspace and a selected source folder exists but cannot be read, THEN the skills of the readable sources SHALL be published and the unreadable source SHALL be recorded as a problem.

#### Scenario: First discovery with an unreadable source offers the readable sources

- **WHEN** the first discovery for a workspace finds one source folder unreadable and another source holding a valid skill
- **THEN** the valid skill is listed
- **AND** the unreadable source is recorded as a problem of kind `source_unreadable`

### Requirement: Linked skill folders are discovered

Skill discovery SHALL follow symbolic links to directories inside every skill source, including links whose target lies outside the workspace and outside the source folder. A skill reached through a link SHALL be offered and loaded like any other skill of that source. A link that cannot be followed SHALL be reported as a problem and SHALL NOT prevent discovery of the other skills in that source.

#### Scenario: Skill folder linked from outside the workspace is offered

- **WHEN** `<workspace>/.agents/skills/shared` is a symbolic link to a directory outside the workspace that contains a valid `SKILL.md`
- **THEN** the skill tool listing includes that skill
- **AND** loading it by callable ID returns its content

#### Scenario: Skill inside a linked category folder is offered

- **WHEN** `<workspace>/.agents/skills/team` is a symbolic link to a directory that contains `review/SKILL.md`
- **THEN** the skill tool listing includes `review`

#### Scenario: Broken skill link does not hide sibling skills

- **WHEN** a source contains a symbolic link whose target does not exist and a valid skill directory
- **THEN** the valid skill is listed
- **AND** the link is recorded as a problem of kind `broken_link`

#### Scenario: Link loop is reported and discovery completes

- **WHEN** a project `.agents/skills` source contains a symbolic link that points to one of its own parent directories
- **THEN** the refresh completes and lists the valid skills of that source
- **AND** the link is recorded as a problem of kind `link_loop`

#### Scenario: Supporting file of a linked skill is read from the link target

- **WHEN** the model loads a linked skill with `file: "templates/checklist.md"` and that file exists in the link target
- **THEN** the tool returns that file's content

#### Scenario: Supporting file path cannot leave a linked skill's target

- **WHEN** the model loads a linked skill with a `file` path whose symbolic link resolves outside the skill's link target
- **THEN** the load fails with an error and no file content is read

#### Scenario: Linked file inside a skill is listed with the skill's files

- **WHEN** a loaded skill's directory contains a symbolic link to a regular file
- **THEN** the skill tool output lists that entry among the skill's sample files

### Requirement: Skill discovery ignores version-control ignore rules

Skill discovery SHALL NOT apply `.gitignore`, `.ignore` or global version-control exclude rules to skill sources. A skill directory SHALL be offered whether or not version control ignores it.

#### Scenario: Gitignored skill folder is offered

- **WHEN** a workspace's `.gitignore` lists `.agents/skills/local-notes` and that directory contains a valid `SKILL.md`
- **THEN** the skill tool listing includes `local-notes`

### Requirement: Nested depth of project `.agents/skills` is configurable

The `nested_depth` key of a skills table (`[agent.skills]`, the `skills` table of a `[[delegates]]` entry, the deprecated `[planner.skills]`) SHALL set how many directory levels below a project `.agents/skills` folder a skill directory may sit. It is an integer from `1` to `16`, default `4`; `1` means direct children only. Any other value SHALL fail configuration loading with an error naming the key. Every other source SHALL accept direct children only.

#### Scenario: Skill four levels deep is offered by default

- **WHEN** no `nested_depth` is configured and `<workspace>/.agents/skills/a/b/c/review/SKILL.md` is valid
- **THEN** the skill tool listing includes `review`

#### Scenario: Skill five levels deep is not offered by default

- **WHEN** no `nested_depth` is configured and the only skill is `<workspace>/.agents/skills/a/b/c/d/review/SKILL.md`
- **THEN** the skill tool listing does not include `review`

#### Scenario: Nested depth of one offers direct children only

- **WHEN** `[agent.skills]` sets `nested_depth = 1` and the workspace has `.agents/skills/top/SKILL.md` and `.agents/skills/group/inner/SKILL.md`
- **THEN** the skill tool listing includes `top` and does not include `inner`

#### Scenario: Out-of-range nested depth fails configuration loading

- **WHEN** a configuration file sets `nested_depth` to `0`, to `17` or to a non-integer value
- **THEN** loading the configuration fails with an error that names `nested_depth`

#### Scenario: Nested depth does not apply to other sources

- **WHEN** `nested_depth` is `4` and the only skill is `<workspace>/.claude/skills/group/inner/SKILL.md`
- **THEN** the skill tool listing does not include `inner`

### Requirement: Skill discovery problems are recorded

Every skill refresh SHALL record the problems it found for the workspace, replacing the earlier record. Each problem SHALL carry the affected path, the source folder it belongs to, a kind and a readable message. The kinds are `source_unreadable`, `entry_unreadable`, `broken_link`, `link_loop`, `duplicate_definition`, `invalid_definition` and `duplicate_id`. Problems SHALL be ordered by path.

#### Scenario: Malformed definition is recorded as invalid

- **WHEN** a skill's definition file has frontmatter that cannot be parsed
- **THEN** a problem of kind `invalid_definition` is recorded with that file's path

#### Scenario: Both definition spellings in one folder are recorded

- **WHEN** one `.agents/skills` entry directory contains both `skill.md` and `SKILL.md` as distinct files
- **THEN** a problem of kind `duplicate_definition` is recorded for that directory
- **AND** neither definition is offered

#### Scenario: Equal-precedence duplicate ID is recorded and the first is kept

- **WHEN** `<workspace>/.claude/skills/review` and `<workspace>/.agents/skills/review` both define callable ID `review`
- **THEN** the skill from the source that is searched first is offered
- **AND** a problem of kind `duplicate_id` is recorded for the other one

#### Scenario: Higher-precedence override is not a problem

- **WHEN** a global source and a project source both define callable ID `review`
- **THEN** the project skill is offered and no problem is recorded
- **AND** this holds whichever of the two sources is searched first

#### Scenario: Dangling source folder link is reported

- **WHEN** `<workspace>/.agents/skills` is a symbolic link to a directory that no longer exists
- **THEN** a problem of kind `broken_link` is recorded for it and no skill is offered from it
- **AND** the scan is not marked as having an unreadable source

#### Scenario: Definition that is not a regular file is a problem

- **WHEN** a skill folder's `SKILL.md` is a link to a pipe or a device, or is larger than 1 MiB
- **THEN** a problem of kind `invalid_definition` is recorded for it without reading it to the end
- **AND** the other skills of the source are still offered

#### Scenario: Definition directly in the source folder is not a skill

- **WHEN** a source folder contains `SKILL.md` next to its skill directories
- **THEN** that file is not offered as a skill and no problem is recorded for it

#### Scenario: Disabled skill is not a problem

- **WHEN** a project `.agents/skills` skill declares `enabled: false`
- **THEN** no problem is recorded for it

#### Scenario: Fixed problem disappears on the next refresh

- **WHEN** a malformed skill definition is corrected and the skill tool schema is generated again
- **THEN** the recorded problems no longer include that file

#### Scenario: Unreadable source is recorded while the earlier skill set is kept

- **WHEN** a refresh fails because a source folder became unreadable
- **THEN** a problem of kind `source_unreadable` is recorded for that folder
- **AND** the previously published skills remain listed

### Requirement: Skill discovery problems are announced to the client

WHEN the model-facing skill tool schema is generated for a session and the workspace's recorded problems differ from those last announced to that session, THEN the agent SHALL send an ACP `notice` update whose `_meta.querymt.skillProblems` holds the full problem list. A non-empty list SHALL use severity `warning`; a list that became empty SHALL use severity `info`. The notice SHALL be sent only to clients that advertise notice support and SHALL NOT be stored in session history.

#### Scenario: Problems are announced on the first model request

- **WHEN** a session's first model request is prepared for a workspace that has a broken skill link and the client advertises notice support
- **THEN** the client receives one `notice` with severity `warning`
- **AND** its `_meta.querymt.skillProblems` contains one item of kind `broken_link`

#### Scenario: Unchanged problems are not announced again

- **WHEN** a later model request of the same session is prepared and the recorded problems are unchanged
- **THEN** no further skill problem notice is sent

#### Scenario: Changed problems are announced again

- **WHEN** a second skill problem appears between two model requests of one session
- **THEN** the client receives a new `notice` whose `_meta.querymt.skillProblems` contains both problems

#### Scenario: Resolved problems are announced with an empty list

- **WHEN** every announced problem has been fixed before the next model request of the session
- **THEN** the client receives one `notice` with severity `info`
- **AND** its `_meta.querymt.skillProblems` is an empty array

#### Scenario: Session without problems receives no skill notice

- **WHEN** a session's model requests are prepared for a workspace with no skill problems
- **THEN** no skill problem notice is sent

#### Scenario: Client without notice support receives no skill notice

- **WHEN** skill problems exist and the client does not advertise notice support
- **THEN** no skill problem notice is sent to that client

#### Scenario: Skill problem notice is not replayed on session load

- **WHEN** a session that announced skill problems is loaded again
- **THEN** the replayed history contains no skill problem notice

### Requirement: Skill problem notices are readable without metadata

A skill problem notice SHALL have a title that states the number of problems, and a description that lists at most 10 problems, one per line as `<path>: <message>`, followed by a line that states how many more there are. Each item of `_meta.querymt.skillProblems` SHALL be an object with the string fields `path`, `source`, `kind` and `message`, and the array SHALL always hold every problem.

#### Scenario: Notice text names each problem path

- **WHEN** a notice announces two skill problems
- **THEN** its title contains the number `2`
- **AND** its description contains both affected paths

#### Scenario: Long problem list is shortened in the description only

- **WHEN** a notice announces 12 skill problems
- **THEN** its description lists 10 problems and a line stating that 2 more exist
- **AND** its `_meta.querymt.skillProblems` contains all 12 items
