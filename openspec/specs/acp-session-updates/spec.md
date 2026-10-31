# acp-session-updates Specification

## Purpose

Defines the ACP v1 session notifications through which QueryMT keeps clients synchronized with dynamic session state, resource usage, plans, advisory notices, and context compaction.

## Requirements

### Requirement: Mode and configuration synchronization
QueryMT SHALL publish the current session mode and the complete current configuration-option set whenever an active session's mode, model, profile, or reasoning effort changes. Mode changes SHALL produce both `current_mode_update` and `config_option_update`; other configuration changes SHALL produce `config_option_update`. Each configuration update SHALL contain a self-consistent full replacement rather than a partial option patch.

#### Scenario: Session mode changes
- **WHEN** an active ACP session changes from one supported mode to another
- **THEN** QueryMT emits `current_mode_update` with the new mode
- **AND** QueryMT emits `config_option_update` containing the complete option set with the new mode selected

#### Scenario: Non-mode configuration changes
- **WHEN** the active model, profile, or reasoning effort changes
- **THEN** QueryMT emits `config_option_update` containing the complete option set with every current value reflected

#### Scenario: No effective configuration change
- **WHEN** a configuration request leaves all effective configuration values unchanged
- **THEN** QueryMT does not emit a redundant configuration update

### Requirement: Session metadata synchronization
QueryMT SHALL emit `session_info_update` when mutable, persisted session metadata changes. The update SHALL use ACP patch semantics, SHALL include the changed title or activity timestamp fields, and subsequent session-list results SHALL reflect the same persisted values.

#### Scenario: Session title is assigned or changed
- **WHEN** QueryMT persists a new title for an active session
- **THEN** it emits `session_info_update` with that title
- **AND** a later session-list response reports the same title

#### Scenario: Session activity timestamp changes
- **WHEN** QueryMT persists a new last-activity time for an active session
- **THEN** it emits `session_info_update` with an RFC 3339 `updatedAt` value representing that time

#### Scenario: Metadata is cleared
- **WHEN** QueryMT intentionally clears a mutable nullable metadata field
- **THEN** it emits that field as `null` rather than omitting it

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

### Requirement: Plan lifecycle and compatibility
QueryMT SHALL report its current task plan through ACP. When the client advertises plan-operations support, QueryMT SHALL use a stable plan identity with `plan_update` full replacements and `plan_removed` lifecycle notifications. Otherwise, QueryMT SHALL retain the legacy `plan` update behavior and SHALL NOT send unsupported plan-operation variants.

#### Scenario: Capable client receives a plan
- **WHEN** a client advertises plan-operations support and QueryMT creates or changes its task plan
- **THEN** QueryMT emits an item-based `plan_update` using the same stable plan ID for that plan's lifetime
- **AND** the update contains the complete current entries

#### Scenario: Capable client sees plan removal
- **WHEN** a previously reported plan no longer exists or its lifecycle is explicitly dismissed
- **THEN** QueryMT emits `plan_removed` once for that plan ID

#### Scenario: Legacy client receives a plan
- **WHEN** a client does not advertise plan-operations support and QueryMT creates or changes its task plan
- **THEN** QueryMT emits the legacy `plan` update
- **AND** it does not emit `plan_update` or `plan_removed`

### Requirement: Advisory session notices
QueryMT SHALL emit ACP `notice` updates only for user-relevant advisory conditions that do not require acknowledgement and do not determine protocol correctness. On ACP v1 connections, QueryMT SHALL send notices only when the client advertises session-notice support. Notices SHALL remain separate from conversation history and SHALL NOT replace errors, permission requests, or other response-bearing interactions.

#### Scenario: Advisory event for a capable client
- **WHEN** a user-relevant advisory event occurs and the ACP v1 client advertises notice support
- **THEN** QueryMT emits a `notice` with an appropriate severity, non-empty title, and optional description

#### Scenario: Client lacks notice support
- **WHEN** an advisory event occurs and the ACP v1 client does not advertise notice support
- **THEN** QueryMT does not emit a `notice`
- **AND** it uses an existing user-visible fallback only when the advisory information must still be surfaced

#### Scenario: Condition requires user action
- **WHEN** continued execution depends on a user decision or authorization
- **THEN** QueryMT uses the applicable response-bearing ACP primitive instead of a notice

### Requirement: Compaction lifecycle reporting
QueryMT SHALL represent each context compaction as one ID-addressed ACP entity. On ACP v1 connections, it SHALL emit compaction updates only when the client advertises compaction support. QueryMT SHALL send an initial `in_progress` update before summary chunks, reuse the same unique compaction ID through one terminal status, and expose only unencrypted user-displayable summary content.

#### Scenario: Successful compaction with a final summary
- **WHEN** a capable client is connected and context compaction starts
- **THEN** QueryMT emits `compaction_update` with a new session-unique ID and `in_progress` status
- **AND WHEN** compaction succeeds with a safe user-displayable summary
- **THEN** QueryMT emits `compaction_update` for the same ID with `completed` status and the authoritative summary

#### Scenario: Incremental compaction summary
- **WHEN** safe summary content becomes available incrementally during compaction
- **THEN** QueryMT emits each `compaction_summary_chunk` after the initial update and before the terminal update using the same compaction ID

#### Scenario: Compaction fails or is cancelled
- **WHEN** compaction terminates unsuccessfully after its initial update
- **THEN** QueryMT emits exactly one terminal `failed` or `cancelled` update for the same compaction ID
- **AND** a failure description is included only for the `failed` state

#### Scenario: Summary is unsafe or unavailable
- **WHEN** compaction output is encrypted, contains internal framing, or has no user-displayable projection
- **THEN** QueryMT omits the summary while still reporting the lifecycle status

#### Scenario: Client lacks compaction support
- **WHEN** an ACP v1 client does not advertise compaction support
- **THEN** QueryMT emits neither `compaction_update` nor `compaction_summary_chunk`

### Requirement: Capability isolation by connection
QueryMT SHALL derive Preview update support from each ACP connection's negotiated client capabilities and SHALL apply those capabilities only to notifications delivered through that connection.

#### Scenario: Clients advertise different Preview capabilities
- **WHEN** two clients connected to QueryMT advertise different plan, notice, or compaction capabilities
- **THEN** each client receives only the update variants it advertised plus ungated stable updates

#### Scenario: Capability is omitted or null
- **WHEN** a client omits a Preview capability or supplies it as `null`
- **THEN** QueryMT treats that capability as unsupported

### Requirement: Replay semantics
QueryMT SHALL replay durable session-update state in materialized form when session history is replayed. Replayed plans and compactions SHALL retain their original identities and final content without duplicating streamed chunks. QueryMT SHALL NOT replay live-only notices or transient in-progress compaction states.

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
