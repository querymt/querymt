# Spec Delta

## ADDED Requirements

### Requirement: Flat-chunk tool calls are pending only after an explicit start
When a provider streams flat (non-structured) chunks, a tool call SHALL become pending only when its start is received. An argument delta for an index with no start SHALL NOT create a pending call and SHALL NOT fail the response. A call that started but never completed before a non-length terminal SHALL still fail the response.

#### Scenario: Orphan argument delta
- **WHEN** a flat stream sends an argument delta for index 1 without a start for index 1, then completes with a tool-calls stop after completing call 0
- **THEN** the response completes successfully with call 0 only

#### Scenario: Started call never completes
- **WHEN** a flat stream starts call 1, sends its argument deltas, and terminates with a tool-calls stop without completing it
- **THEN** the response fails with an unfinished-tool-call error naming index 1 and no call is dispatched

#### Scenario: Started call cut off by the length limit
- **WHEN** a flat stream starts call 1 and terminates with a length stop without completing it
- **THEN** the response is incomplete with the output-token-limit cause, not failed

### Requirement: Orphan argument deltas are reported once per index
An argument delta for a flat-chunk index with no start SHALL be logged as a provider contract warning naming the index, at most once per index per attempt. An argument delta for a call that already completed SHALL NOT be reported as a missing start.

#### Scenario: Repeated orphan deltas
- **WHEN** a flat stream sends many argument deltas for an index that was never started
- **THEN** the response outcome is unaffected and the warning for that index is logged once

#### Scenario: Delta after completion
- **WHEN** a flat stream sends an argument delta for a call that already completed
- **THEN** no missing-start warning is logged and the completed call is unchanged
