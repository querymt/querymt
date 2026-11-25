# Spec Delta

## MODIFIED Requirements

### Requirement: Filesystem resolution is confined and safe
The system SHALL reject path traversal outside a selected protocol entry root, unsafe symlink escapes, non-regular files where regular files are required, and relative executable or file references that escape their owning layer. Workspace discovery SHALL only occur when the workspace `.agents` directory already exists. Skill folders are exempt from the symlink rule: a skill directory reached through a symbolic link SHALL be accepted wherever its target is.

#### Scenario: Entry uses path traversal
- **WHEN** a protocol reference resolves outside its allowed root
- **THEN** the reference is rejected before any file is read or process is started
- **AND** a diagnostic identifies the unsafe source without exposing unrelated filesystem content

#### Scenario: Symlink escapes the layer
- **WHEN** a discovered entry other than a skill folder resolves through a symlink outside the enabled protocol root
- **THEN** the entry is rejected unless the caller explicitly enabled that external root

#### Scenario: Linked skill folder outside the layer is accepted
- **WHEN** `<workspace>/.agents/skills/shared` is a symbolic link to a directory outside the workspace that contains a valid skill definition
- **THEN** the resolved manifest lists that skill
- **AND** no path-escape diagnostic is reported for it

## ADDED Requirements

### Requirement: Resolved manifest lists every offered local skill
The resolved manifest's skill list SHALL contain exactly the local skills that the skill tool offers for the resolved workspace, from every skill source and not only `.agents/skills`. Each entry SHALL carry the callable ID, name, description, source folder, source kind (`global`, `project` or `configured`), skill directory and, for a skill reached through a link, the resolved target. Entries SHALL be ordered by callable ID. Skills published by MCP servers SHALL NOT be listed.

#### Scenario: Skill from a non-protocol folder is listed in the manifest
- **WHEN** a caller resolves a workspace whose only skill is `<workspace>/.claude/skills/review/SKILL.md`
- **THEN** the manifest's skill list contains `review` with source kind `project` and source folder `<workspace>/.claude/skills`

#### Scenario: Manifest and skill tool list the same local skills
- **WHEN** an agent is built for a workspace with skills in a global source, in `.agents/skills` and in a configured path
- **THEN** the callable IDs in the resolved manifest's skill list equal the local callable IDs in the skill tool listing

#### Scenario: Linked skill entry records its target
- **WHEN** a listed skill's directory is reached through a symbolic link
- **THEN** its manifest entry carries the link's resolved target directory

#### Scenario: Disabled skill is not listed in the manifest
- **WHEN** `<workspace>/.agents/skills/draft/skill.md` declares `enabled: false`
- **THEN** the manifest's skill list does not contain `draft`

#### Scenario: Agent with skills disabled resolves an empty skill list
- **WHEN** an agent whose skills table sets `enabled = false` previews its manifest for a workspace that contains skills
- **THEN** the manifest's skill list is empty

#### Scenario: Agent without the skill tool resolves an empty skill list
- **WHEN** an agent with skills enabled and a tool list that does not name `skill` previews its manifest for a workspace that contains skills
- **THEN** the manifest's skill list is empty

#### Scenario: Resolution without skill settings uses the defaults
- **WHEN** a caller resolves a workspace directly, without an agent configuration, and the workspace has `.agents/skills/review/SKILL.md`
- **THEN** the manifest's skill list contains `review`

### Requirement: Skill problems are manifest warnings
Every skill discovery problem found while resolving SHALL appear in the resolved manifest as a warning diagnostic that names the affected path. Skill problems SHALL NOT be error diagnostics and SHALL NOT fail a strict resolution.

#### Scenario: Malformed skill yields a manifest warning
- **WHEN** a caller resolves a workspace containing one malformed skill definition and one valid skill
- **THEN** the manifest lists the valid skill
- **AND** it contains a warning diagnostic naming the malformed definition's path

#### Scenario: Strict resolution succeeds despite a skill problem
- **WHEN** a caller resolves with strictness `strict` and the only diagnostic is a broken skill link
- **THEN** resolution succeeds and returns the manifest with that warning
