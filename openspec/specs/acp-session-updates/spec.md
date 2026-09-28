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
QueryMT SHALL emit `usage_update` whenever reliable current context utilization becomes available or materially changes. `used` SHALL represent tokens currently occupying model context, including cached tokens, and `size` SHALL represent the active model's effective context limit. QueryMT SHALL include cumulative session cost in USD when it is available and SHALL omit cost when it cannot calculate it reliably.

#### Scenario: Provider request reports usage
- **WHEN** a provider request completes with token usage and the active model has a known context limit
- **THEN** QueryMT emits `usage_update` with the current context tokens as `used` and the effective context limit as `size`
- **AND** it includes cumulative USD cost when that value is available

#### Scenario: Active model changes
- **WHEN** the active model changes to one with a different known context limit
- **THEN** the next usage update uses the new model's effective limit

#### Scenario: Context size is unknown
- **WHEN** QueryMT cannot determine a meaningful effective context limit
- **THEN** it does not emit a malformed or estimated `usage_update`

#### Scenario: Compaction reduces context
- **WHEN** compaction completes and QueryMT can determine the resulting context utilization
- **THEN** it emits a new `usage_update` reflecting the reduced context usage

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
