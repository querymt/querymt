# Spec Delta

## MODIFIED Requirements

### Requirement: Unanswered permission waits for the user
A permission-required tool call that is not decided by a session allow, hook decision or configured approval timeout SHALL wait for an explicit user answer. The system MUST NOT grant permission because no client is attached, delivery failed, or the client disconnected. When the agent configures an approval timeout, an unanswered request SHALL be rejected once the timeout elapses and retired on every client.

#### Scenario: No client attached
- **WHEN** a permission-required tool call occurs while no client is attached to the session and no approval timeout is configured
- **THEN** the tool call stays pending and neither executes nor is reported as granted

#### Scenario: Client attaches later
- **WHEN** a client authorized for the session attaches while a permission request is pending
- **THEN** the client receives the pending permission request and its answer decides the waiting tool call

#### Scenario: Run cancelled while waiting
- **WHEN** the user cancels a run that is waiting on a permission request
- **THEN** the tool call does not execute and the permission request is no longer actionable

#### Scenario: Pre-decided by policy
- **WHEN** a permission-required tool call is allowed by an earlier "Allow for this session" answer, or allowed or denied by a hook decision
- **THEN** the call proceeds or is rejected without prompting the user

#### Scenario: Approval timeout elapses
- **WHEN** the agent configures `permissions.timeout_sec` and a permission request stays unanswered for that long, with or without a client attached
- **THEN** the tool call is rejected without executing, the request is no longer actionable, and a later answer is rejected as not pending

### Requirement: Interaction remembers permanent choices
An "Allow for this session" answer SHALL apply to later calls of the same server-qualified tool in that session without prompting, for local and remote sessions alike. The choice MUST NOT apply to tools configured to ask on every call, to calls a hook asks about, or to other sessions. No rejection SHALL be remembered; every rejection applies to one call only.

#### Scenario: Always allow on a remote session
- **WHEN** the user answers "Allow for this session" to a permission request from a remote session
- **THEN** later calls of that tool in the same session run without a new permission request

#### Scenario: Rejection is not remembered
- **WHEN** the user rejects a permission request and the model calls the same tool again
- **THEN** the user receives a new permission request

#### Scenario: Session allow ends with the session
- **WHEN** the user allowed a tool for one session and a new session of the same agent calls that tool
- **THEN** the new session prompts
