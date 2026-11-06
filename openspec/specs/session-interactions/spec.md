# session-interactions Specification

## Purpose

Defines one location-transparent lifecycle for agent questions that need a user answer, so tool permission and form elicitation behave identically whether a session runs on the local agent or on a remote mesh node.

## Requirements

### Requirement: Uniform interaction lifecycle across session locations
The system SHALL request, deliver, and resolve every user interaction (tool permission and form elicitation) through the same observable cycle regardless of whether the session runs locally or on a remote mesh node. A client MUST NOT need location-specific handling to receive or answer an interaction.

#### Scenario: Remote form question
- **WHEN** a tool in a session hosted on a remote mesh node asks a form question while a client is attached through its local agent
- **THEN** the client receives a standard ACP `elicitation/create` request and a valid answer resumes the remote tool exactly once

#### Scenario: Remote permission request
- **WHEN** a permission-required tool call occurs in a session hosted on a remote mesh node
- **THEN** the attached client receives a standard ACP `session/request_permission` request and the chosen option decides the remote tool call

#### Scenario: Local behavior unchanged
- **WHEN** the same interactions occur in a locally hosted session
- **THEN** the client observes the same requests, responses, and outcomes as for the remote session

### Requirement: Remote sessions accept prompts from any ACP transport
The system SHALL accept `session/prompt` for a session hosted on a remote mesh node from any ACP transport that can prompt a local session. Prompting MUST NOT fail because the transport has a per-connection callback channel.

#### Scenario: Prompt a newly created remote session
- **WHEN** a client creates a session targeting a remote mesh node and sends its first prompt over WebSocket
- **THEN** the remote session runs the prompt and streams updates, and the error `client bridges are not supported for remote session prompts` no longer occurs

#### Scenario: Prompt an idle attached remote session
- **WHEN** a client sends a prompt to an idle remote session it has attached to
- **THEN** the prompt starts a run on the remote node and the response reports its stop reason

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

### Requirement: Transport failure is not a permission decision
The system SHALL treat permission delivery failure, disconnect, malformed response, and response timeout as undelivered, keeping the request pending. Only an explicit selected option from the client MAY grant or reject the tool call.

#### Scenario: Disconnect before answering
- **WHEN** the connection that received a permission request closes before answering
- **THEN** the request stays pending and is re-delivered to the next eligible client

#### Scenario: Malformed answer
- **WHEN** a client answers a permission request with an unknown option or invalid payload
- **THEN** the tool call is neither granted nor rejected and the request stays pending

### Requirement: Single-winner interaction resolution
The system SHALL accept at most one terminal answer per interaction. Answers to an already resolved, retired, or superseded delivery MUST NOT change the outcome.

#### Scenario: Two windows answer
- **WHEN** two attached clients answer the same permission request
- **THEN** the first valid answer decides the tool call and the second receives a not-pending or stale result

#### Scenario: Late answer after cancel
- **WHEN** a client answers an interaction after its run was cancelled
- **THEN** the answer is rejected as not pending and no tool executes

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

### Requirement: Recovery covers sessions hosted on remote mesh nodes
The system SHALL apply long-lived waiting, authorized discovery, re-delivery, single-winner resolution, and lifecycle retirement to form questions from sessions hosted on remote mesh nodes exactly as for local sessions. A remote question MUST be discoverable and answerable through the local agent the client is connected to.

#### Scenario: Remote question while a client is attached
- **WHEN** a tool in a remote-hosted session asks a form question and a client is attached to that session through its local agent
- **THEN** the client receives a standard ACP `elicitation/create` request carrying the stable question identity, and a valid answer resumes the remote tool exactly once

#### Scenario: Reconnect to a pending remote question
- **WHEN** the authorized client reconnects while a form question in a remote-hosted session is still pending
- **THEN** it can discover the session, receives a fresh `elicitation/create` request for the same question, and no `cancel` is synthesized for the earlier delivery

#### Scenario: Remote run cancelled while waiting
- **WHEN** the user stops a remote-hosted run that is waiting on a form question
- **THEN** the question ceases to be actionable on every attached client and a later reconnection cannot revive it

#### Scenario: Invalid answer to a remote question
- **WHEN** an authorized client submits accepted form content that fails the remote question's schema
- **THEN** the client receives a validation failure, the remote tool keeps waiting, and the question is re-delivered for correction

### Requirement: Editor workspace queries unavailable for remote sessions
The system SHALL report editor workspace queries as unavailable for a session hosted on a remote mesh node, returning a tool result that states the reason instead of an error or hang.

#### Scenario: Language query in a remote session
- **WHEN** the model calls the editor workspace query tool in a session hosted on a remote mesh node
- **THEN** the tool returns an "unavailable for remote sessions" result and the run continues

### Requirement: Mesh nodes run a matching interaction contract
All nodes participating in a mesh session SHALL run an agent version that supports this interaction contract. When a node lacks it, the system MUST fail the affected operation with an error that states an upgrade is required instead of falling back to bridge-based or auto-granted behavior.

#### Scenario: Outdated remote node
- **WHEN** the local agent attaches to or prompts a session on a remote node running an incompatible agent version
- **THEN** the operation fails with an error stating that the remote node must be upgraded, and no tool runs without the user being able to answer its interactions
