# Spec Delta

## ADDED Requirements

### Requirement: Skill availability is scoped to the session
The skill tool schema and skill loading SHALL be determined by the session that uses them: local skills SHALL be discovered for that session's workspace, and MCP skills SHALL come only from MCP servers connected to that session. Two sessions of the same agent with different workspaces or different MCP servers SHALL be offered different skill sets. Loading a local skill SHALL use the session's current workspace at the time of the call.

#### Scenario: Sessions with different MCP servers
- **WHEN** one session of an agent is connected to an MCP server publishing skill `triage` and another session of the same agent is not
- **THEN** only the first session's skill tool schema lists `<server>.triage`
- **AND** loading `<server>.triage` from the second session fails as unavailable

#### Scenario: Sessions with different workspaces
- **WHEN** two sessions of the same agent use different workspaces that contain different project skills
- **THEN** each session's skill tool schema lists only the project skills of its own workspace

#### Scenario: Workspace changes during a turn
- **WHEN** a session's workspace changes during a turn and the model then loads a project skill that exists only in the new workspace
- **THEN** the skill is loaded from the new workspace

### Requirement: MCP skills are offered through the skill tool
The agent SHALL list every valid skill published by a session's eligible MCP servers in the same skill tool as local skills. An MCP skill's callable ID SHALL be `<server>.<skill name>`, using the frontmatter name from the server listing, and its listing entry SHALL include the description and identify the server. No additional skill tool SHALL be exposed.

#### Scenario: Server skill is listed
- **WHEN** MCP server `docs` publishes a valid skill named `release-notes`
- **THEN** the session's skill tool schema lists callable ID `docs.release-notes` with its description
- **AND** local skills remain listed in the same tool

#### Scenario: Invalid server skill is excluded
- **WHEN** a server lists a skill entry that breaks the extension's structural rules, such as a URI that does not end in `/SKILL.md`
- **THEN** that entry is excluded with a diagnostic naming the server
- **AND** the server's valid skills remain listed

### Requirement: MCP skill callable IDs are unambiguous
A server whose name contains `.` SHALL contribute no skills, and a skill whose name contains `.` SHALL be excluded, each with a diagnostic, so that every MCP callable ID splits into server and skill at its first `.`.

#### Scenario: Server name containing a dot
- **WHEN** a connected server named `team.docs` publishes skills
- **THEN** none of its skills are listed or loadable
- **AND** a diagnostic names the server
- **AND** its MCP tools remain available as before

#### Scenario: Skill name containing a dot
- **WHEN** server `docs` lists a skill whose frontmatter name contains `.`
- **THEN** that skill is excluded with a diagnostic naming the server and the skill URI
- **AND** the server's other valid skills remain listed

### Requirement: MCP skill listing text is bounded
A server-supplied skill description SHALL reach the model only after control characters are removed, line breaks and other whitespace runs are collapsed to single spaces, and the result is limited to 1024 characters.

#### Scenario: Oversized multi-line description
- **WHEN** a server lists a skill whose description is longer than 1024 characters and contains line breaks
- **THEN** the skill tool schema shows a single-line description of at most 1024 characters for that skill

### Requirement: Local skills win callable ID collisions with MCP skills
WHEN a local skill's callable ID equals an MCP skill's server-qualified ID, THEN the local skill SHALL be listed and loaded under that ID, the MCP skill SHALL NOT be offered, and a diagnostic SHALL identify both sources.

#### Scenario: Local skill shadows a server skill
- **WHEN** a local protocol skill declares `id: docs.release-notes` and server `docs` publishes skill `release-notes`
- **THEN** loading `docs.release-notes` returns the local skill's content
- **AND** a diagnostic names the local skill source and the server

### Requirement: MCP skill content is verified before use
Loading an MCP skill SHALL read the content whose URI equals the skill's `SKILL.md` URI. WHEN the entry lists file digests, THEN the manifest SHALL contain a `SKILL.md` entry and the bytes SHALL match its size and `sha256` digest. The frontmatter name SHALL match the entry. On any mismatch or read failure, the load SHALL fail with an error naming the skill, and no content SHALL be returned.

#### Scenario: Verified skill loads
- **WHEN** the model loads `docs.release-notes` and the served `SKILL.md` matches its manifest entry
- **THEN** the tool returns the skill body without frontmatter, its description, and the skill-relative paths of its supporting files, which are the values accepted by the `file` argument

#### Scenario: Tampered content is rejected
- **WHEN** the served `SKILL.md` bytes do not match the manifest digest or size
- **THEN** the load fails with an integrity error naming the skill and no skill content is returned

#### Scenario: Frontmatter name mismatch is rejected
- **WHEN** the served `SKILL.md` frontmatter name differs from the skill's listed name
- **THEN** the load fails and no skill content is returned

#### Scenario: Manifest without SKILL.md is rejected
- **WHEN** a skill's file manifest has no `SKILL.md` entry
- **THEN** the load fails with an integrity error naming the skill and no skill content is returned

#### Scenario: Dynamic skill loads without digests
- **WHEN** a server declares a skill's files as dynamic
- **THEN** the skill loads after the frontmatter name check and the size limit, without digest verification

### Requirement: MCP skill entries are re-checked at load time
Loading an MCP skill SHALL first fetch its current entry from the server, validate it against the extension's structural rules, and require its URI and name to equal the listed entry. Otherwise the load SHALL fail with an integrity error naming the skill.

#### Scenario: Changed entry is rejected
- **WHEN** the entry returned at load time has a different URI or name than the listed entry, or fails the extension's structural rules
- **THEN** the load fails with an integrity error naming the skill

### Requirement: Skill content size is limited
A skill's `SKILL.md` and each supporting file SHALL be limited to 256 KiB. WHEN an MCP manifest declares a larger size, THEN the content SHALL NOT be requested. Content that exceeds the limit when read SHALL be rejected.

#### Scenario: Oversized content is rejected
- **WHEN** a skill's declared or served `SKILL.md` size exceeds 256 KiB
- **THEN** the load fails naming the skill, and when the size is declared, the content is not requested

### Requirement: Skill content cannot break out of its wrapper
Skill output SHALL be wrapped so that skill-supplied text, including descriptions, bodies, file names and file contents, cannot close or open the wrapper element.

#### Scenario: Body contains a closing wrapper tag
- **WHEN** a loaded skill's body contains the text `</skill_content>`
- **THEN** the tool output contains exactly one unescaped closing wrapper tag, at the end of the output

### Requirement: Skill supporting files can be loaded
The skill tool SHALL accept an optional `file` argument, a path relative to the skill root. For a local skill, the canonical path SHALL stay inside the skill directory; symlink escapes SHALL be rejected. For an MCP skill with a file manifest, only listed files SHALL be readable, and each SHALL be verified against its entry.

#### Scenario: Local supporting file is read
- **WHEN** the model loads skill `review` with `file: "templates/checklist.md"` and that file exists in the skill directory
- **THEN** the tool returns that file's content

#### Scenario: Local path escape is rejected
- **WHEN** the model loads a local skill with `file: "../../secrets.env"` or a path whose symlink resolves outside the skill directory
- **THEN** the load fails with an error and no file content is read

#### Scenario: Unlisted MCP file is rejected
- **WHEN** the model requests a file that is not listed in an MCP skill's file manifest
- **THEN** the load fails with an error naming the skill and file, without requesting the resource

#### Scenario: MCP supporting file is verified
- **WHEN** the model requests a listed MCP skill file whose served bytes do not match its manifest entry
- **THEN** the load fails with an integrity error and no file content is returned

### Requirement: Supporting file paths are normalized before use
For every skill, the `file` argument SHALL be rejected before anything is read or requested when it is empty, absolute or scheme-qualified, contains a query, fragment, backslash or encoded separator, or contains a `.` or `..` segment, also when percent-encoded.

#### Scenario: Encoded traversal on a dynamic skill is rejected
- **WHEN** the model loads a dynamic MCP skill with `file: "%2e%2e/other/SKILL.md"`
- **THEN** the load fails with an error and no resource is requested

### Requirement: Skill permission patterns resolve deterministically
A skill's permission SHALL come from the exact pattern naming its callable ID, else from the matching prefix wildcard with the longest prefix. This SHALL apply to local and MCP skills. WHEN neither matches, THEN a local skill SHALL use the catch-all `"*"` pattern, else allow.

#### Scenario: Overlapping wildcards
- **WHEN** permissions contain `"team-*" = "allow"` and `"team-secret-*" = "deny"`
- **THEN** loading local skill `team-secret-notes` is rejected with a permission error every time
- **AND** loading local skill `team-notes` is allowed

### Requirement: MCP skills require approval unless explicitly configured
An MCP skill's permission SHALL come from the most specific configured pattern naming it: an exact `<server>.<skill>` entry, else the longest matching prefix wildcard such as `<server>.*`. WHEN no such pattern matches, THEN a catch-all `"*" = "deny"` SHALL deny it, and otherwise loading it SHALL require user approval. A catch-all `"*"` set to `allow` or `ask` SHALL NOT admit MCP skills without approval.

#### Scenario: Unconfigured server skill asks
- **WHEN** no permission pattern names server `docs` and the model loads `docs.release-notes`
- **THEN** the user is asked to approve loading it before any content is returned

#### Scenario: Catch-all allow does not admit server skills
- **WHEN** permissions contain only `"*" = "allow"` and the model loads `docs.release-notes`
- **THEN** the user is still asked to approve loading it

#### Scenario: Catch-all deny applies to server skills
- **WHEN** permissions contain only `"*" = "deny"` and the model loads `docs.release-notes`
- **THEN** the load is rejected with a permission error and the user is not asked

#### Scenario: Server wildcard allows or denies
- **WHEN** permissions contain `"docs.*" = "deny"`
- **THEN** loading any `docs` skill is rejected with a permission error and the user is not asked

#### Scenario: Supporting files share the skill's permission
- **WHEN** the model loads a supporting file of a denied skill
- **THEN** the load is rejected with a permission error

### Requirement: Skill approval requests are specific to one call
An approval request for an MCP skill SHALL name the server and the skill. Concurrent approval requests for the same skill SHALL be answered independently.

#### Scenario: Request names the server
- **WHEN** the user is asked to approve loading `docs.release-notes`
- **THEN** the request names server `docs` and skill `release-notes`

#### Scenario: Concurrent approvals for the same skill
- **WHEN** two tool calls in the same turn load `docs.release-notes` and both require approval
- **THEN** the user receives two separate approval requests, and each answer applies only to its own call

### Requirement: MCP skill visibility follows MCP tool access
A session SHALL offer a server's skills only when the skill tool is offered, its tool policy includes both built-in and MCP tools, the allowlist (when configured) contains `<server>.*`, and the denylist contains neither `<server>.*` nor `<server>`. Hidden MCP skills SHALL NOT be loadable.

#### Scenario: Built-in-only policy hides server skills
- **WHEN** a session's tool policy excludes MCP tools
- **THEN** its skill tool schema lists no MCP skills and loading one fails as unavailable

#### Scenario: Provider-only policy offers no skill tool
- **WHEN** a session's tool policy is provider-only
- **THEN** the skill tool is not offered and no skill can be loaded

#### Scenario: Allowlist without the server hides its skills
- **WHEN** a session's allowlist contains `skill` and `alpha.*` but no `docs.*` selector
- **THEN** `alpha` skills are listed and `docs` skills are not

#### Scenario: Denylisted server hides its skills
- **WHEN** a session's denylist contains `docs.*`
- **THEN** `docs` skills are not listed and loading one fails as unavailable

### Requirement: MCP skills can be disabled by configuration
The skills configuration SHALL accept an `mcp` setting that defaults to enabled. WHEN it is disabled, THEN no MCP skills SHALL be listed or loadable while local skills remain available. A configuration that omits the setting SHALL load with MCP skills enabled.

#### Scenario: MCP skills disabled
- **WHEN** the skills configuration sets `mcp = false` and a connected server publishes skills
- **THEN** the skill tool lists only local skills
- **AND** loading one of the server's skills fails as unavailable

#### Scenario: Setting omitted
- **WHEN** an existing skills configuration without `mcp` is loaded
- **THEN** it parses and MCP skills are enabled

### Requirement: MCP skill changes appear in the next model-facing schema
WHEN a connected server's skill list is refreshed after a change announcement, THEN the skill tool schema generated for the next model request SHALL reflect the refreshed set, and removed MCP skills SHALL fail as unavailable with the currently available callable IDs listed. WHEN the model loads a visible server's skill that the session has not listed, THEN the system SHALL list that server's skills once more before failing.

#### Scenario: Server adds a skill
- **WHEN** a server publishes a new skill and announces that its resource list changed
- **THEN** the next model request's skill tool schema lists the new server-qualified ID

#### Scenario: Server removes a skill
- **WHEN** a server stops publishing a skill and announces that its resource list changed
- **THEN** the next schema omits it and loading it fails, naming the missing ID and the available IDs

#### Scenario: Unannounced new skill is loaded
- **WHEN** a server publishes a new skill without announcing a change and the model loads its server-qualified ID
- **THEN** the server's skills are listed again, the skill loads, and the next schema lists it
