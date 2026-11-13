# session-todos Specification

## Purpose

Defines how the `todowrite` and `todoread` tools store and report a session's task list: per-session isolation, the allowed status and priority values, summary counts, and cleanup when a session is deleted.

## Requirements

### Requirement: Todo lists are isolated per session
`todowrite` SHALL replace the calling session's todo list. `todoread` SHALL return only the calling session's list, or an empty list when that session has written none.

#### Scenario: Two sessions keep separate todo lists
- **WHEN** session A writes two todos and session B writes one todo
- **THEN** `todoread` in session A returns A's two todos and `todoread` in session B returns B's one todo

#### Scenario: Reading before any write returns an empty list
- **WHEN** a session calls `todoread` before calling `todowrite`
- **THEN** the result has `todos: []` and `total: 0`

### Requirement: Todo status and priority are enumerated
Each todo's `status` SHALL be one of `pending`, `in_progress`, `completed`, `cancelled`, and `priority` SHALL be one of `high`, `medium`, `low`. A `todowrite` call with any other value SHALL fail with an invalid-request tool error and SHALL leave the stored list unchanged. `todoread` SHALL return these values as the same lowercase strings.

#### Scenario: Unknown status is rejected
- **WHEN** `todowrite` is called with a todo whose status is `done`
- **THEN** the tool returns an invalid-request error
- **AND** a following `todoread` returns the list stored before the call

#### Scenario: Stored values round-trip unchanged
- **WHEN** `todowrite` stores a todo with status `in_progress` and priority `high`
- **THEN** `todoread` returns that todo with `"status": "in_progress"` and `"priority": "high"`

### Requirement: Todo write reports counts by status
A successful `todowrite` SHALL return `success: true`, `total_todos`, and one count each for `pending`, `in_progress`, `completed` and `cancelled`. The four counts SHALL sum to `total_todos`.

#### Scenario: Counts match the written list
- **WHEN** `todowrite` stores one pending, two in-progress and one cancelled todo
- **THEN** the result reports `total_todos: 4`, `pending: 1`, `in_progress: 2`, `completed: 0`, `cancelled: 1`

### Requirement: Deleting a session discards its todo list
When a session is deleted, QueryMT SHALL discard that session's todo list. Todo lists SHALL NOT be retained for sessions that no longer exist.

#### Scenario: Recreated session id starts with an empty list
- **WHEN** a session writes todos, is deleted, and a session with the same id later calls `todoread`
- **THEN** the result has `todos: []` and `total: 0`
