# Spec Delta

## ADDED Requirements

### Requirement: Requests answer every tool call
Every model request SHALL answer each assistant tool call with exactly one tool result, placed in the message directly following the assistant message that contains the call. Producing a request that violates this is a defect: the agent SHALL repair the request by adding a cancelled error result for each unanswered call and SHALL record an error naming the call, so the provider never rejects the request for a missing result. Repair SHALL NOT change the stored transcript.

#### Scenario: Unanswered call before a request
- **WHEN** the history about to be sent ends with an assistant tool call that has no result
- **THEN** the request carries a cancelled error result for that call directly after it, and an error naming the call id is logged

#### Scenario: Answered calls are untouched
- **WHEN** every tool call in the history has its result
- **THEN** the request messages are sent unchanged and nothing is logged
