# Spec Delta

## Purpose

Defines what the app knows about the plans (todo lists) of the sessions it lists, where that knowledge comes from, and how it stays current, so views can show a session's progress without opening it.

## ADDED Requirements

### Requirement: Listed sessions carry their plans
The app SHALL read each `session/list` entry's `_meta["querymt/plans"]` and keep, for that session, every element whose `type` is `items`, with its `planId` and entries. A missing key, a value that is not an array, or elements of another `type` SHALL leave the session without those plans. A malformed key SHALL NOT drop the session from the list.

#### Scenario: Plan read from the session list
- **WHEN** the agent lists a session whose `_meta["querymt/plans"]` holds one `items` plan with one completed and two pending entries
- **THEN** the app knows that session's plan as 1 of 3 done, with the three entries in order

#### Scenario: Agent without plans in its list
- **WHEN** the agent's `session/list` entries have no `querymt/plans` key
- **THEN** the sessions are listed as before and have no plan

#### Scenario: Unknown plan type is ignored
- **WHEN** a listed session's `querymt/plans` holds a `markdown` plan only
- **THEN** that session has no plan

#### Scenario: Malformed plans keep the session
- **WHEN** a listed session's `querymt/plans` is a string
- **THEN** the session is listed with no plan

### Requirement: Plan changes update listed sessions
On `querymt/session/plansChanged`, the app SHALL replace the plans of the named session with the notification's `plans`, without a `session/list` request. `plans: []` SHALL clear them. A notification for a session the app does not list SHALL be ignored.

#### Scenario: Progress moves while a background session runs
- **GIVEN** session A is listed with 1 of 3 todos done and is not open in the app
- **WHEN** a `plansChanged` notification for A reports 2 of 3 done
- **THEN** the app knows A's plan as 2 of 3 done without listing sessions again

#### Scenario: Plan emptied
- **WHEN** a `plansChanged` notification for a listed session has `plans: []`
- **THEN** that session has no plan

#### Scenario: Notification for an unknown session
- **WHEN** a `plansChanged` notification names a session the app has not listed
- **THEN** no session is added and nothing else changes

### Requirement: A session open in the app uses its live plan
For a session open in the app, its progress SHALL come from the session's own plan updates as soon as it has one, instead of the listed plan. A later session-list refresh SHALL NOT replace it.

#### Scenario: Live plan wins over a stale list
- **GIVEN** session A is open in the app and its live plan has 3 of 4 done
- **WHEN** a session-list refresh still reports 2 of 4 for A
- **THEN** the app shows A as 3 of 4 done

#### Scenario: Open session without a live plan yet
- **GIVEN** session A was listed with 2 of 4 done and is being opened in the app
- **WHEN** its replay has not delivered a plan yet
- **THEN** the app shows A as 2 of 4 done

### Requirement: Progress counts completed entries
A plan's progress SHALL be the number of entries with status `completed` over the number of entries. A session with several `items` plans SHALL count the plan with id `querymt-todos` if present, else the first one. A plan with no entries SHALL count as no plan.

#### Scenario: Progress of a todo plan
- **WHEN** a session's `querymt-todos` plan has entries `completed`, `in_progress`, `pending`, `pending`
- **THEN** its progress is 1 of 4

#### Scenario: Empty plan shows no progress
- **WHEN** a session's only plan has no entries
- **THEN** the session has no progress
