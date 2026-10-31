# LLM Cost Accounting Specification

## Purpose

Define consistent supported LLM token-cost estimates across provider attempts, prompt-cache categories, durable session reporting, auxiliary generation, and execution-cycle budgets, with explicit uncertainty instead of fabricated bills.

## Requirements

### Requirement: Usage categories do not overlap
Canonical usage SHALL distinguish uncached input, non-reasoning output, reasoning output, cache reads, and cache writes. Each token SHALL contribute to one top-level category per attempt. Reasoning SHALL be priced at the applicable output rate exactly once. Provider-normalized usage SHALL retain its meaning through local, plugin, storage, and remote paths.

#### Scenario: Reasoning output is included in a provider total
- **WHEN** a provider reports 1000 total output tokens including 900 reasoning tokens
- **THEN** canonical usage reports 100 non-reasoning output tokens and 900 reasoning tokens
- **AND** the output charge covers 1000 tokens, not 1900

#### Scenario: Cached input is included in a provider total
- **WHEN** a provider reports 1000 input tokens including 600 cache reads and 300 cache writes
- **THEN** canonical usage reports 100 uncached input tokens, 600 cache reads, and 300 cache writes
- **AND** the categories survive serialization without adding the cached counts to the input total again

### Requirement: Cache-write tiers partition aggregate writes
Returned cache-write tiers SHALL be a breakdown of aggregate writes, not additional tokens. The catalog `cache_write` rate SHALL price five-minute writes and writes without duration detail. Returned one-hour writes SHALL use an explicit one-hour rate; without one, only that portion SHALL be incomplete. One-hour rates SHALL NOT be synthesized, and contradictory tier data SHALL be reported as uncertainty.

#### Scenario: Writes priced at the catalog base rate
- **WHEN** usage reports 300 cache writes with no duration breakdown, or a breakdown of 300 five-minute writes, and the catalog provides a `cache_write` rate
- **THEN** the write charge is 300 tokens at the catalog `cache_write` rate
- **AND** the estimate can be complete

#### Scenario: Mixed-duration cache writes with a one-hour rate
- **WHEN** usage reports 300 writes comprising 200 five-minute and 100 one-hour writes and an explicit one-hour rate is configured
- **THEN** the write charge is 200 tokens at the base rate plus 100 tokens at the one-hour rate
- **AND** total input-token accounting includes 300 writes, not 600

#### Scenario: One-hour writes without a one-hour rate
- **WHEN** usage reports 100 one-hour writes and no explicit one-hour rate is available
- **THEN** accounting marks the one-hour write charge incomplete while retaining the base-rate five-minute charge and all other components
- **AND** it does not price the one-hour writes at the base rate

#### Scenario: Inconsistent breakdown
- **WHEN** the returned write-tier counts disagree with the returned aggregate write count
- **THEN** accounting exposes the discrepancy and does not claim a complete write charge
- **AND** it does not count both the aggregate and its purported breakdown

### Requirement: Each attempt binds its applicable pricing
Each attempt SHALL bind provider/model identity, billing classification, applicable pricing inputs, and pricing source to the provider that handles it. Historical charges SHALL NOT be recalculated when the session switches models or the pricing catalog changes. Routing or provenance that makes the selected price inapplicable SHALL make the affected valuation incomplete rather than silently price another model.

#### Scenario: Model changes between requests
- **WHEN** a session incurs a charge on model A and then switches to model B
- **THEN** later cumulative cost adds B's new charge to A's retained charge
- **AND** it does not price A's tokens using B's rates

#### Scenario: Catalog refresh after an attempt
- **WHEN** published rates change after an attempt is recorded
- **THEN** replay and exports retain the charge and pricing basis recorded for that attempt

#### Scenario: Provider reports a different effective model
- **WHEN** a routed attempt reports an effective model incompatible with its selected pricing snapshot
- **THEN** its valuation uses an authoritative applicable basis if available
- **AND** otherwise reports the affected cost as incomplete instead of asserting the configured model's price

### Requirement: Valuation exposes complete and partial amounts
Valuation SHALL expose component charges, a known subtotal, and completeness reasons. A missing rate SHALL matter only for a category with nonzero usage. Missing usage, required rates, or detected unsupported pricing dimensions SHALL NOT silently become zero charges. A complete amount SHALL mean a supported token-cost estimate, not a reconciled provider invoice.

#### Scenario: An unused category has no price
- **WHEN** cache-write usage is zero and the cache-write rate is unavailable
- **THEN** that missing rate alone does not make the estimate incomplete

#### Scenario: A used category has no price
- **WHEN** input and output can be priced but a nonzero cache-read category has no applicable rate
- **THEN** the known subtotal includes the input and output charges
- **AND** a complete total is unavailable with cache-read pricing identified as missing

#### Scenario: A pricing dimension is unsupported
- **WHEN** the request is known to use a pricing modifier or non-token charge that cannot be valued
- **THEN** accounting discloses the unsupported dimension
- **AND** does not label its supported subtotal as a complete bill

### Requirement: Zero and excluded billing remain distinct from unknown cost
Accounting SHALL distinguish known zero-priced usage, billing excluded by provider/authentication policy, and unknown prices. Existing subscription/OAuth exclusions SHALL NOT be replaced with catalog API charges. Exclusions SHALL be visible in accounting scope, and an entirely excluded session SHALL NOT be represented as a known zero-dollar provider invoice.

#### Scenario: Explicit zero rate
- **WHEN** an applicable rate is explicitly zero for reported usage
- **THEN** its charge is known zero rather than missing

#### Scenario: Subscription-backed generation
- **WHEN** provider/authentication policy excludes an attempt from metered API-cost estimates
- **THEN** usage is retained with an excluded billing classification
- **AND** catalog token rates are not charged or used for default API-budget enforcement

### Requirement: Numeric failures cannot masquerade as valid prices
Accounting SHALL reject negative or non-finite rates and SHALL prevent token or monetary overflow from producing wrapped, non-finite, or misleadingly complete amounts. Repeated valuation of identical usage and pricing SHALL return the same components and subtotal without accumulating cache charges again.

#### Scenario: Invalid rate
- **WHEN** a supplied rate is negative or non-finite
- **THEN** configuration is rejected or the affected external pricing is marked invalid
- **AND** no complete cost is published from that rate

#### Scenario: Repeated valuation
- **WHEN** identical input, output, and cache usage is valued twice
- **THEN** both results have the same charges
- **AND** the second evaluation does not add a second copy of cache cost

### Requirement: Attempts have durable accounting lifecycles
Before dispatch, each attributable attempt SHALL have durable identity and session/cycle ownership. Available usage and valuation SHALL be recorded before derived charges are published or a further paid attempt is dispatched. A dispatched attempt interrupted without final accounting SHALL remain unresolved, not disappear as zero spend. Later conversation-storage failure SHALL NOT erase an already recorded charge.

#### Scenario: Crash after dispatch
- **WHEN** a process stops after a durable attempt start but before its final usage is recorded
- **THEN** restored accounting identifies unresolved spend for that attempt
- **AND** does not fabricate a usage count or complete total

#### Scenario: Accounting persistence fails
- **WHEN** a received charge cannot be durably recorded
- **THEN** execution surfaces a storage failure and does not publish the charge as durable
- **AND** it does not dispatch another paid attempt until the persistence failure is resolved

#### Scenario: Assistant history cannot be stored
- **WHEN** accounting has been recorded but saving the generated assistant message fails
- **THEN** the charge remains in session and cycle accounting

### Requirement: Accounting is idempotent across delivery paths
One provider attempt SHALL contribute at most one settled charge. Repeated snapshots, final response delivery, event replay, and remote redelivery SHALL NOT create additional charges. Distinct dispatched retries SHALL have distinct identities. Conflicting settlements for one identity SHALL surface an accounting error rather than silently overwrite or add another charge.

#### Scenario: Remote settlement is delivered twice
- **WHEN** a recorded settlement is replayed and also arrives through a remote live relay
- **THEN** session and cycle totals include its charge once

#### Scenario: Distinct retry attempts report usage
- **WHEN** an unsuccessful attempt reports usage and a separately dispatched retry succeeds
- **THEN** both attempts' attributable usage is retained and valued independently
- **AND** retry output is not mixed with the earlier attempt's output

### Requirement: Session and cycle totals are separate additive scopes
Session cost SHALL accumulate recorded charges for that session across execution cycles and restart. Cycle cost SHALL include only attempts attributed to that cycle. Supported known subtotals SHALL be additive and nondecreasing as new charges settle, while completeness SHALL retain unresolved reasons. A new cycle SHALL reset cycle spend without resetting session spend.

#### Scenario: Reasoning followed by ordinary output
- **WHEN** a cycle first generates 100 ordinary output and 900 reasoning tokens, then 100 ordinary output and no reasoning, at a fixed output rate
- **THEN** its cumulative output charge covers 1100 tokens
- **AND** it neither repeats current reasoning nor omits the earlier 900 reasoning tokens

#### Scenario: Next user turn after restart
- **WHEN** a session with recorded spend is reopened and begins a new execution cycle
- **THEN** session cost retains prior charges
- **AND** the new cycle begins with zero cycle spend and accumulates only its own attempts

### Requirement: Conversation edits do not rewrite incurred spend
Pruning, compaction, history edits, and rewind SHALL NOT remove accounting for incurred attempts in a retained session. A fork SHALL start its own spend scope without charging copied conversation history again. Parent and child spend SHALL remain separate unless an explicitly labeled aggregate view is requested. Full session deletion remains outside this retention guarantee.

#### Scenario: Rewind removes a generated turn
- **WHEN** a session rewinds to before a turn that incurred recorded charges
- **THEN** the original session retains those charges even though the turn leaves effective conversation history

#### Scenario: Fork inherits an assistant response
- **WHEN** a new session inherits conversation content containing prior usage
- **THEN** inherited content does not create new charges in the fork
- **AND** the source session retains its original spend

### Requirement: Auxiliary attempts count under their initiating scope
Compaction and delegation-summary attempts SHALL use the same valuation and durable accounting as main generation. Their charges SHALL belong to their initiating session/cycle even if completion occurs later. Rejected generated summaries and usage-bearing unsuccessful attempts SHALL remain accountable. Auxiliary usage SHALL NOT replace the main conversation's current context measurement or increment its main-response step count.

#### Scenario: Compaction succeeds and reduces context
- **WHEN** a compaction attempt incurs charges and reduces effective conversation context
- **THEN** its charges increase the initiating session/cycle spend
- **AND** context utilization decreases independently

#### Scenario: Summary validation causes a retry
- **WHEN** a delegation-summary response reports usage but is rejected and retried
- **THEN** the rejected response and the new attempt are each accounted for once

#### Scenario: Delegated child also generates responses
- **WHEN** a child session incurs generation charges and the parent generates a delegation summary
- **THEN** the child retains its own generation charges
- **AND** the parent includes its summary charge without charging the child generation a second time

### Requirement: Missing attempt usage remains explicit
A dispatched potentially metered attempt with unavailable usage SHALL contribute an uncertainty reason, not fabricated tokens. Known pre-dispatch rejection SHALL NOT be treated as a paid attempt. A provider-confirmed non-billable rejection SHALL be distinguished from a transport failure with unknown billing consequences.

#### Scenario: Interrupted stream reports only partial usage
- **WHEN** a potentially metered stream fails with only partial usage available
- **THEN** the available supported charges are retained as a known subtotal
- **AND** final usage uncertainty remains visible even when a later retry succeeds

#### Scenario: Local validation rejects a request
- **WHEN** request validation rejects a request before provider dispatch
- **THEN** accounting does not add an unknown paid attempt for that rejection

### Requirement: Budgets use cycle accounting including cache charges
Price limits SHALL evaluate attributable cycle charges, including cache reads/writes and auxiliary attempts, before subsequent paid attempts. Manual pricing SHALL use the shared valuation rules and explicit cache rates without inferring them from ordinary input prices. Budget valuation SHALL NOT reprice prior attempts when model or manual-rate settings change.

#### Scenario: Cache and compaction reach the cap
- **WHEN** known main-response, cache, and compaction charges reach the configured cycle cap
- **THEN** the next paid attempt is blocked by the price limit

#### Scenario: Session already has prior-cycle spend
- **WHEN** a new cycle starts in a session whose prior cycles exceeded the new cycle's cap
- **THEN** prior-cycle charges alone do not exhaust the new cycle's budget

#### Scenario: Manual rates lack a used cache category
- **WHEN** manual input/output rates exist but a nonzero cache-write category has no configured rate
- **THEN** its budget valuation is partial rather than treating writes as zero or ordinary input

### Requirement: Incomplete budgets warn and continue
When cycle pricing is incomplete, execution SHALL warn and continue using the known budget subtotal, as a best-effort limit. Reaching the cap with known charges SHALL still stop further paid attempts. Warnings SHALL identify missing information without requiring acknowledgement or flooding each stream update. Accounting persistence failure SHALL remain an execution error, not a missing-price warning.

#### Scenario: Unknown cache price below cap
- **WHEN** a cache rate is missing and known cycle spend is below the cap
- **THEN** execution continues with a visible best-effort-budget warning
- **AND** reports retain the uncertainty instead of claiming the cap is strictly enforced

#### Scenario: Partial subtotal reaches cap
- **WHEN** accounting remains incomplete but its known subtotal reaches the cap
- **THEN** the next paid attempt is blocked

### Requirement: Reporting and exports share recorded accounting
Request events, session snapshots, live/replayed reports, and exports SHALL derive charges from the same recorded accounting. Complete totals SHALL include supported cache and auxiliary charges. Partial known subtotals SHALL be labeled and accompanied by uncertainty reasons. Unavailable costs SHALL NOT be silently summed as zero and then presented as complete totals.

#### Scenario: Export includes auxiliary spend
- **WHEN** a session export includes main responses and attributable compaction charges
- **THEN** its session cost matches the recorded session accounting
- **AND** auxiliary charges are included without fabricated assistant turns or duplicate main-response charges

#### Scenario: Export contains an unpriced attempt
- **WHEN** one exported attempt has unknown or partial cost
- **THEN** the export exposes the known subtotal and its incomplete status
- **AND** does not present the sum of priced attempts as a complete session total

#### Scenario: Old records lack a pricing basis
- **WHEN** retained history contains generation records without the data required by this accounting contract
- **THEN** reporting identifies that history as unaccounted rather than recomputing an exact historical cost from current rates
