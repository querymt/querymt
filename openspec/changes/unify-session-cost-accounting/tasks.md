# Tasks

Implementation follows `design.md` and the three delta specs. Preserve unrelated work, use the project dev environment when available, and write only to fresh test databases. No task authorizes migration of the user's existing session database, restarting user-run services, mixed-version adapters, or dashboard implementation in this repository.

## 1. Regression Baseline And Shared Valuation

- [ ] 1.1 Extend the existing transition/mock-provider tests to expose first-request reasoning overcount, loss of earlier reasoning on the next request, and missing request cache charges; verify the new assertions fail against current production accounting using deterministic injected pricing, not the user's model cache or a network fetch.
- [x] 1.2 Define the shared usage-aware component valuation and billing/completeness types around `crates/querymt/src/providers/types.rs`, documenting complete supported estimates versus invoices; verify tests price ordinary output plus reasoning once, uncached input, cache reads, aggregate writes, and mixed write tiers without counting a partition twice.
- [x] 1.3 Implement pure partial valuation and numeric validation; verify distinct tests for unused missing rates, used missing rates, explicit zero rates, contradictory/missing write details, known unsupported modifiers, negative/non-finite rates, checked token sums, overflow, and repeated valuation without repeated cache charges.
- [ ] 1.4 Extend registry pricing ingestion/cache serialization with applicable write-tier rates, source/applicability metadata, and recognized unsupported dimensions; verify recorded registry fixtures round-trip those details and missing tier rates remain missing rather than becoming global multiplier guesses.

## 2. Canonical Usage And Provider Boundaries

- [ ] 2.1 Extend canonical usage with optional typed write-duration detail and explicit absence/inconsistency semantics, documenting exclusive categories; verify serialization and context/telemetry totals include aggregate writes once while detail partitions do not add tokens.
- [ ] 2.2 Update affected canonical output, plugin/native DTO consumers, `querymt-remote` usage transport, and Python usage projections for the new detail; verify round-trip tests preserve missing versus zero tier counts and package-scoped checks catch construction-site changes without unrelated provider behavior changes.
- [ ] 2.3 Update Anthropic raw usage decoding and presence-aware streaming reconciliation while preserving its exclusive output/reasoning mapping; verify non-streaming/streaming mixed-TTL parity, retained start-event input/cache fields, absent thinking detail, and final ordinary output dropping from provisional 1 to 0.
- [ ] 2.4 Verify the documented OpenAI Chat/Responses, DeepSeek, Codex, and Google usage field shapes with focused recorded fixtures, changing only confirmed normalization gaps; verify explicit-zero cache misses, inclusive-cache subtraction, reasoning semantics, and attempt-local final usage, and record provider limitations in existing provider documentation.

## 3. Durable Attempt Lifecycle And Reducer

- [ ] 3.1 Add typed started/settled accounting events with operation/attempt/session/cycle identities, purpose, billing classification, applicable price snapshots, usage disposition, and optional manual-budget valuation; verify event serialization/durability classification and that payloads contain no prompts, reasoning text, credentials, or secret endpoints.
- [ ] 3.2 Implement an accounting recorder through the existing awaited ordered session stream, with durable start before dispatch and settlement before derived completion; verify injected journal failures prevent dispatch or further paid work, and an unsettled start remains visible after fresh-database reopen.
- [ ] 3.3 Implement the shared incremental accounting reducer; verify identical duplicates and remote redelivery do not add charges, settlements resolve starts, distinct retry attempts add independently, conflicting settlements fail explicitly, and invalid or excluded billing cannot create a complete fabricated total.
- [ ] 3.4 Add paginated journal hydration/catch-up and session-runtime ownership of the reducer, without new accounting tables or schema migration; verify reopening reproduces live components/reasons, hot-path updates process only new records, and session retirement releases the projection.
- [ ] 3.5 Document the attempt lifecycle, pricing snapshot, retry identity, uncertainty states, and journal durability limits alongside the implementation; verify examples correspond to event/reducer serialization tests rather than promising exact billing after an unobservable crash.

## 4. Main Generation And Monetary Scopes

- [ ] 4.1 Create explicit immutable execution-cycle/accounting handles at the current cycle boundary and retain session accounting independently from new cycle stats; verify a second cycle starts at zero cycle spend while retaining lifetime spend after both normal continuation and fresh-database reopen.
- [ ] 4.2 Integrate the recorder into actual main-provider retry/streaming/non-streaming attempt boundaries below output validation; verify local preflight rejection creates no paid attempt, usage-bearing failures remain priced, unconfirmed transport failures remain uncertain, cancellation preserves available partial usage, and retries keep separate accumulators.
- [ ] 4.3 Bind actual provider/model and billing policy at dispatch and validate effective provenance before settlement; verify model switching/catalog refresh cannot reprice earlier attempts, incompatible effective-model pricing becomes incomplete, and existing Codex/kimi-code/OAuth exclusions do not acquire catalog charges.
- [ ] 4.4 Replace monetary `AgentStats::update_costs` repricing and transition-specific arithmetic with views of recorded components; verify task 1.1 regressions pass and saving assistant history after settlement can fail without losing the recorded charge.
- [ ] 4.5 Derive request-end accounting from the logical operation's attempts and session reducer, retaining identifiers for downstream deduplication; verify retries contribute once to operation cost, cache charges are included, partial operation totals are not marked complete, and incomplete execution remains accounted without a normal request-end event.
- [ ] 4.6 Document session-versus-cycle spend and logical-operation-versus-attempt cost semantics in existing agent/API documentation; verify documented event fields and scope labels match the generated DTOs.

## 5. History Retention And Fork Isolation

- [ ] 5.1 Protect accounting events and required unaccounted-generation evidence in journal range deletion and history-edit/rewind paths while preserving explicit full-session deletion; verify fresh-database pruning, compaction, and rewind leave incurred totals intact without renumbering journal records.
- [ ] 5.2 Preserve fork behavior that copies only conversational history and excludes accounting ownership; verify inherited messages/usage create no fork charges, new fork generation has its own identities, and source/child costs are not charged to parent budgets.
- [ ] 5.3 Detect retained local generation lacking a new accounting basis as incomplete historical coverage without trusting old cumulative amounts or backfilling current rates; verify legacy-shaped fresh test fixtures are partial while empty sessions and forks containing only inherited history remain clean.
- [ ] 5.4 Document cost retention, inherited history, historical incompleteness, and rollback constraints; verify examples agree with history/fork fixtures and do not instruct users to delete charges or rewrite existing databases.

## 6. Auxiliary Generation

- [ ] 6.1 Pass an explicit initiating session/cycle recorder into compaction, including a maintenance-cycle handle for compaction outside a prompt; verify streaming/non-streaming compaction charges and cache usage settle even when summary validation rejects output and retries.
- [ ] 6.2 Pass explicit initiating ownership into delegation summarization and retain it through delayed completion; verify a summary settling after a new parent cycle starts charges its original cycle, and generated refusals/empty summaries retain available cost.
- [ ] 6.3 Separate auxiliary monetary/token purpose totals from main context and execution-step projections; verify compaction reduces effective context while increasing spend, summary usage does not replace main context usage, and delegated child generation is not charged twice to the parent.
- [ ] 6.4 Document auxiliary accounting scope and failure semantics in existing compaction/delegation documentation; verify every documented dispatch path has a recorder or an explicit non-attributable/non-metered classification rather than a silent no-op.

## 7. Cycle Budgets And Missing-Price Policy

- [ ] 7.1 Extend manual model/price-limit configuration with validated cache/tier rates and snapshot a separate manual budget valuation per attempt when configured; verify the shared calculator is used, changing manual rates does not reprice earlier budget charges, and API reporting retains its own applicable pricing basis.
- [ ] 7.2 Replace both budget middleware calculations with shared cycle-accounting checks and apply the same check before main, retry, compaction, and summary dispatch; verify cache/auxiliary charges can exhaust the cap, known partial spend still stops at the cap, and prior-cycle spend alone does not exhaust a new cycle.
- [ ] 7.3 Implement warn-and-continue for unavailable usage/rates with cycle/reason deduplication and existing notice/fallback channels; verify repeated snapshots do not flood warnings, missing context size does not hide them, normal in-flight attempts do not spam warnings, and journal failure remains an execution error.
- [ ] 7.4 Document best-effort enforcement, manual-rate scope, unsupported cache dimensions, exclusions, and possible in-flight overshoot; verify configuration examples parse and distinct budget tests cover manual missing-cache rates and subscription-excluded attempts.

## 8. ACP, Snapshots, Exports, And Client Contract

- [ ] 8.1 Add replacement accounting metadata alongside existing QueryMT usage metadata, including session/cycle known subtotals, nullable complete totals, scope, reasons, unresolved/excluded counts, and best-effort status; verify DTO serialization and generated backend/client types preserve explicit clearing without replacing existing token metadata.
- [ ] 8.2 Drive ACP live state, snapshot replay, and remote observation from the shared reducer, removing `Some`-only cost retention in both event paths; verify complete-to-partial clearing, all-excluded and mixed billing, duplicate delivery, cross-cycle totals, and restored/live snapshot equivalence.
- [ ] 8.3 Keep accounting updates independent of context occupancy and negotiated advisory capabilities; verify auxiliary cost changes use the last reliable main context when available, unknown limits do not fabricate usage updates, and clients without notice support receive no unsupported notice.
- [ ] 8.4 Update turn/ATIF export aggregation to associate main operation costs and auxiliary/unassociated attempts without double counting; verify exported session subtotals match the reducer, unknown costs do not become complete zero totals, failed attempts remain represented, and auxiliary charges do not fabricate assistant turns.
- [ ] 8.5 Document backend accounting metadata, breaking DTO/event changes, supported token-price dimensions, deployment/rollback constraints, and the required `querymt-desktop` display/invalidation follow-up; verify schema examples against serialization fixtures and leave dashboard implementation outside this repository.

## 9. Cross-Cutting Validation

- [ ] 9.1 Run an end-to-end fresh-database scenario combining mixed cache TTLs, reasoning followed by zero reasoning, model switching, auxiliary retry spend, a new cycle, and restart; verify runtime totals, cycle limits, ACP live/replay metadata, and export accounting agree with the recorded attempts.
- [ ] 9.2 Run failure-path integration through actual dispatch/ordered-journal/remote-relay paths for cancellation, settlement persistence failure, unresolved restart, duplicate settlement delivery, and rewind/fork; verify no further paid dispatch after accounting-storage failure, no duplicate charges, and no loss of incurred spend.
- [ ] 9.3 In the detected project dev environment, run smallest affected package `cargo check` before formatting, focused tests, formatting checks, and Clippy, then affected binding/remote and agent CI feature combinations with the dashboard fixture and native provider test features; record commands/results and remaining gaps without indiscriminate all-features builds or model downloads.
- [ ] 9.4 Validate the completed change against all three delta specs and review the final diff for duplicated formulas, unintended compatibility adapters, UI edits, live-database writes, or service manipulation; verify `openspec validate unify-session-cost-accounting --strict` passes and all required behavioral scenarios have a test or explicitly recorded validation result.

## Workflow follow-up

- Review the planning artifacts and explicitly start the apply workflow before any implementation.
- Coordinate desktop/client and plugin/remote upgrades for the breaking accounting contract; deployment and any access to user databases require separate authorization.
- Archive only after implementation, validation, and project review are complete.
