## ADDED Requirements

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
A skill whose configured permission resolves to deny SHALL be rejected with a permission error even when the session holds a grant for it. An agent that cannot prompt SHALL keep failing at once with the configuration hint.

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
