# Design

## Context

See `proposal.md` for motivation and the three delta specs for behavior contracts. The audit found these implementation constraints:

| Area | Current behavior | Source |
| --- | --- | --- |
| Canonical usage | Five aggregate counters; no cache-write duration or usage-completeness detail | `crates/querymt/src/lib.rs:222` |
| Pricing | Input/output calculation and a separate optional cache calculation | `crates/querymt/src/providers/types.rs:80` |
| Pricing ingestion | Deserializes the models.dev provider map directly, then serializes a local cache | `crates/querymt/src/providers/registry.rs:55` |
| Request events | Request cost ignores cache; cumulative output uses prior ordinary output plus current reasoning plus current billable output | `crates/agent/src/agent/execution/transitions.rs:1219` |
| Stats | Revalues accumulated counters using current rates; cache charges are added separately | `crates/agent/src/middleware/state.rs:37` |
| Budget checks | Both implementations independently value input/output and omit cache | `crates/agent/src/middleware/limits.rs:116`, `:433` |
| Cycle boundary | Constructs new default stats for every execution cycle | `crates/agent/src/agent/execution/mod.rs:320` |
| Billing exclusions | Codex, kimi-code, and qualifying OAuth-backed providers intentionally have no catalog API price | `crates/agent/src/session/provider.rs:955` |
| Auxiliary calls | Compaction and delegation summaries can report usage to tracing but return summary-only results to callers | `crates/agent/src/session/compaction.rs:95`, `crates/agent/src/delegation/summarizer.rs:94` |
| Durable ordering | An ordered per-session stream supports awaited persistence and source-event deduplication | `crates/agent/src/session_event_stream.rs:193`, `crates/agent/src/event_sink.rs:139` |
| ACP | Sums token categories but only replaces cost for `Some`; omitting a new cost retains the old one | `crates/agent/src/acp/shared.rs:1247` |
| Export | Adds absent request costs as zero and emits a total | `crates/agent/src/export/atif.rs:324` |
| History | Journal range deletion is indiscriminate; forks copy conversational events but not operational events | `crates/agent/src/session/sqlite_storage/event_journal.rs:205`, `crates/agent/src/session/sqlite_storage/session_store.rs:845` |

Existing main specs disagree: `anthropic-messages-protocol` still describes inclusive canonical output, whereas `openai-responses-protocol` already requires non-overlapping categories and the current Anthropic mapping subtracts reasoning. The Anthropic delta explicitly resolves that conflict. The existing item-aware streaming contract already requires authoritative snapshots and attempt-local isolation; this design implements those obligations without a redundant streaming capability.

User-selected policies are warn-and-continue for incomplete USD budgets and inclusion of auxiliary LLM calls. Repository constraints prohibit touching the user's existing database/services and prohibit adding legacy compatibility layers by default.

## Goals / Non-Goals

**Goals:**
- One pure valuation implementation, with shared replayable accounting reduction rather than formulas in each consumer.
- Bounded hot-path work: value each attempt once and incrementally reduce new records; hydrate retained session accounting once per session-runtime load.
- Make uncertainty survive restart, retries, model switching, remote delivery, and exports.
- Preserve current context/step semantics while adding independent monetary scopes.

**Non-Goals:**
- A new external billing service, distributed transaction protocol, or replacement event transport.
- Exact invoice reconciliation, historical rate reconstruction, discounts/credits reconciliation, server-tool fees, or implementing cache storage/time billing.
- Changing cache enablement, TTL defaults, breakpoints, or provider request caching strategy.
- Strict spend reservation against parallel or in-flight requests; a per-cycle cap remains a best-effort pre-dispatch check.
- A new parent-plus-descendants budget or dashboard implementation in this repository.

## Decisions

### 1. Keep exclusive aggregate usage and add optional typed detail

Keep the five top-level categories, explicitly documenting their non-overlap. Add optional typed cache-write detail with optional five-minute and one-hour counts, plus identifiable inconsistency/missing-detail information. Absence must differ from an explicit zero. Tier counts partition `cache_write`; they are never included again in context or telemetry totals. Distinguish final, partial, and unavailable attempt usage in accounting, rather than treating default-zero counters as proof of a complete response.

Preserve detail through canonical output, plugin/native boundaries, message serialization, Python projections, and `querymt-remote` usage transport. Update construction sites mechanically where necessary, but do not change unrelated native-provider behavior or trigger model downloads for a DTO edit.

Provider adapters own wire normalization. Reuse the existing OpenAI/DeepSeek normalization where it is correct, including explicit zero cache-miss counts. Fix only documented discrepancies in Google and other paths, with recorded wire fixtures. Do not infer unsupported counters from provider names or requested cache configuration.

For Anthropic, accumulate raw, presence-aware usage fields before normalization, then use authoritative final inclusive output/reasoning values. An omitted input field in a later delta retains earlier input data; an explicitly reported zero is a replacement. Do not run `merge_max` over independently normalized ordinary-output values: final reasoning may reduce ordinary output below its provisional count. Preserve duplicate-event idempotence and avoid changing generic merge semantics for unrelated providers.

**Alternative rejected:** provider-aware arithmetic in shared billing would spread normalization policy across consumers and leave limits/context inconsistent.

### 2. Value usage with a pure typed result, not mutable totals

Extend `ModelPricing` and its ingestion to represent the cache-read and cache-write rates the models.dev `cost` schema actually carries (`input`, `output`, `reasoning`, `cache_read`, `cache_write`, audio rates, and `tiers`/`context_over_200k` context tiers), plus pricing applicability and source metadata. Preserve recognized extra pricing dimensions when loading and reserializing the registry instead of silently dropping information needed to identify an unsupported modifier.

The catalog has no per-TTL cache-write rates. Its `cache_write` is the provider's base write price (for Anthropic, the five-minute price), so it prices five-minute writes and writes without a duration breakdown. The only additional rate is an optional explicit one-hour write rate, available solely through manual configuration (or a future catalog field). Nonzero one-hour writes without that rate make only the one-hour portion incomplete. Do not synthesize the one-hour rate from a universal multiplier. Missing trustworthy rates produce partial valuation, not a network dependency in the request path.

Introduce a pure valuation result with:
- USD component amounts for ordinary input, output including reasoning, cache reads, base-rate cache writes, and one-hour cache writes.
- A finite nonnegative `known_subtotal_usd` and optional `complete_total_usd`.
- Completeness reasons such as missing usage, missing rate (including a missing one-hour write rate), contradictory write detail, invalid pricing, unsupported known modifier, or unresolved attempt.
- Billing scope/classification that distinguishes metered API, explicitly zero-priced, excluded subscription/OAuth, and unknown.

Conceptually, with rates per million tokens:

```text
input       = uncached_input * applicable_input_rate / 1e6
output      = (ordinary_output + reasoning) * applicable_output_rate / 1e6
cache_read  = cache_read_tokens * applicable_cache_read_rate / 1e6
cache_write = (cache_write - one_hour_writes) * cache_write_rate / 1e6
            + one_hour_writes * one_hour_write_rate / 1e6   # one-hour term only when reported
known_subtotal = sum(independently known components)
```

Writes without a breakdown, or with an absent one-hour count, are valued entirely at the base rate; a reported one-hour count of zero needs no one-hour rate. For contradictory tier counts, do not claim a write charge derived from a purported partition is independently known. Preserve unrelated known components. Required rates depend on nonzero usage; a zero-token component is known zero without needing a rate.

Use validated finite nonnegative `f64` amounts consistent with current USD DTOs, widened/checked token arithmetic, deterministic event order, and tolerance-based tests. Do not round each request to cents. Overflow or invalid external rates become explicit invalid valuation; invalid manual configuration fails validation. This is not an accounting-grade fixed-point currency subsystem.

Remove the competing agent aggregate-token repricing paths. Existing input/output helpers can remain only where their documented scope is explicitly input/output-only and they cannot be mistaken for a complete session cost; do not add a legacy cost-calculation adapter.

**Alternative rejected:** adding cache cost to the existing cumulative expression still reprices history and cannot express missing categories, mixed TTLs, or exclusions.

### 3. Bind pricing and optional manual budget valuation to each attempt

Create an immutable accounting context when an actual provider attempt is prepared. It carries operation ID, attempt ID, initiating session/cycle IDs, purpose, requested provider/model, billing classification, and the applicable pricing snapshot/source. Use UUIDs already available in the agent. Capture endpoint/routing identity only in a non-secret form; never persist credentials, complete endpoints with secrets, prompt text, or reasoning content for accounting.

If canonical provenance reports another effective provider/model, validate applicability. Bind a trustworthy applicable effective pricing basis before settlement when available; otherwise mark that valuation incomplete. Never silently apply rates for a known-inapplicable configured model.

Default budgets use the same catalog-valued components as reported API spend. Preserve explicit manual-rate budget configuration as a separately labeled budget valuation using the same pure calculator and the same usage record. Snapshot its rates at dispatch, including cache/tier rates, and retain the resulting budget components in settlement. This prevents manual limits from silently replacing reported provider charges and avoids repricing earlier attempts when overrides change. No manual override reclassifies subscription-excluded usage as metered API spend in this change.

**Alternative rejected:** computing all historical tokens at the current manual or catalog rate conflates a hypothetical estimate with incurred supported charges.

### 4. Use durable start and settlement events in the existing journal

Add durable accounting events with stable operation/attempt identity:
1. `LlmAttemptStarted`: ownership, purpose, billing class, catalog/manual pricing contexts, and dispatch intent.
2. `LlmAttemptSettled`: final/partial/unavailable usage, disposition, pricing applicability, and provider/budget valuation results.

The exact Rust DTO names are implementation details; the two-stage lifecycle is intentional. Await the existing ordered persistence API for start before network dispatch. Local preflight failures do not create paid attempts. If dispatch is abandoned after a start, settle it as not sent when that fact is known; after a crash an unmatched start is conservatively unresolved, not evidence that payment definitely happened.

On response/error/cancellation, settle available attempt accounting before returning validated display content, storing assistant history, retrying, or publishing derived completion totals. Keep the recorder outside cancellation-selected futures so task cancellation runs explicit settlement where possible. A crash may prevent settlement, but the durable start survives. Record available partial usage for interrupted streams; a provider-confirmed non-billable rejection differs from an ambiguous transport failure.

Reuse session sequencing, durability classification, batch journaling, and remote origin/source deduplication from current code and `ordered-session-events`. Do not introduce another writer or remote pricing authority. The node performing the generation settles once; attached nodes only replay/reduce its events. A retried settlement append uses the same attempt identity. An identical duplicate is a no-op in the reducer; a contradictory settlement is an error. A start is resolved by its settlement, not added as a second charge.

Persistence errors are fatal to continued paid dispatch, even though missing-price errors warn and continue. Never silently proceed after a journal append fails. Do not claim durability stronger than the existing journal's WAL/synchronization guarantees. No event-based design can guarantee exact provider charges after an unobservable crash; unresolved-attempt metadata makes that limit explicit.

**Alternative rejected:** success-only end events lose rejected summaries, failed attempts, and dispatches interrupted by process death.

### 5. Share an incremental reducer for runtime, replay, and export

Implement one accounting reducer over started/settled records. It tracks identities, pending attempts, known component sums, final missing reasons, exclusion counts, and both catalog/provider and optional budget views. Apply an attempt's settled components once. Pending reasons are removed when their settlement arrives; settled incompleteness remains unless an explicit future reconciliation feature is introduced. No replacement valuations or retrospective repricing are included here.

Hydrate a session-owned accounting handle from its journal on runtime activation, then feed it new committed records. Retain it across execution cycles; release it when the session runtime retires. Use paginated journal reads and a cursor for hydration/catch-up rather than rescanning on each token/request. This design needs no new SQLite accounting tables or database schema migration: the existing JSON journal stores new event kinds, and the reducer is a rebuildable in-memory projection. A persisted projection optimization would be a separate measured change.

Create an explicit cycle ID at the execution-cycle boundary and carry an immutable handle through main requests and auxiliary tasks. Delayed summaries retain their initiating cycle rather than reading whichever cycle is currently active. Budget checks read that cycle's committed accounting, not session lifetime totals. Serialize check/start bookkeeping through the session accounting handle without holding its lock across provider generation. Parallel in-flight operations can still overshoot a cap; no reservation algorithm is promised.

`AgentStats` continues to expose execution/token stats where callers rely on them, but monetary fields become a view of recorded components rather than `update_costs(current_pricing)`. Auxiliary tokens have separate purpose totals and do not overwrite main `context_tokens`, main usage, or main-response step counts. Replaying starts/settlements alone must be sufficient to reconstruct cost; `LlmRequestEnd` costs are derived presentation, never additional ledger entries.

**Alternative rejected:** initializing cycle stats from all history would accidentally turn existing scheduled/per-cycle caps into lifetime caps.

### 6. Preserve accounting when conversation history changes

Exclude accounting lifecycle events from conversation-tail/range deletion in `EventJournal::delete_session_events_from` and any history rewind path; retain their sequence gaps as valid journal history. Do not renumber retained records or rebuild spend from visible assistant messages. Apply the same retention rule to unaccounted-attempt evidence needed to avoid claiming historical completeness. Full explicit session deletion may remove the session and its accounting; this is not indefinite financial-data retention.

Keep accounting events excluded from fork copying. The current fork implementation already copies only selected conversational events: preserve this behavior explicitly when adding the new event kinds. A fork starts its own monetary scope even if copied messages contain usage. Parent/child aggregation, if consumed by an existing export, must be explicitly labeled and deduplicated by ownership/attempt identity; it is not parent budget spend.

For retained histories predating the new contract, identify local generation records with no corresponding accounting basis as unaccounted coverage. Inherited conversation events are not local generation. Do not trust old cumulative fields, backfill historical rates, or migrate old records. Empty sessions and forks with only inherited history can start with clean accounting; an old session with unaccounted local generation cannot claim a complete lifetime total. This is disclosure of absent evidence, not a compatibility valuation adapter.

### 7. Integrate accounting below output validation, including auxiliary attempts

Thread the recorder into `llm_retry`, streaming/non-streaming request execution, compaction, and delegation summarization at the actual dispatch/attempt boundaries. Output validation comes after available usage is captured: a summary refusal, length limit, empty summary, or failed validation can incur provider cost. Retry attempts have separate accumulators and accounting identities under one logical operation.

Compaction and summarizer APIs must carry an explicit initiating accounting context; do not infer ownership from the first history message or from mutable global session state. Keep recorder output independent of summary text so callers do not need to fabricate chat turns to record charges. Compaction outside a prompt uses a dedicated explicit maintenance cycle with the same identity rules; it has a cap only when one is actually configured. A summary launched by a parent is charged to that parent cycle; delegated child execution stays on the child.

For a logical main request with retries, `LlmRequestEnd.cost_usd` represents the complete supported cost across that operation's attempts, not just the final successful attempt. Partial subtotals live in the accompanying accounting representation. If execution ends before a normal request-end event, durable attempt records still carry its charges and uncertainties. Export operation links prevent adding those records and the derived request cost twice.

### 8. Budgets warn on missing prices, not on accounting-storage failure

Both limits middleware paths consume cycle budget valuation from the shared handle. Extend `ModelInfoSource::Manual` and price-limit configuration with explicit cache-read, cache-write, and optional one-hour cache-write rates and input validation, or route both through an equivalent typed pricing configuration. Preserve per-cycle scope and existing price-limit outcomes. Do not apply uncached input rates to cache tokens as an implicit fallback.

Before the next paid main/auxiliary attempt, stop when known cycle spend reaches the cap. Otherwise allow incomplete budgets to continue with a deduplicated warning that enforcement is best-effort. Deduplicate by cycle and material reason set; new missing categories can produce a new warning, repeated streaming snapshots cannot. Ordinary active in-flight attempts are pending, not a reason to spam price warnings; interrupted or settled-incomplete attempts and uncertain prices are reportable conditions.

Use existing advisory capability gating and user-visible fallback channels, not permission requests. A missing context limit does not hide budget uncertainty. New cycles get independent budget totals; delayed charges remain attributed to their original cycle.

### 9. Publish replacement accounting state and export the ledger once

Preserve `_meta.querymt.usage` v1, including its current main-request token scope and JSON shape. Its latest-request and session token totals are telemetry, not the all-attempt monetary ledger. Do not silently broaden them to include auxiliary/retry attempt tokens or replace them with accounting token sums; any future all-attempt token display needs an explicitly labeled scope.

Add `_meta.querymt.accounting` version 1 alongside that token metadata. Use one typed replacement DTO for live updates, remote/replayed projections, and an authoritative `accounting` field in the existing `SessionLoadSnapshot`. It contains API-spend scope, session and identified active-cycle known subtotals, nullable complete totals, completeness state/reasons, unresolved/incomplete/excluded counts and historical coverage. Represent no active cycle explicitly rather than guessing from the last attempt to settle; delayed auxiliary settlement must not select an old cycle as active. The cycle also exposes separately labeled budget valuation, budget-specific reasons, and best-effort-budget status so manual budget rates do not masquerade as API charges. Complete totals are explicitly null when unavailable, never omitted by DTO serialization or generated types.

Presence of the accounting payload replaces the previous accounting state atomically, including explicit nulls. Absence of that payload means the message does not update accounting; omission of standard ACP `cost` alone is not a QueryMT clearing signal. Read and serialize totals from the shared reducer, not from request-end convenience costs. Pending attempts may temporarily prevent a complete total but are distinct from settled missing-price/usage reasons and do not alone trigger budget warnings.

Populate standard ACP `cost` only from a complete session metered-token estimate. An all-excluded session has no standard dollar total; explicit zero-priced metered usage remains distinct. Mixed sessions disclose that the amount covers metered API attempts and excludes subscriptions. These are supported estimates, not reconciled invoices. Extend both the normal projection and snapshot event paths; remove the `Some`-only update behavior.

Use `usage_update` as the primary carrier when reliable main-context occupancy and the active model's effective limit are available. Accounting-only changes reuse the last reliable main measurement without replacing it with auxiliary tokens. Validate or invalidate that measurement across model switches rather than combining a stale reading with an unrelated limit. If no reliable measurement/limit pair exists, emit metadata-only `session_info_update._meta.querymt.accounting` with the identical DTO; do not fabricate zero/estimated context or reuse an obsolete limit. Accounting state delivery is independent of notice capability. This fallback does not change the title or activity timestamp solely to carry accounting. Budget notices remain separately capability-gated and deduplicated.

Include authoritative reducer-derived accounting directly in load snapshots even when historical notification replay is disabled or context is unknown. Associate it with the existing snapshot cursor and preserve ordering through the existing local/remote cursor machinery. Applying captured live records after the snapshot must not double count, and an older snapshot must not overwrite newer accounting. Do not introduce a second monetary reducer in the desktop.

Snapshots and remote replay reduce the same records as live handling. Request-end fields remain convenience projections, not accounting inputs for newly recorded attempts. In exports, associate main operation costs with turns and expose auxiliary/unassociated attempt records and session totals separately. ATIF totals must consume one accounting scope and expose incomplete totals as unavailable plus known-subtotal metadata; never use `unwrap_or(0)` to imply complete spend. Do not invent assistant turns for auxiliary charges. Preserve uncertainty when the chosen external export format cannot express all components, using its supported extension mechanism or an explicit omission.

Update generated types and affected Python projections in this repository. Generate types with `PATH=/Users/wiking/.cargo/bin:$PATH ./scripts/generate-types.sh` and verify explicit-null serialization fixtures against generated types.

Document the required `querymt-desktop` follow-up, but do not edit UI implementation here. Both live/replay notification handlers and snapshot hydration must consume authoritative accounting, distinguish an absent payload from a present null complete total, and replace state atomically. `mergeMissingSessionUsage` must not interpret an intentionally cleared cost as a hole to refill from an older candidate; merge precedence must honor existing snapshot/live cursors. Do not derive monetary totals from `llm_request_end` history. Read the same accounting DTO from either update carrier, and avoid scheduling catalog refreshes for accounting-only `session_info_update` messages. Preserve current token-history telemetry separately. Use labels such as "Estimated API cost", "Known API subtotal - incomplete", and "Subscription usage excluded"; show manual-rate cycle budgets separately. Release the desktop follow-up with the backend contract change.

## Risks / Trade-offs

- [Broad DTO change] -> Coordinate core/provider/plugin/remote/binding updates and run serialization tests; mark source/wire changes as breaking instead of layering legacy adapters.
- [Catalog lacks a one-hour write rate] -> Price base-duration writes with catalog `cache_write`; report only one-hour writes as partial unless a manual one-hour rate is configured. Missing modifier rates likewise report partial valuation; warn-and-continue is intentional and does not guarantee a strict cap.
- [Extra persistence at attempt start] -> Reuse ordered batching and perform no per-token writes; test latency with existing event infrastructure if the added boundary is measurable.
- [Unbounded historical identity state] -> Retire projections with session runtimes, hydrate in pages, and avoid copying full events into the reducer; measure before proposing persisted compaction/indexes.
- [Crash between intent and dispatch] -> Report unresolved spend conservatively; never claim that an unmatched start proves a provider charge or zero cost.
- [Cancellation drops usage] -> Capture attempt-local usage before validation and finalize outside cancellation selection; retain pending evidence when final usage is unavailable.
- [Old sessions cannot have exact lifetime totals] -> Explicit unaccounted coverage with no historical repricing/backfill.
- [History deletion erases accounting evidence] -> Protect accounting and coverage evidence in range-deletion/rewind tests while retaining explicit full-session deletion semantics.
- [Overlapping event-stream work] -> Reuse the current awaited per-session API and confirm behavior against `ordered-session-events`; do not alter its ordering or transport contracts.
- [Legacy clients retain stale display cost] -> Publish replacement metadata, mark the client contract change, and require coordinated client updates; standard omitted cost alone cannot clear every external client's cache. Preserve intentional nulls through load/snapshot merges and use existing cursors to reject stale accounting.
- [Accounting-only metadata causes UI refresh churn] -> Document that desktop catalog refreshes depend on actual session-summary changes, not merely receipt of a metadata-only `session_info_update`.

## Migration Plan

1. Review the breaking usage/pricing/event/metadata contracts in this change before implementation; use the implementation request as approval of those documented changes, not approval to rewrite user data.
2. Implement and validate against new in-memory and fresh temporary SQLite databases. Store accounting in the current journal shape; no schema migration or automatic historical rewrite is part of this design.
3. Upgrade backend, affected plugins/native providers, remote peers, bindings, and clients together. Do not promise mixed-version accounting or add negotiation shims in this change.
4. Existing readable sessions without accounting evidence report incomplete historical coverage. New sessions start clean. Do not fill missing historical charges from today's model catalog.
5. For rollback, use a separately preserved pre-upgrade database copy and matching binaries; do not run an older event decoder against a journal containing new event variants or delete accounting events to make rollback succeed. Preparing any copy of user data or deploying this change requires separate user authorization; this planning/apply work uses test databases only.
6. Document supported token-price dimensions, missing-rate warnings, per-cycle versus session scope, non-strict in-flight caps, subscription exclusions, and required desktop follow-up.

## Validation Strategy

Use production entry points and existing test fixtures to protect distinct behaviors, not a second implementation of pricing in tests. First expose the reasoning/cumulative regression with mock provider requests, then add the shared calculator and test cache/tier/uncertainty boundaries. Cover a first reasoning response followed by zero reasoning, explicit-zero rate/counter cases, final ordinary output below provisional output, mixed TTLs, partial rates, and invalid arithmetic.

Exercise start/settlement persistence, duplicate/conflicting events, journal failure before dispatch, interrupted/cancelled attempts, response-storage failure, auxiliary validation retries, delayed cycle attribution, model switching, multi-cycle restart, rewind/fork isolation, and remote delivery with fresh databases. Assert that main context/steps stay independent from auxiliary usage. Compare live ACP snapshots, replay, budget state, and exports from the same recorded fixture, including complete-to-partial invalidation and all-excluded billing. Add serialization fixtures for both update carriers and the load snapshot, unchanged usage v1 token scope, distinct API/budget reasons, unknown-context accounting without notices, model-switch context invalidation, and snapshot/captured-live cursor precedence without stale-cost resurrection. Backend fixtures specify the separate desktop acceptance contract; they do not claim desktop implementation is complete.

Detect Nix and use the project dev environment when available. After edits, run package-scoped `cargo check` before formatting, focused tests with `cargo nextest run`, formatting checks and Clippy; expand to affected bindings and agent `oauth`/`remote`/`dashboard` combinations from CI. Use the dashboard fixture from `.github/workflows/test-agent.yml` and native provider features rather than default Extism test linking. Do not run indiscriminate all-features or model-download tests. Report commands actually run and remaining gaps. No code builds are required for these planning artifacts.
