## ADDED Requirements

### Requirement: Provider grants filter discovery and execution

A Custom grant SHALL allow selected host providers, each with all currently and subsequently available models or a non-empty list of canonical model IDs. Quota configuration SHALL NOT grant model access. The host SHALL enforce selection on catalogs, contract queries, direct inference, and session-triggered inference. It SHALL use host-owned bindings and credentials and SHALL NOT silently fall back on policy denial.

#### Scenario: Only the selected provider and models are usable
- **WHEN** a peer can use one model on one provider and attempts catalog, contract, and chat operations for permitted and unpermitted pairs
- **THEN** only the permitted pair is advertised and executes, and denied calls never reach an upstream provider

#### Scenario: All models follows the host catalog
- **WHEN** the host adds a model to a provider shared with two peers, one with All models and one with a fixed selection that excludes the new model
- **THEN** the new model is shared only with the All models peer, subject to that peer's applicable quotas and the model's ability to enforce them

#### Scenario: Host provider configuration is authoritative
- **WHEN** a request for an allowed pair includes an override of a host-owned provider setting
- **THEN** the host rejects the override or uses the configured binding without changing the effective pair, endpoint, or credential

#### Scenario: Provider revocation stops an active stream
- **WHEN** the host revokes a grant while the peer is receiving a stream for the removed model
- **THEN** the host stops delivery, cancels the affected attempt at the next supported boundary, and still settles its usage without interrupting other peers' streams

### Requirement: Provider and model quotas apply together

Each peer SHALL have an independent allowance per provider and per model within that provider. The default is unlimited; finite caps SHALL be positive whole tokens expressed exactly in Mtok (1 Mtok = 1,000,000 tokens). Invalid or overflowing values SHALL be rejected. All models share the provider cap. Each attempt SHALL count once against every applicable finite cap, even if the other scope is unlimited.

#### Scenario: Session inference and direct inference share an allowance
- **WHEN** a peer spends tokens in a direct stream and then through a hosted session using the same model, and also calls a different allowed model on that provider
- **THEN** all attempts draw from the same provider balance, each model balance reflects only its own attempts, and no other peer's or provider's balance changes

#### Scenario: Provider and model limits are independently binding
- **WHEN** a request would exceed either a provider cap or a model cap, even if the other scope has capacity or is unlimited
- **THEN** it is denied for quota at the limiting scope and does not dispatch

### Requirement: Finite quotas can renew on UTC calendar boundaries

Each finite quota SHALL have one reset schedule: manual (the default), daily at 00:00 UTC, weekly on Monday at 00:00 UTC, or monthly on the first day at 00:00 UTC. The provider and model schedules MAY differ. Unused capacity SHALL NOT carry over. A new attempt SHALL use the windows current at reservation time; settlement SHALL remain in those windows even after renewal. The host SHALL NOT reopen a closed window for new reservations after restart or clock rollback.

#### Scenario: Recurring windows renew at UTC calendar boundaries
- **WHEN** the host reaches a configured daily, Monday weekly, or first-of-month boundary, including while offline
- **THEN** the applicable scope opens a new window with the full configured allowance and no carryover; other scopes keep their current windows until their own boundaries, and a restart or clock rollback does not reopen an old window

#### Scenario: A stream spanning renewal settles against its original windows
- **WHEN** an attempt has provider and model reservations and one or both windows renew before it finishes
- **THEN** the attempt settles into the original windows, and new attempts use the current balance at each scope

### Requirement: Finite allowances are reserved before upstream work

Before any attempt subject to a finite cap, including an auxiliary attempt, the host SHALL reserve a conservative input and enforced output bound in all applicable windows in one transaction, or reserve nothing and not dispatch. Policy denial, insufficient quota, and unsupported hard limits SHALL be distinct errors. Attempts with no finite caps SHALL still check access but SHALL NOT reserve tokens.

#### Scenario: Concurrent requests cannot overcommit quota
- **WHEN** two requests each fit individually but their combined reservations exceed the remaining allowance
- **THEN** at most one is dispatched and the other fails for insufficient quota before any upstream call, with no partial reservation left in a scope that had room

#### Scenario: Unbounded requests are not accepted on a finite grant
- **WHEN** a peer submits a finite request whose effective input or provider generation cannot be bounded
- **THEN** the host refuses the request before dispatch rather than treating either the provider or model cap as advisory

#### Scenario: Unlimited inference needs no token bound
- **WHEN** an otherwise allowed request has no reliable token bound and neither scope has a finite cap
- **THEN** it may dispatch without a quota reservation

### Requirement: Settlement uses host-observed attempt usage

The host SHALL settle each dispatched attempt once using its authoritative normalized token total. It SHALL release confirmed pre-dispatch failures and charge missing or incomplete finite usage at the reserved amount as estimated. Missing unlimited usage SHALL be reported as unknown. If reported usage exceeds the bound, the host SHALL record the actual usage, report a fault, and suspend finite dispatch for that binding until a bound can be enforced.

#### Scenario: Streaming and one-shot usage settle once
- **WHEN** a one-shot response reports usage and a stream produces cumulative usage and repeats delivery across reconnect
- **THEN** each applicable reservation settles once for the same normalized total of uncached input, output, reasoning, cache reads and aggregate cache writes, without summing repeated cumulative usage, adding cache-write tiers, or trusting client claims

#### Scenario: Cancelled work without final usage is not free
- **WHEN** a finite attempt is already dispatched and disconnects or is cancelled without complete usage
- **THEN** its conservative reserved amount becomes estimated usage even if the peer never receives output

### Requirement: Quotas survive restart and access edits

Reservations and usage SHALL survive restart. Editing access or the amount of an existing finite cap SHALL NOT reset the quota. Changing a scope between unlimited and finite or changing its schedule SHALL require an explicit owner reconfiguration that starts a new accounting period. Saved Custom quotas SHALL survive a switch to No access or Full access. Full access attempts SHALL NOT draw from Custom quotas.

#### Scenario: Restart and regrant do not top up an allowance
- **WHEN** the host restarts with a possibly dispatched attempt and the owner disables and reenables access to its provider or model within the same window
- **THEN** the unresolved attempt remains charged conservatively and neither applicable quota is reset

#### Scenario: Changing a cap preserves consumption
- **WHEN** the owner edits the amount of an existing finite provider or model cap without changing its schedule
- **THEN** the new amount applies to new attempts but settled usage and outstanding reservations remain in the same window; a reduction below consumption leaves no available capacity

#### Scenario: Changing a schedule requires explicit reconfiguration
- **WHEN** the owner tries to change an existing model cap from weekly to daily as an ordinary grant edit, then explicitly reconfigures that scope after outstanding attempts finish
- **THEN** the ordinary edit is refused, and the explicit reconfiguration opens a fresh daily allowance with the next UTC midnight as its renewal, retaining prior usage and leaving the provider schedule and balance unchanged

### Requirement: Owner resets apply to one quota scope

A reset, first finite cap, or reconfiguration to a new finite period SHALL target one provider or model scope and be refused while attempts at that scope are outstanding. It SHALL retain prior usage and leave other scopes unchanged. A reset without a schedule change SHALL NOT move the current renewal boundary.

#### Scenario: Reset waits for outstanding attempts
- **WHEN** the owner requests a model reset with an attempt outstanding and retries after it settles
- **THEN** the first reset is refused and the second opens a fresh model allowance without changing provider or other model usage, previous records, or the next scheduled renewal

### Requirement: Quota status identifies the relevant limits

For each applicable scope the host SHALL report the limit or unlimited, known and estimated usage, and outstanding reservations and available capacity for finite caps. It SHALL report finite period start and next renewal time when scheduled. Status SHALL be visible only to the owner and the affected peer. Quota rejection SHALL identify the limiting scope or scopes and their next renewal times, if scheduled.

#### Scenario: A quota denial identifies the relevant reset time
- **WHEN** a model exhausts its daily cap while its provider still has weekly capacity
- **THEN** the peer can see both quota statuses and the denial identifies the model cap and its next UTC renewal
