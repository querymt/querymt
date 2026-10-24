# Proposal

## Why

Cumulative-cost reporting counts current reasoning twice, omits prior reasoning and cache charges, and resets across execution cycles; other paths independently reprice accumulated tokens using the latest model. A shared, durable accounting contract is needed so reports, exports, and per-cycle budgets agree about main and auxiliary LLM spend without claiming incomplete estimates are complete bills.

## What Changes

- Price normalized, non-overlapping usage once per provider attempt, with a component breakdown, pricing snapshot, billing classification, known subtotal, and explicit completeness reasons.
- Preserve prompt-cache details needed for pricing, including Anthropic 5-minute and 1-hour writes, without counting write-tier details in addition to aggregate writes. Missing rates or unsupported pricing dimensions remain explicit, not zero-valued charges.
- Persist attempt-level accounting through the existing ordered session event infrastructure, with idempotent aggregation and separate session-lifetime and execution-cycle totals. Model/rate changes never reprice prior charges.
- Include compaction and delegation-summary attempts, including usage-bearing unsuccessful attempts. Attribute child-session spend only to the child and auxiliary spend to its initiating session/cycle.
- Make budget middleware, ACP live/replay state, request events, and exports consume the same accounting results. Preserve per-cycle budget scope; warn and continue against known subtotals when accounting is incomplete, as selected by the user.
- Preserve costs through compaction, history edits, rewind, restart, replay, and remote redelivery. Inherited fork history does not create new charges.
- **BREAKING**: extend canonical usage/pricing/accounting DTOs and event contracts as needed, and replace aggregate-token repricing APIs with request-based accounting. Clients/plugins/remote peers must use the coordinated new contracts; no legacy adapters or historical cost backfill are planned.
- Reconcile the existing Anthropic spec's inclusive-output wording with the already-corrected exclusive output/reasoning mapping. Clarify that ACP standard cost is only a complete supported token-cost estimate; partial subtotals and uncertainty are reported separately.

## Capabilities

### New Capabilities

- `llm-cost-accounting`: normalized usage valuation, cache pricing, billing classification, durable attempt attribution, session/cycle accumulation, auxiliary spend, budget behavior, and consistent exported accounting.

### Modified Capabilities

- `anthropic-messages-protocol`: make output/reasoning categories exclusive, preserve returned cache-write TTL details, and require authoritative final streaming usage without normalized-max artifacts.
- `acp-session-updates`: publish durable session-wide accounting consistently during live delivery and replay, distinguish partial subtotals from complete cost, invalidate stale complete values, and preserve context/cost separation.

## Impact

- Core: `crates/querymt/src/lib.rs`, provider pricing types and registry loading, canonical output/stream accumulation, and serialization consumers.
- Providers: Anthropic usage decoding, shared OpenAI-compatible normalization, and confirmed discrepancies in other provider paths; preserve existing OAuth/subscription exclusions in agent pricing resolution.
- Agent: request/retry execution, stats and both price-limit middleware paths, manual pricing configuration, compaction, delegation summarization, ordered durable events and SQLite projections, history/fork/rewind handling, ACP snapshots/replay, and exports.
- Transport and bindings: remote provider usage transport, event relay, generated client types, and affected language bindings. Reuse existing dependencies and event ordering/deduplication; do not replace the event transport being delivered by `ordered-session-events`.
- Scope is supported LLM token-cost estimates, not invoice reconciliation, new cache-management behavior, server-tool fees, or cache storage/time billing. Detected unsupported pricing dimensions must be disclosed as incomplete accounting.
- Dashboard implementation remains in `querymt-desktop`; this change defines the backend payload and records required UI follow-up.
- Development and validation use fresh test databases only. Do not modify, migrate, replace, or delete the user's existing session database or restart user-run services. Any required deployment compatibility or database upgrade policy must be reviewed before implementation; this proposal does not authorize a migration of existing user data.
