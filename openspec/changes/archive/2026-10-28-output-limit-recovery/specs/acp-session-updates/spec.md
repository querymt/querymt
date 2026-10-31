# Spec Delta

## ADDED Requirements

### Requirement: Run stops are reported to clients
When a prompt's run ends on a stop rather than normal completion, QueryMT SHALL return the matching ACP stop reason in the prompt response. Clients that advertise notice support SHALL also receive a live warning `notice` naming the cause. Stop notices are live-only and SHALL NOT be replayed. The prompt response SHALL NOT carry QueryMT-specific stop metadata: the ACP stop reason is the protocol-level signal, and the cause is conveyed by the notice and by QueryMT's own event stream.

#### Scenario: Output-limit stop with a notice-capable client
- **WHEN** a run ends because the model hit its output-token limit and the client advertises notice support
- **THEN** the client receives a warning `notice` whose title names the output-token limit
- **AND** the prompt response has stop reason `max_tokens`

#### Scenario: Client without notice support
- **WHEN** the same stop occurs on a client that does not advertise notice support
- **THEN** no `notice` is sent and the prompt response has stop reason `max_tokens`

#### Scenario: Replay after a stop
- **WHEN** a client loads a session whose last run ended on a stop
- **THEN** QueryMT does not replay the stop notice
