# Spec Delta

## MODIFIED Requirements

### Requirement: Session metadata synchronization
QueryMT SHALL emit `session_info_update` when mutable, persisted session metadata changes. The update SHALL use ACP patch semantics, SHALL include the changed title or activity timestamp fields, and subsequent session-list results SHALL reflect the same persisted values.

#### Scenario: Session title is assigned or changed
- **WHEN** QueryMT persists a new title for an active session
- **THEN** it emits `session_info_update` with that title
- **AND** a later session-list response reports the same title

#### Scenario: Session activity timestamp changes
- **WHEN** QueryMT persists a new last-activity time for an active session
- **THEN** it emits `session_info_update` with an RFC 3339 `updatedAt` value representing that time

#### Scenario: Metadata is cleared
- **WHEN** QueryMT intentionally clears a mutable nullable metadata field
- **THEN** it emits that field as `null` rather than omitting it

#### Scenario: Todo plan changes on an active session
- **WHEN** QueryMT persists a new non-empty todo plan for a session active on a connection
- **THEN** it emits `session_info_update` on that connection whose `_meta` sets only the top-level key `querymt/plans`, to the value a later session-list response reports
- **AND** it does not send title, `updatedAt` or other `_meta` keys in that update

#### Scenario: Todo plan is emptied on an active session
- **WHEN** a session active on a connection empties its todo plan
- **THEN** QueryMT emits `session_info_update` on that connection with `_meta: {"querymt/plans": null}`
