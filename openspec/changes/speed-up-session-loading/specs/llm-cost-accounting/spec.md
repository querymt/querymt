# Spec Delta

## MODIFIED Requirements

### Requirement: Accounting is idempotent across delivery paths
One provider attempt SHALL contribute at most one settled charge. Repeated snapshots, final response delivery, event replay, remote redelivery, chunked or out-of-order transfer of remote history, and replacement of a local copy after the owner rewrote its history SHALL NOT create additional charges or remove recorded ones. Distinct dispatched retries SHALL have distinct identities. Conflicting settlements for one identity SHALL surface an accounting error rather than silently overwrite or add another charge.

#### Scenario: Remote settlement is delivered twice
- **WHEN** a recorded settlement is replayed and also arrives through a remote live relay
- **THEN** session and cycle totals include its charge once

#### Scenario: Distinct retry attempts report usage
- **WHEN** an unsuccessful attempt reports usage and a separately dispatched retry succeeds
- **THEN** both attempts' attributable usage is retained and valued independently
- **AND** retry output is not mixed with the earlier attempt's output

#### Scenario: Settlement arrives in a history chunk and over the live relay
- **WHEN** a recorded settlement is transferred as part of a history chunk, possibly before older events, and also arrives through the live relay
- **THEN** session and cycle totals include its charge once

#### Scenario: Local copy replaced after the owner's undo
- **WHEN** the owner rewrote its history by undo and the attaching agent replaces its local copy of that session's events
- **THEN** charges the owner retained remain counted exactly once
- **AND** no charge is lost or duplicated by the replacement
