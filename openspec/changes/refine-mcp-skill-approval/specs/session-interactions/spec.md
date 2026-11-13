## ADDED Requirements

### Requirement: Form question outcomes are recorded
Every form question a session announces with an `elicitation_requested` event (built-in questions, skill approvals and MCP server elicitations) SHALL end with exactly one durable `elicitation_resolved` event after it. The event SHALL carry the question's `elicitation_id` and an outcome of `accepted`, `declined` or `cancelled`. A question that ends without an answer SHALL be recorded as `cancelled`.

#### Scenario: Skill approval answer is recorded
- **WHEN** the user answers "Allow for this session" to the approval request for `docs.guide`
- **THEN** the session journal contains an `elicitation_resolved` event with that request's `elicitation_id`, outcome `accepted` and selection `["Allow for this session"]`, after its `elicitation_requested` event

#### Scenario: Declined question is recorded
- **WHEN** the user declines a built-in question
- **THEN** the session journal contains an `elicitation_resolved` event with that question's `elicitation_id` and outcome `declined`

#### Scenario: Cancelled run records the pending question as cancelled
- **WHEN** a run is cancelled while a skill approval request is pending
- **THEN** the session journal contains an `elicitation_resolved` event for that request with outcome `cancelled`

### Requirement: Recorded outcomes keep server form content private
For built-in questions and skill approvals, an `accepted` outcome SHALL carry the selected option labels. For MCP server elicitations, the `elicitation_resolved` event MUST NOT contain the submitted form content.

#### Scenario: Server elicitation content is not recorded
- **WHEN** an MCP server elicitation is accepted with form content
- **THEN** the session journal contains an `elicitation_resolved` event with outcome `accepted` and without the submitted content
