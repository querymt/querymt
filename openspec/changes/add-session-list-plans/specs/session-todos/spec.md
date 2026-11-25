# Spec Delta

## MODIFIED Requirements

### Requirement: Deleting a session discards its todo list
When a session is deleted, QueryMT SHALL discard that session's todo list, including the persisted todo plan that `session/list` reports. Todo lists SHALL NOT be retained for sessions that no longer exist.

#### Scenario: Recreated session id starts with an empty list
- **WHEN** a session writes todos, is deleted, and a session with the same id later calls `todoread`
- **THEN** the result has `todos: []` and `total: 0`

#### Scenario: Deleted session's plan is not listed again
- **WHEN** a session writes todos, is deleted, and a session with the same id is later created without writing todos
- **THEN** its `session/list` entry has no `querymt/plans` key
