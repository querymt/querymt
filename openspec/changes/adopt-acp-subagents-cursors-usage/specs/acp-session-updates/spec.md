## ADDED Requirements

### Requirement: Turn usage on prompt responses
A `session/prompt` response SHALL carry `usage` with the token counts of the execution cycle that answered the prompt: input, output, thought (reasoning), cache read, cache write, and their sum as total. WHEN no provider attempt in that cycle reported usage, `usage` SHALL be omitted rather than reported as zero.

#### Scenario: Completed turn reports its usage
- **WHEN** a prompt completes after two provider requests reporting 1000 and 500 input tokens and 200 and 100 output tokens
- **THEN** the prompt response carries `usage` with `inputTokens` 1500, `outputTokens` 300 and `totalTokens` equal to the sum of all reported categories

#### Scenario: Turn without reported usage
- **WHEN** a prompt completes and no provider attempt in its cycle reported token usage
- **THEN** the prompt response has no `usage` field

#### Scenario: Cancelled turn reports usage incurred so far
- **WHEN** a prompt is cancelled after one provider request reported usage
- **THEN** the response with stop reason `cancelled` carries `usage` for that request

#### Scenario: Turn usage excludes earlier cycles
- **WHEN** a second prompt completes in a session whose first prompt used 1000 input tokens
- **THEN** the second response's `usage` counts only the second cycle's tokens

## MODIFIED Requirements

### Requirement: Capability isolation by connection
QueryMT SHALL derive Preview update support from each ACP connection's negotiated client capabilities and SHALL apply those capabilities only to notifications delivered through that connection. This applies to plan, notice, compaction, cursor and subagent updates.

#### Scenario: Clients advertise different Preview capabilities
- **WHEN** two clients connected to QueryMT advertise different plan, notice, compaction, cursor or subagent capabilities
- **THEN** each client receives only the update variants it advertised plus ungated stable updates

#### Scenario: Capability is omitted or null
- **WHEN** a client omits a Preview capability or supplies it as `null`
- **THEN** QueryMT treats that capability as unsupported
