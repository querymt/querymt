# Spec Delta

## Purpose

Lets clients show each listed session's current plans, such as its todo progress and next steps, from `session/list` and lightweight notifications, without loading or resuming the session.

## ADDED Requirements

### Requirement: Listed sessions report their current plans
Each `session/list` entry for a local session with a non-empty todo plan SHALL carry the top-level `_meta` key `querymt/plans`: an array holding one ACP plan content object `{"type": "items", "planId": "querymt-todos", "entries": [...]}`. Sessions without a todo plan, or whose plan is empty, SHALL omit the key.

#### Scenario: Session with todos lists its plan
- **WHEN** a session's last `todowrite` stored one completed and two pending todos and a client calls `session/list`
- **THEN** that session's entry has `_meta["querymt/plans"]` with one element whose `type` is `items` and `planId` is `querymt-todos`
- **AND** its `entries` hold the three todos with `status` `completed`, `pending`, `pending`

#### Scenario: Session without todos omits the key
- **WHEN** a session never called `todowrite` and a client calls `session/list`
- **THEN** that session's entry has no `querymt/plans` key in `_meta`

#### Scenario: Emptied todo list omits the key
- **WHEN** a session's last `todowrite` stored an empty list
- **THEN** its `session/list` entry has no `querymt/plans` key

### Requirement: Listed plan entries match the session's plan updates
The `entries` of a listed todo plan SHALL equal the entries of the `plan_update` QueryMT would send for the same plan on a live update or a resume replay: same order, `content`, `priority` and `status`, with `cancelled` todos and todos with an unknown status left out.

#### Scenario: Cancelled todo is left out of the listed plan
- **WHEN** a session's last `todowrite` stored one in-progress and one cancelled todo
- **THEN** its listed plan has exactly one entry, with status `in_progress`

#### Scenario: List and replay agree
- **WHEN** a client lists a session and then resumes it with replay from the start
- **THEN** the entries of the replayed `querymt-todos` `plan_update` equal the entries listed for that session

### Requirement: Listed plans survive agent restarts
QueryMT SHALL persist each session's current todo plan, so `session/list` reports the same plan after the agent restarts. Sessions that wrote todos before this capability existed SHALL report the plan from their last `todowrite` once the agent has upgraded its session database.

#### Scenario: Plan listed after restart
- **WHEN** a session writes todos, the agent restarts, and a client calls `session/list`
- **THEN** that session's entry reports the same `querymt/plans` value as before the restart

#### Scenario: Pre-existing session is backfilled
- **WHEN** an agent upgrades a session database holding a session whose journal contains `todowrite` calls
- **THEN** `session/list` reports that session's plan from its last `todowrite` call

### Requirement: Peer sessions do not report plans
`session/list` entries for sessions listed from mesh peers SHALL NOT carry `querymt/plans`.

#### Scenario: Remote entry has no plans key
- **WHEN** a client lists sessions that include an entry with `_meta.location` `remote`
- **THEN** that entry has no `querymt/plans` key, whether or not the session has todos on its peer

### Requirement: Plan changes are announced to every connection
After a `todowrite` changes a session's stored plan, QueryMT SHALL broadcast `querymt/session/plansChanged` to every connection, with params `{"session_id": <id>, "plans": <array>}`, where `plans` is the value `session/list` would now report (`[]` when the key is omitted). The control capabilities' `notifications` list SHALL include `querymt/session/plansChanged`.

#### Scenario: Connection without the session active is notified
- **WHEN** session A calls `todowrite` while a second connection has only listed sessions and has not resumed A
- **THEN** the second connection receives `querymt/session/plansChanged` with A's `session_id` and its current `plans`

#### Scenario: Emptying the list is announced
- **WHEN** a session with a non-empty plan calls `todowrite` with an empty list
- **THEN** connections receive `querymt/session/plansChanged` for it with `plans: []`

#### Scenario: Capability is advertised
- **WHEN** a client reads the control capabilities
- **THEN** `notifications` contains `querymt/session/plansChanged`

### Requirement: Listed plans change with the todo plan updates
The listed plan SHALL change exactly when QueryMT reports a new todo plan through `plan_update` or `plan_removed`, from the same `todowrite` call, or from a rewind that changes the plan (see "A rewind announces the plan it restores"). A `todowrite` call whose arguments are not a todo list changes neither and SHALL NOT produce a `plansChanged` notification.

#### Scenario: Malformed todowrite arguments change nothing
- **WHEN** a session with a listed plan calls `todowrite` with arguments that have no `todos` array
- **THEN** no `querymt/session/plansChanged` notification is sent
- **AND** a following `session/list` reports the previous plan

#### Scenario: Unchanged plan is not announced again
- **WHEN** a session calls `todowrite` with exactly the entries it already lists
- **THEN** no `querymt/session/plansChanged` notification is sent for it

### Requirement: A rewind announces the plan it restores
When the next prompt after an undo removes the session's last `todowrite` from its history, the listed plan SHALL become the plan of the last remaining `todowrite` (none when no call remains). If that changes the plan, QueryMT SHALL announce it as it announces a `todowrite`: the metadata patch, `plan_update` or `plan_removed`, and `querymt/session/plansChanged`.

#### Scenario: Rewind restores the earlier plan
- **WHEN** a session wrote a one-item list, then a two-item list in a later turn, the later turn is undone, and a new prompt starts
- **THEN** `session/list` reports the one-item plan
- **AND** connections receive `querymt/session/plansChanged` with that plan

#### Scenario: Rewind past the only todowrite empties the plan
- **WHEN** the only turn with a `todowrite` is undone and a new prompt starts
- **THEN** the session's entry has no `querymt/plans` key
- **AND** connections receive `querymt/session/plansChanged` with `plans: []`

#### Scenario: Rewind of a turn without a todowrite changes nothing
- **WHEN** a turn without a `todowrite` is undone and a new prompt starts
- **THEN** no `querymt/session/plansChanged` notification is sent and the plan is as before

### Requirement: A replayed plan is removed when the list is emptied
A connection that loaded a session by replay holds the plan the history ends with. When the list is later emptied, QueryMT SHALL send that connection `plan_removed`, as it does for a plan it announced live.

#### Scenario: Emptying the list after a load
- **WHEN** a plan-operations client loads a session whose last `todowrite` has items, and a later `todowrite` empties the list
- **THEN** the client receives `plan_removed` for the `querymt-todos` plan

#### Scenario: History that ends with an emptied list
- **WHEN** a client loads a session whose last `todowrite` emptied the list, and a later `todowrite` empties it again
- **THEN** the client receives no `plan_removed` and no metadata patch

### Requirement: A connection that fell behind catches up
When a connection falls behind the agent's event stream, QueryMT SHALL compare the stored plans of the sessions it holds a plan for with what it announced, and announce the ones that differ. If the store cannot report plans, the connection SHALL forget what it announced instead.

#### Scenario: Plan changed during the lag
- **WHEN** a connection that announced a session's plan falls behind and the stored plan changes meanwhile
- **THEN** it receives one `plansChanged` with the stored plan and none for the plans in between

#### Scenario: Plan emptied during the lag
- **WHEN** a connection that announced a plan falls behind and the list is emptied meanwhile
- **THEN** it receives `plansChanged` with `plans: []`

### Requirement: A rewind restores the in-memory todo list
When the next prompt after an undo removes the session's last `todowrite` from its history, the list `todoread` returns SHALL become the list of the last remaining `todowrite` (empty when none remains), whether or not the plan clients see changed.

#### Scenario: Rewind restores the list for the model
- **WHEN** a session wrote a one-item list, then a two-item list in a later turn, the later turn is undone, and a new prompt starts
- **THEN** `todoread` returns the one-item list

#### Scenario: Rewind that changes only cancelled items resets the list quietly
- **WHEN** the undone `todowrite` differed from the earlier one only by a cancelled item
- **THEN** no `querymt/session/plansChanged` notification is sent
- **AND** `todoread` returns the earlier list
