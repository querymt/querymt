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
- **AND** it emits replacement accounting through metadata-only `session_info_update._meta.querymt.accounting` and includes accounting directly in load snapshots without fabricated context utilization

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
Accounting updates and snapshots SHALL use the same versioned DTO with scoped known subtotals, nullable complete totals, completeness reasons, and coverage/exclusion counts. A present payload SHALL atomically replace accounting, including explicit nulls; absence SHALL mean no accounting update. Partial amounts SHALL NOT appear as complete standard cost. Live delivery and replay SHALL produce equivalent accounting for the same durable records.

#### Scenario: Replacement payload describes monetary scopes
- **WHEN** QueryMT publishes accounting
- **THEN** it provides session API-spend and explicitly identified active-cycle scopes, each with completeness state, reasons, and unresolved/incomplete/excluded counts and historical coverage
- **AND** it represents no active cycle explicitly and keeps cycle budget valuation and reasons separate from API pricing

#### Scenario: Message has no accounting payload
- **WHEN** a message omits accounting metadata
- **THEN** it does not update QueryMT accounting state
- **AND** omission of standard ACP `cost` alone does not define QueryMT clearing semantics

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

### Requirement: Existing token metadata retains its scope
QueryMT SHALL preserve `_meta.querymt.usage` v1's JSON shape and main-request telemetry scope alongside accounting metadata. Retry and auxiliary accounting SHALL NOT silently broaden its latest-request or session token totals. Token telemetry and all-attempt monetary accounting SHALL remain independently scoped.

#### Scenario: Auxiliary settlement changes accounting
- **WHEN** compaction or delegation-summary charges settle without a new main request completing
- **THEN** accounting includes those charges
- **AND** existing usage v1 latest-request and session token totals are not replaced with all-attempt totals

### Requirement: Accounting delivery does not require context or notices
QueryMT SHALL carry `_meta.querymt.accounting` on `usage_update` when reliable main-context occupancy and the active model's effective limit are available. Otherwise it SHALL emit the same DTO through metadata-only `session_info_update`, independently of notice capability. Accounting delivery SHALL NOT fabricate context or change session-summary fields solely to carry accounting.

#### Scenario: Auxiliary cost changes with valid main context
- **WHEN** auxiliary charges change accounting and a reliable main-context measurement and active-model limit are available
- **THEN** QueryMT emits a usage update with that measurement and the updated accounting
- **AND** auxiliary token usage does not overwrite main-context occupancy

#### Scenario: Unknown context and unsupported notices
- **WHEN** accounting changes without a reliable context measurement/limit pair and the client has not negotiated notices
- **THEN** QueryMT emits metadata-only `session_info_update` carrying replacement accounting without changing session-summary fields
- **AND** it sends neither a fabricated `usage_update` nor an unsupported notice

#### Scenario: Model switch invalidates a prior measurement
- **WHEN** the active model changes and the previous context measurement has not been validated for the new model
- **THEN** accounting remains available through the metadata-only fallback
- **AND** QueryMT does not pair a stale measurement or obsolete limit with the new model merely to emit a usage update

### Requirement: Load snapshots contain authoritative ordered accounting
`SessionLoadSnapshot.accounting` SHALL contain the shared reducer's authoritative replacement DTO at the existing snapshot cursor, without requiring historical replay or known context limits. Snapshot and captured-live application SHALL respect existing local/remote ordering and deduplication. Older state SHALL NOT overwrite newer accounting or restore an invalidated complete total.

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
