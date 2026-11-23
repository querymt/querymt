## ADDED Requirements

### Requirement: Standalone provider hosts can opt into peer policy

A standalone provider host started with a grant and quota store SHALL enforce the same provider policy as an agent host, in addition to the global model allowlist. If a supplied store cannot be opened, startup SHALL fail. Without a store, any connected peer MAY consume globally shared models, without per-peer grants or quotas.

#### Scenario: Standalone sharing respects its configured mode
- **WHEN** a standalone host shares the same global model allowlist with and without a peer access store
- **THEN** in store-backed mode a peer needs a matching grant and available quota; in no-store mode connected peers may use the allowlisted models without per-peer checks; neither mode exposes a model outside the global allowlist

### Requirement: Provider grants filter discovery and execution

A Custom grant SHALL allow selected host providers, each with all currently and subsequently available models or a non-empty list of canonical model IDs. Quota configuration SHALL NOT grant model access. The host SHALL enforce selection on catalogs, contract queries, direct inference, and session-triggered inference. It SHALL use host-owned bindings and credentials and SHALL NOT silently fall back on policy denial.

#### Scenario: Only the selected provider and models are usable
- **WHEN** a peer can use one model on one provider and attempts catalog, contract, and chat operations for permitted and unpermitted pairs
- **THEN** only the permitted pair is advertised and executes, and denied calls never reach an upstream provider

#### Scenario: All models follows the host catalog
- **WHEN** the host adds a model to a provider shared with two peers, one with All models and one with a fixed selection that excludes the new model
- **THEN** the new model is shared only with the All models peer, subject to that peer's applicable quotas

#### Scenario: Host provider configuration is authoritative
- **WHEN** a request for an allowed pair includes an override of a host-owned provider setting
- **THEN** the host rejects the override or uses the configured binding without changing the effective pair, endpoint, or credential

#### Scenario: Provider revocation stops an active stream
- **WHEN** the host revokes a grant while the peer is receiving a stream for the removed model
- **THEN** the host stops delivery, cancels the affected attempt at the next supported boundary, and still settles its usage without interrupting other peers' streams

### Requirement: Provider and model quotas apply together

Each peer SHALL have an independent allowance per provider and per model within that provider. The default is unlimited; finite caps SHALL be positive whole tokens. Invalid or overflowing values SHALL be rejected. All models share the provider cap. Each attempt SHALL count once against every applicable finite cap, even if the other scope is unlimited.

#### Scenario: Session inference and direct inference share an allowance
- **WHEN** a peer spends tokens in a direct stream and then through a hosted session using the same model, and also calls a different allowed model on that provider
- **THEN** all attempts draw from the same provider balance, each model balance reflects only its own attempts, and no other peer's or provider's balance changes

#### Scenario: Provider and model limits are independently binding
- **WHEN** either a provider cap or a model cap has no remaining capacity after accounting for usage and outstanding reservations, even if the other scope has capacity or is unlimited
- **THEN** the new attempt is denied for quota at the exhausted scope, reserves nothing at either scope, and does not dispatch

### Requirement: Finite quotas can renew on UTC calendar boundaries

Each finite quota SHALL have one reset schedule: manual (the default), daily at 00:00 UTC, weekly on Monday at 00:00 UTC, or monthly on the first day at 00:00 UTC. The provider and model schedules MAY differ. Unused capacity SHALL NOT carry over. A new attempt SHALL use the windows current at reservation time; settlement SHALL remain in those windows even after renewal. The host SHALL NOT reopen a closed window for new reservations after restart or clock rollback.

#### Scenario: Recurring windows renew at UTC calendar boundaries
- **WHEN** the host reaches a configured daily, Monday weekly, or first-of-month boundary, including while offline
- **THEN** the applicable scope opens a new window with the full configured allowance and no carryover; other scopes keep their current windows until their own boundaries, and a restart or clock rollback does not reopen an old window

#### Scenario: A stream spanning renewal settles against its original windows
- **WHEN** an attempt has provider and model reservations and one or both windows renew before it finishes
- **THEN** the attempt settles into the original windows, and new attempts use the current balance at each scope

### Requirement: Finite allowances are reserved before upstream work

Before dispatch, the host SHALL atomically reserve the full token estimate in each applicable finite window if all scopes have positive capacity after usage and outstanding reservations, even if the estimate exceeds that capacity. Otherwise it SHALL deny for quota, without reserving or dispatching. Quota exhaustion SHALL be distinct from grant denial. Attempts with no finite caps SHALL check access but SHALL NOT reserve tokens.

#### Scenario: Parallel attempts can cross the cap on the last admission
- **WHEN** both applicable scopes have 100 tokens available and three requests each need a 60-token reservation before any of them settles
- **THEN** the first two admitted requests each reserve 60 tokens in both scopes and may run in parallel, and the third is denied without a reservation or upstream dispatch

#### Scenario: Unlimited inference needs no token estimate
- **WHEN** an otherwise allowed request has no applicable finite cap
- **THEN** it may dispatch without a quota reservation

### Requirement: All providers use the same reservation estimate

For all providers and request forms, the estimate SHALL include the prepared prompt (including separately configured system context and tool definitions) plus the effective output cap, limited by configured llama.cpp `n_ctx` or the models.dev context window. If there is no configured `n_ctx` and the model is absent from models.dev, the host SHALL use the profile context middleware's `fallback_max_tokens` if available. With no applicable ceiling the estimate SHALL be uncapped. The host SHALL NOT use profile context overrides or add a separate media allowance.

#### Scenario: One reservation estimate applies to every provider and request form
- **WHEN** otherwise authorized requests with finite quotas use one-shot or streaming inference, including requests containing media, and some supply an explicit `max_tokens`
- **THEN** each reservation is the estimated prompt plus the effective output cap, using the explicit cap when the bound provider supports the override and clamping the sum to models.dev context or configured llama.cpp `n_ctx` as applicable, without separate media rules or use of a context override

#### Scenario: Absent model metadata does not prevent admission
- **WHEN** the model is absent from models.dev, no llama.cpp `n_ctx` is configured, and each applicable finite scope has capacity
- **THEN** the host caps the estimate at the profile context middleware's `fallback_max_tokens` if present, otherwise reserves the uncapped estimate, and dispatches without rejecting for missing metadata

### Requirement: Metering uses existing deadlines

Metering SHALL NOT add a separate inference or preparation timeout to existing provider and session deadlines and cancellation.

#### Scenario: A slow provider can complete an authorized request
- **WHEN** a peer's authorized attempt runs longer than the former mesh-only deadline but the caller and provider are still waiting
- **THEN** metering does not stop the request solely because of elapsed time

### Requirement: Settlement uses host-observed attempt usage

The host SHALL settle each dispatched attempt once at its actual normalized total, counting all token kinds equally even above the estimate. If usage is missing or incomplete, it SHALL charge the reservation by default, with a code-only option to charge the estimated prompt instead. Missing unlimited usage SHALL remain unknown. Confirmed pre-dispatch failures SHALL release reservations.

#### Scenario: Streaming and one-shot usage settle once
- **WHEN** a one-shot response reports usage and a stream produces cumulative usage and repeats delivery across reconnect
- **THEN** each applicable reservation settles once for the same normalized total of uncached input, output, reasoning, cache reads and aggregate cache writes, without summing repeated cumulative usage, adding cache-write tiers, or trusting client claims

#### Scenario: Actual usage can exceed the reservation
- **WHEN** the host observes an authoritative token total greater than the attempt's reservation
- **THEN** it charges the full total to the original windows and does not fault the model or suspend the binding; new attempts are admitted according to remaining capacity

#### Scenario: Cancelled work without final usage is not free
- **WHEN** a finite attempt is already dispatched and disconnects or is cancelled without complete usage
- **THEN** the reservation becomes estimated usage by default, or the estimated prompt alone is charged as estimated usage if the programmatic option is selected, even if the peer never receives output

### Requirement: Quotas survive restart and access edits

Quota configuration and settled usage SHALL survive restart. Reservations SHALL live in memory. A crash forgets the charge of requests in flight. Access or amount edits SHALL NOT reset quotas. Full attempts SHALL NOT spend saved Custom quotas. A change of quota kind or cadence SHALL require explicit reconfiguration.

#### Scenario: Restart and regrant do not top up an allowance
- **WHEN** the host restarts after settling usage and the owner disables and reenables access within the same window
- **THEN** the settled usage counts against both applicable quotas

#### Scenario: A crash forgets in-flight charges
- **WHEN** the host restarts after a crash with previously unsettled attempts
- **THEN** their reservations and charges are gone, but saved caps and settled usage remain

#### Scenario: Changing a cap preserves consumption
- **WHEN** the owner edits the amount of an existing finite provider or model cap without changing its schedule
- **THEN** the new amount applies to new attempts but settled usage and outstanding reservations remain in the same window; a reduction below consumption leaves no available capacity

#### Scenario: Quota exhaustion does not interrupt authorized work
- **WHEN** reservations, settlement, or a lower cap leave no remaining capacity while an authorized attempt is in flight and the grant remains in effect
- **THEN** that attempt can finish and settle normally, including delivery of its result, but any new attempt at the exhausted scope is denied until capacity is available

#### Scenario: Changing a schedule requires explicit reconfiguration
- **WHEN** the owner tries to change an existing model cap from weekly to daily as an ordinary grant edit, then explicitly reconfigures that scope after outstanding attempts finish
- **THEN** the ordinary edit is refused, and the explicit reconfiguration opens a fresh daily allowance with the next UTC midnight as its renewal, retaining prior usage and leaving the provider schedule and balance unchanged

### Requirement: Owner resets apply to one quota scope

A reset or reconfiguration SHALL target one provider or model scope and be refused while attempts at that scope are outstanding. It SHALL retain prior usage and leave other scopes unchanged. A reset without a schedule change SHALL NOT move the current renewal boundary.

#### Scenario: Reset waits for outstanding attempts
- **WHEN** the owner requests a model reset with an attempt outstanding and retries after it settles
- **THEN** the first reset is refused and the second opens a fresh model allowance without changing provider or other model usage, previous records, or the next scheduled renewal

### Requirement: Quota ledger prunes expired history

The host SHALL retain the current and previous quota period for each scope and all open reservations regardless of age. Open attempts are tracked in memory. Completed replay records expire when the periods they charged are no longer retained. The host SHALL prune expired quota periods without a new setting.

#### Scenario: Pruning preserves late settlement and status
- **WHEN** the host starts or quota renews with older settled history and an outstanding reservation from an older period
- **THEN** the reservation can still settle in its original period, the current and previous period remain visible, and expired history not needed for settlement or replay is removed

### Requirement: Quota status identifies the relevant limits

For each applicable scope the host SHALL report the limit or unlimited, known and estimated usage, and outstanding reservations and available capacity for finite caps. It SHALL report finite period start and next renewal time when scheduled. Status SHALL be visible only to the owner and the affected peer. Quota rejection SHALL identify the limiting scope or scopes and their next renewal times, if scheduled.

#### Scenario: A quota denial identifies the relevant reset time
- **WHEN** a model exhausts its daily cap while its provider still has weekly capacity
- **THEN** the peer can see both quota statuses and the denial identifies the model cap and its next UTC renewal
