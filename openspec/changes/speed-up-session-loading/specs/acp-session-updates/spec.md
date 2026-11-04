# Spec Delta

## MODIFIED Requirements

### Requirement: Replay semantics
QueryMT SHALL replay durable session-update state in materialized form when session history is replayed. Replayed plans and compactions SHALL retain their original identities and final content without duplicating streamed chunks. QueryMT SHALL NOT replay live-only notices or transient in-progress compaction states. For a remote-hosted session, replay SHALL be built from the local copy of the session's events and SHALL NOT cause events already transferred for the load to be transferred again.

#### Scenario: Replay includes a current plan
- **WHEN** a capable client loads a session with a retained plan
- **THEN** QueryMT replays one materialized `plan_update` using the retained plan ID and current complete content

#### Scenario: Replay includes completed compaction
- **WHEN** a capable client loads a session containing a completed compaction
- **THEN** QueryMT replays one terminal `compaction_update` with its original ID and materialized summary
- **AND** it does not also replay historical summary chunks that would duplicate the summary

#### Scenario: Replay excludes notices and transient progress
- **WHEN** QueryMT replays retained session history
- **THEN** it does not replay prior notices
- **AND** it does not replay an obsolete `in_progress` compaction state before a retained terminal state

#### Scenario: Replay of a remote session after attach
- **WHEN** a client that receives history as session updates loads a remote-hosted session whose events were transferred during attach
- **THEN** it receives the same updates as for a local session with the same events
- **AND** no event crosses the mesh a second time for the replay

### Requirement: Load snapshots contain authoritative ordered accounting
`SessionLoadSnapshot.accounting` SHALL contain the shared reducer's authoritative replacement DTO at the existing snapshot cursor, without requiring historical replay or known context limits. Snapshot and captured-live application SHALL respect existing local/remote ordering and deduplication. Older state SHALL NOT overwrite newer accounting or restore an invalidated complete total. When a client requests a load window, `accounting` SHALL still cover the entire session, not only the events in the window. When older history of a remote-hosted session is not yet held locally, `accounting` SHALL cover the locally held events and SHALL report the missing history through the existing completeness metadata: `complete` false, `completeTotalUsd` null and a reason identifying untransferred history. Once that history is local, the reason SHALL be absent.

#### Scenario: Desktop follow-up consumes backend accounting
- **WHEN** the backend client contract is documented for desktop implementation
- **THEN** snapshot and notification handling use the same authoritative accounting DTO instead of reconstructing monetary totals from request-end history
- **AND** metadata-only accounting updates do not require catalog refreshes unless actual session-summary fields change

#### Scenario: Snapshot hydration without historical replay
- **WHEN** a client hydrates from a load snapshot without historical notification replay and context size is unknown
- **THEN** the snapshot supplies accounting including failed and auxiliary attempts
- **AND** its totals and reasons agree with reducer-backed live state at that cursor

#### Scenario: Newer partial accounting survives load merging
- **WHEN** an older snapshot reports a complete amount and newer captured-live accounting explicitly clears that amount
- **THEN** the newer accounting wins according to the existing cursor ordering
- **AND** merging candidates does not refill the explicit null from the older complete amount

#### Scenario: Manual budget pricing differs from API pricing
- **WHEN** manual budget rates cannot price a used cache category but API pricing can
- **THEN** accounting preserves the complete API estimate separately from the incomplete budget valuation and budget-specific reason
- **AND** ordinary in-flight attempts alone do not trigger a missing-price budget warning

#### Scenario: Windowed load reports whole-session accounting
- **WHEN** the same session is loaded once with a load window that excludes older turns and once without a window, and the same history is held locally for both
- **THEN** both snapshots carry identical accounting values and completeness metadata

#### Scenario: Remote older history not yet local
- **WHEN** a windowed load of a remote-hosted session is answered before its older history is held locally
- **THEN** `accounting` reports `complete` false, a null `completeTotalUsd` and a reason identifying untransferred history
- **AND** a later load after the history is local reports the exact accounting without that reason
