# Spec Delta

## ADDED Requirements

### Requirement: Session lifecycle changes are announced
When a session's resolved or archived state changes through this node, by a lifecycle update request or by automatic resolution of a finished delegation, QueryMT SHALL send the `querymt/session/lifecycleChanged` notification to every connection. It SHALL carry `session_id` and the complete current state (`resolved`, `archived`, `resolved_at`, `archived_at`). A request that changes neither state, or that fails, SHALL send no notification.

#### Scenario: Session is archived
- **WHEN** a client sets `archived = true` on an open session
- **THEN** every connection receives `querymt/session/lifecycleChanged` with that `session_id`, `archived = true` and an `archived_at` timestamp

#### Scenario: Lifecycle request without an effective change
- **WHEN** a client sets `resolved = true` on a session that is already resolved
- **THEN** no `querymt/session/lifecycleChanged` notification is sent

#### Scenario: Delegation resolves its session
- **WHEN** a one-shot delegation reaches a terminal state and its delegate session is resolved automatically
- **THEN** every connection receives `querymt/session/lifecycleChanged` for the delegate session with `resolved = true`

#### Scenario: Remote-owned session updated through this node
- **WHEN** a client of this node archives a session owned by another node and the owner confirms the update
- **THEN** this node's connections receive `querymt/session/lifecycleChanged` with the state the owner returned

#### Scenario: Lifecycle update fails
- **WHEN** a lifecycle update request fails
- **THEN** no `querymt/session/lifecycleChanged` notification is sent

#### Scenario: Change made on another node
- **WHEN** a session owned by another node is archived by a client of that node
- **THEN** this node sends no notification for it, and a lifecycle read through this node returns the owner's current state

### Requirement: Changes to the set of sessions are announced
When a session that session listings can show is created, forked, deleted or renamed on this node, QueryMT SHALL send the `querymt/sessions/changed` notification to every connection, carrying `session_id` and `reason` (`created`, `deleted` or `renamed`). A fork SHALL be announced as `created` for the new session. Sessions that listings never show, such as delegate sessions, SHALL NOT be announced. Activity-time changes alone SHALL NOT be announced.

#### Scenario: New session
- **WHEN** a client creates a session
- **THEN** every connection receives `querymt/sessions/changed` with the new `session_id` and `reason = "created"`

#### Scenario: Forked session
- **WHEN** a client forks a session
- **THEN** every connection receives `querymt/sessions/changed` with the fork's `session_id` and `reason = "created"`

#### Scenario: Deleted session
- **WHEN** a client deletes a session
- **THEN** every connection receives `querymt/sessions/changed` with that `session_id` and `reason = "deleted"`

#### Scenario: Title assigned
- **WHEN** QueryMT persists a new title for a session
- **THEN** every connection receives `querymt/sessions/changed` with that `session_id` and `reason = "renamed"`

#### Scenario: Delegate session is not announced
- **WHEN** a delegation creates a delegate session
- **THEN** no `querymt/sessions/changed` notification is sent for it

#### Scenario: Activity alone is not announced
- **WHEN** a session stores new messages and its activity time advances
- **THEN** no `querymt/sessions/changed` notification is sent

### Requirement: Change notifications are advertised
The capabilities QueryMT advertises to a client SHALL list `querymt/sessions/changed` among its notifications, and SHALL list `querymt/session/lifecycleChanged` exactly when the lifecycle requests are advertised.

#### Scenario: Server with lifecycle support
- **WHEN** a client reads capabilities from a server that supports session lifecycle
- **THEN** the notifications list contains `querymt/sessions/changed` and `querymt/session/lifecycleChanged`

#### Scenario: Server without lifecycle support
- **WHEN** a client reads capabilities from a server whose storage does not support session lifecycle
- **THEN** the notifications list contains `querymt/sessions/changed` and not `querymt/session/lifecycleChanged`

### Requirement: Lifecycle requests fail promptly when the owner is not connected
A lifecycle read or update for a session owned by another node SHALL fail within 1 second with an owner-unavailable error when that node is not connected to this node. It SHALL NOT wait for the remote request timeout. When the owner node is connected, the remote request timeout SHALL apply as before.

#### Scenario: Owner node is offline
- **WHEN** a client reads the lifecycle of a session whose owner node is not connected
- **THEN** the request fails within 1 second with an error stating that the owner is unavailable

#### Scenario: Owner node is connected
- **WHEN** a client reads the lifecycle of a session whose owner node is connected and answers
- **THEN** the response carries the owner's lifecycle state

#### Scenario: Update while the owner is offline
- **WHEN** a client archives a session whose owner node is not connected
- **THEN** the request fails within 1 second, no state is changed and no notification is sent
