# Spec Delta

## MODIFIED Requirements

### Requirement: Context usage and cumulative cost reporting
QueryMT SHALL emit `usage_update` whenever reliable current context utilization becomes available or materially changes. `used` SHALL represent tokens currently occupying model context, including cached tokens, and `size` SHALL represent the active model's effective context limit. QueryMT SHALL include cumulative session USD cost only for a complete supported token-cost estimate and SHALL omit standard cost otherwise. Session cost SHALL span execution cycles and remain independent of context occupancy.

#### Scenario: Provider request reports usage
- **WHEN** a provider request completes with token usage and the active model has a known context limit
- **THEN** QueryMT emits `usage_update` with the current context tokens as `used` and the effective context limit as `size`
- **AND** it includes cumulative USD cost when session accounting provides a complete supported estimate

#### Scenario: Active model changes
- **WHEN** the active model changes to one with a different known context limit
- **THEN** the next usage update uses the new model's effective limit
- **AND** prior charges remain priced using their recorded basis rather than the new model's rates

#### Scenario: Context size is unknown
- **WHEN** QueryMT cannot determine a meaningful effective context limit
- **THEN** it does not emit a malformed or estimated `usage_update`
- **AND** session accounting remains available through accounting snapshots and applicable advisory updates without fabricated context utilization

#### Scenario: Compaction reduces context
- **WHEN** compaction completes and QueryMT can determine the resulting context utilization
- **THEN** it emits a new `usage_update` reflecting the reduced context usage
- **AND** accounting includes attributable compaction charges without reducing already incurred spend

#### Scenario: New cycle in a retained session
- **WHEN** a later execution cycle completes requests in a session with recorded earlier spend
- **THEN** cumulative session cost includes recorded charges from both cycles
- **AND** its value agrees with a restored snapshot of the same durable accounting

## ADDED Requirements

### Requirement: Accounting uncertainty replaces stale cost
Accounting updates and snapshots SHALL provide a known USD subtotal, completeness state, missing-component reasons, and excluded-billing scope. Partial amounts SHALL NOT appear as complete standard cost. An update SHALL replace prior accounting state, including invalidating a previously complete total. Live delivery and replay SHALL produce equivalent accounting for the same durable records.

#### Scenario: Missing cache rate after a complete estimate
- **WHEN** a session previously reported complete cost and a later request has an unpriced nonzero cache category
- **THEN** the new accounting metadata reports the known subtotal and the missing cache-rate reason
- **AND** the standard complete cost is omitted and QueryMT's stored complete-total state is cleared rather than retaining the previous value

#### Scenario: Replay includes excluded billing
- **WHEN** accounting contains subscription-excluded attempts alongside metered API attempts
- **THEN** replay describes the excluded scope and reports only applicable metered charges
- **AND** an entirely excluded session is not represented as a known zero-dollar provider invoice

#### Scenario: Duplicate remote events
- **WHEN** the same accounting record is observed through replay and remote live delivery
- **THEN** the resulting metadata and session total match single delivery

### Requirement: Budget uncertainty is advisory and visible
A configured USD budget with incomplete cycle pricing SHALL surface a best-effort warning identifying unavailable pricing or usage. Execution SHALL continue against the known subtotal unless it reaches the cap. Notices SHALL follow negotiated capability rules and SHALL NOT require acknowledgement. Missing context size SHALL NOT suppress this warning, and repeated usage snapshots SHALL NOT flood clients with duplicate warnings.

#### Scenario: Capable client receives a partial-budget warning
- **WHEN** a cycle's configured cap cannot include a used cache category because its rate is missing
- **THEN** a notice-capable client receives an advisory explaining best-effort enforcement
- **AND** execution continues while known spend remains below the cap

#### Scenario: Client does not support notices
- **WHEN** a client lacks notice support and budget accounting becomes incomplete
- **THEN** QueryMT preserves the condition in accounting metadata and snapshots and uses the existing user-visible warning fallback
- **AND** it does not send an unsupported notice or block waiting for a user response

#### Scenario: Known subtotal exhausts an incomplete budget
- **WHEN** the known cycle subtotal reaches the cap despite unresolved charges
- **THEN** the next paid attempt is stopped with the existing price-limit outcome
- **AND** the reported accounting remains explicitly incomplete
