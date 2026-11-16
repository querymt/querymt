# Agent Skills Specification

## Purpose

Define how the agent discovers, lists, and loads skills from global and project skill directories, and guarantee that skill availability tracks the filesystem at turn boundaries without requiring an agent restart.

## Requirements

### Requirement: Skill discovery at session start

The agent SHALL discover skills from all configured global and project skill sources when a session starts and SHALL advertise every discovered skill in the skill tool listing (callable ID + description). A protocol skill's callable ID SHALL be its explicit stable `id`, or the existing protocol fallback when `id` is absent; its human-readable `name` SHALL remain display metadata. Project `.agents/skills` sources SHALL be discovered recursively and accept the protocol lowercase `skill.md` spelling; skills whose protocol metadata disables them (`enabled: false`) SHALL be excluded.

#### Scenario: Skills from multiple sources are listed

- **WHEN** the agent session starts with skills present in both a project source (e.g. `.agents/skills`) and a global source
- **THEN** the skill tool listing includes every discovered skill from both sources

#### Scenario: Protocol skill uses its stable ID

- **WHEN** a protocol skill declares an `id` that differs from its human-readable `name`
- **THEN** the skill tool advertises and loads the skill by that stable ID
- **AND** the human-readable name remains available as display metadata

#### Scenario: Disabled protocol skill is excluded

- **WHEN** a skill definition in a project `.agents/skills` source declares `enabled: false`
- **THEN** that skill does not appear in the skill tool listing and cannot be loaded

### Requirement: Added skills appear in the next model-facing schema

The agent SHALL refresh skill discovery before generating each model-facing skill tool schema. WHEN a skill is added to any configured source after the session has started, THEN the skill tool schema generated for the next model request SHALL include it, and loading it by callable ID SHALL succeed without restarting the agent. Refreshes MAY also occur when a schema is requested for a non-turn context.

#### Scenario: Skill added mid-session becomes available

- **WHEN** a valid skill directory is added to a project skill source while a session is in progress
- **THEN** the skill tool schema generated for the next model request includes the new skill, and loading it by callable ID returns its content

### Requirement: Removed skills stop being offered in the next model-facing schema

A successful refresh SHALL use replace semantics: the advertised skill set after the refresh MUST equal the valid, enabled set currently present in the configured sources. WHEN a skill's source directory (or its definition file) is deleted after startup, THEN the skill SHALL no longer appear in the schema generated for the next model request, and invoking it SHALL fail with an error that identifies the callable ID as unavailable and lists the currently available callable IDs, or explicitly states that none are available.

#### Scenario: Deleted skill is no longer listed

- **WHEN** a skill directory is removed from a skill source while a session is in progress
- **THEN** the skill tool schema generated for the next model request does not include the deleted skill

#### Scenario: Stale invocation fails with available names

- **WHEN** the model attempts to load a skill whose source was removed
- **THEN** the tool call fails with an error naming the missing callable ID and enumerating the currently available callable IDs, or stating that none are available

### Requirement: Mid-turn invocation of a newly added skill succeeds

Loading a skill SHALL NOT rely solely on the advertised listing. WHEN the model invokes a skill by callable ID that is present on disk but not yet in the advertised listing, THEN the agent SHALL re-check the skill sources once during that invocation and load the skill if found.

#### Scenario: Invocation of unadvertised but present skill

- **WHEN** a skill is added to a source between two turns and the model invokes it before the listing refresh
- **THEN** the invocation succeeds and returns the skill content

### Requirement: Changed skill content is served after a schema refresh

WHEN a skill's definition file content changes after it has been discovered, THEN the skill tool schema generated for the next model request and subsequent loads of that skill SHALL use the updated content and metadata. Loading an already-registered skill before that schema refresh is not required to re-read its definition file.

#### Scenario: Edited skill description propagates

- **WHEN** a discovered skill's definition file is edited to change its description
- **AND** the skill tool schema is generated for the next model request
- **THEN** the tool listing and subsequent loaded content reflect the edited description

### Requirement: Failed refresh preserves the known skill set

A refresh SHALL publish its replacement skill set atomically. WHEN any configured source that is selected for discovery cannot be traversed completely, THEN the refresh SHALL fail, the agent SHALL retain the previously published skill set unchanged, and the skill tool SHALL remain usable. A missing source directory SHALL count as an empty source rather than a refresh failure. An invalid individual skill definition SHALL be diagnosed and excluded without invalidating otherwise successful source discovery.

#### Scenario: Unreadable source does not publish a partial snapshot

- **WHEN** a refresh encounters an unreadable skill source while other sources are intact
- **THEN** no partial replacement is published
- **AND** previously discovered skills remain listed and loadable

#### Scenario: Missing source is treated as empty

- **WHEN** a previously present configured source directory is deleted
- **THEN** refresh succeeds and removes skills that came only from that source

#### Scenario: Invalid skill does not block valid skills

- **WHEN** a traversable source contains both a malformed skill definition and valid skill definitions
- **THEN** refresh succeeds with the malformed skill excluded and the valid skills available

### Requirement: Permission configuration applies across refreshes

Permission rules configured for a skill SHALL continue to apply to that skill after any refresh. WHEN a skill is configured to be denied or to require approval and it is (re)discovered by a refresh, THEN the same permission behavior SHALL be enforced on invocation.

#### Scenario: Denied skill stays denied after refresh

- **WHEN** a skill is configured as denied and is re-discovered during a refresh
- **THEN** an attempt to load it is still rejected with a permission error

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

### Requirement: Skill approvals can be allowed for the session
A skill approval request SHALL offer the choices "Allow", "Allow for this session" and "Deny". After "Allow for this session", later loads of the same callable skill ID in the same session, and reads of that skill's supporting files, SHALL proceed without an approval request. This SHALL apply to MCP skills and to local skills whose permission is `ask`.

#### Scenario: Session allow skips later prompts for the same skill
- **WHEN** the user answers "Allow for this session" to the approval request for `docs.guide` and the model loads `docs.guide` again in the same session
- **THEN** the second load returns the skill without an approval request

#### Scenario: Session allow covers the skill's supporting files
- **WHEN** the user allowed `docs.guide` for the session and the model loads `docs.guide` with `file: "templates/checklist.md"`
- **THEN** the file is returned without an approval request

#### Scenario: Session allow applies to an ask-configured local skill
- **WHEN** permissions contain `"review" = "ask"`, the user answers "Allow for this session" for local skill `review`, and the model loads `review` again
- **THEN** the second load returns the skill without an approval request

### Requirement: Skill session grants are narrow
A skill session grant SHALL apply only to the exact callable ID it was given for. It MUST NOT apply to other skills of the same server or to other sessions, including delegate sessions, and MUST NOT outlive the session. "Allow" and "Deny" answers SHALL apply to one load only.

#### Scenario: Session allow does not cover other skills of the server
- **WHEN** the user allowed `docs.guide` for the session and the model loads `docs.release-notes`
- **THEN** the user receives an approval request for `docs.release-notes`

#### Scenario: Session allow does not reach another session
- **WHEN** the user allowed `docs.guide` for one session and another session of the same agent loads `docs.guide`
- **THEN** that session receives its own approval request

#### Scenario: Allow once is not remembered
- **WHEN** the user answers "Allow" to the approval request for `docs.guide` and the model loads `docs.guide` again
- **THEN** the user receives a new approval request

#### Scenario: Skill denial is not remembered
- **WHEN** the user answers "Deny" for `docs.guide` and the model loads `docs.guide` again
- **THEN** the user receives a new approval request

### Requirement: Configuration overrides skill session grants
A skill whose configured permission resolves to deny SHALL be rejected with a permission error even when the session holds a grant for it.

#### Scenario: Configured deny overrides a session allow
- **WHEN** the session holds a grant for `docs.guide` and the permissions in effect for the next load resolve `docs.guide` to deny
- **THEN** loading `docs.guide` is rejected with a permission error and the user is not asked

### Requirement: Invalid supporting file paths are rejected without prompting
A `file` argument that fails path normalization SHALL be rejected before any approval request, for local and MCP skills, with the same error as when the skill is allowed. A skill whose permission resolves to deny SHALL still be rejected with a permission error. A well-formed file that is not listed in an MCP skill's manifest SHALL still be rejected only after approval.

#### Scenario: Traversal path fails without an approval request
- **WHEN** no permission pattern names server `docs` and the model loads `docs.guide` with `file: "../secrets.env"`
- **THEN** the load fails with an invalid file path error, the user is not asked, and no request is sent to the server

#### Scenario: Denied skill with an invalid path reports the denial
- **WHEN** permissions contain `"docs.*" = "deny"` and the model loads `docs.guide` with `file: "../secrets.env"`
- **THEN** the load is rejected with a permission error

#### Scenario: Unlisted file still asks first
- **WHEN** no permission pattern names server `docs` and the model loads `docs.guide` with `file: "notlisted.md"`
- **THEN** the user is asked to approve, and after approval the load fails naming the skill and file without requesting the resource
