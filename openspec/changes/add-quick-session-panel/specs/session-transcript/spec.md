# Spec Delta

## MODIFIED Requirements

### Requirement: One reducer as the transcript source
Each open session's transcript SHALL be built only from its own QueryMT session reducer. Every agent input for that session (load response, session updates including replayed ones, state updates, runtime and input-state notifications, older-history pages, full tool content, undo stacks, config option responses, and the client's prompt submissions and outcomes) SHALL be applied to it in the order received. Inputs for another session SHALL NOT change the transcript.

#### Scenario: Replayed updates before the load response
- **WHEN** an agent replays history as session updates before answering the load of an open session
- **THEN** that session's transcript shows the replayed conversation once, and the load response does not duplicate it

#### Scenario: Update for another session
- **WHEN** a session update for a different session id arrives on the same connection
- **THEN** the open session's transcript is unchanged

#### Scenario: Two open sessions on one connection
- **GIVEN** sessions A and B are both open and belong to the same agent connection
- **WHEN** updates for A and B arrive interleaved
- **THEN** each transcript contains exactly its own session's updates, applied in the order they arrived
