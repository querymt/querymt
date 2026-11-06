# Spec Delta

## Purpose

Decides which tool calls need the user's approval before they run, and how the approval prompt behaves, uniformly for built-in, MCP and provider tools.

## ADDED Requirements

### Requirement: Approval lists are configured per agent
An agent configuration SHALL accept a `permissions` section with `ask` and `always_ask` tool selector lists and an optional positive `timeout_sec`. The same section SHALL be accepted for a single agent, a quorum planner and each quorum delegate. Tools matched by neither list SHALL run without an approval prompt unless a hook asks.

#### Scenario: Listed tool prompts
- **WHEN** `permissions.ask = ["shell"]` and the model calls `shell`
- **THEN** the user receives a permission request before the tool runs

#### Scenario: Unlisted tool runs
- **WHEN** `permissions.ask = ["shell"]` and the model calls `read_tool`
- **THEN** `read_tool` runs without a permission request

#### Scenario: Delegate permissions
- **WHEN** a quorum delegate sets `permissions.always_ask = ["ibkr.place_order"]` and the delegate calls that tool
- **THEN** the delegate's session prompts for that call

### Requirement: One approval path for every tool source
Approval SHALL be decided before execution for built-in, MCP and provider tools alike, using the same permission interaction, events and outcome. A tool call that is denied MUST NOT execute.

#### Scenario: MCP tool approval
- **WHEN** `permissions.ask = ["github.create_issue"]` and the model calls `create_issue` served by MCP server `github`
- **THEN** the user receives a standard permission request and the MCP server is not called until it is granted

#### Scenario: Built-in tool rejected
- **WHEN** the user rejects a permission request for a built-in tool
- **THEN** the tool does not run and the model receives an error result stating the call was rejected and not run

### Requirement: Session-wide allow
For a tool matched only by `ask`, the permission request SHALL offer "Allow once", "Allow for this session" and "Reject". Choosing "Allow for this session" SHALL let later calls of the same server-qualified tool in the same session run without a prompt. The choice MUST NOT outlive the session or apply to other sessions.

#### Scenario: Allow for this session
- **WHEN** the user chooses "Allow for this session" for `shell` and the model calls `shell` again in the same session
- **THEN** the second call runs without a permission request

#### Scenario: Other session still asks
- **WHEN** a different session calls `shell`
- **THEN** that session receives its own permission request

#### Scenario: Same tool name on another server
- **WHEN** the user allowed `alpha.lookup` for the session and the model calls `lookup` served by MCP server `beta` which is also in `ask`
- **THEN** the call to `beta.lookup` prompts

### Requirement: Always-ask tools prompt on every call
A tool matched by `always_ask` SHALL prompt on every call, offering only "Allow once" and "Reject", even when it is also matched by `ask` and even when it was previously allowed for the session.

#### Scenario: Repeated order placement
- **WHEN** `permissions.always_ask = ["ibkr.place_order"]` and the model places two orders
- **THEN** the user receives two permission requests, neither offering a session option

### Requirement: Hook ask prompts with the hook's reason
A pre-tool hook returning an `ask` decision SHALL cause a per-call permission request whose reason is the hook's reason when present, regardless of the approval lists. It MUST NOT offer the session option and MUST NOT be satisfied by an earlier session allow.

#### Scenario: Hook ask on an unlisted tool
- **WHEN** no approval list matches `edit` and a pre-tool hook returns `ask` with reason `EDIT a.rs`
- **THEN** the user receives a permission request showing `EDIT a.rs` with "Allow once" and "Reject" only

#### Scenario: Hook ask after session allow
- **WHEN** `shell` was allowed for the session and a hook returns `ask` for the next `shell` call
- **THEN** the user is prompted again

### Requirement: Hook allow skips the prompt
A pre-tool hook returning an `allow` decision SHALL let the call run without a permission request, even when an approval list matches the tool.

#### Scenario: Hook allow on a listed tool
- **WHEN** `permissions.ask = ["shell"]` and a pre-tool hook returns `allow` for a `shell` call
- **THEN** the call runs without a permission request

### Requirement: Approval timeout rejects unanswered prompts
When `permissions.timeout_sec` is set, any permission request (from a list or a hook) left unanswered for that long SHALL be rejected, retired on every client and reported to the model as timed out. Without it, the request SHALL wait for an answer or cancellation. The timeout MUST apply even when both approval lists are empty.

#### Scenario: Timeout with a hook ask
- **WHEN** `permissions.timeout_sec = 120`, both lists are empty, a hook asks and nobody answers for 120 seconds
- **THEN** the tool does not run, the request is no longer actionable and the model receives an error stating no answer arrived within the timeout

#### Scenario: No timeout configured
- **WHEN** `timeout_sec` is unset and nobody answers
- **THEN** the call keeps waiting until it is answered or the run is cancelled

### Requirement: Denials are explained to the model
A denied call SHALL produce an error tool result naming the reason (rejected by the user, timed out, run cancelled, task not active, or denied by a hook) and stating the tool was not run, and SHALL be recorded as a permission denial in telemetry and permission events.

#### Scenario: Denial reason in the result
- **WHEN** a `permission_request` hook denies a call with message `outside trading hours`
- **THEN** the tool result is an error containing `outside trading hours` and stating the tool was not run

### Requirement: Inactive task blocks approval
A call that needs approval while the session's active task is not active SHALL be denied without prompting.

#### Scenario: Paused task
- **WHEN** the active task is paused and the model calls a tool matched by `ask`
- **THEN** no permission request is shown and the call is denied as task not active
