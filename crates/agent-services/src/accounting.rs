//! Durable LLM-attempt accounting records.
//!
//! Every attributable provider attempt has a two-stage lifecycle recorded in
//! the session event journal:
//!
//! 1. [`AttemptStart`] (`LlmAttemptStarted`) is appended *before* dispatch. It
//!    binds ownership, purpose, billing class and the pricing snapshots that
//!    apply to the attempt.
//! 2. [`AttemptSettlement`] (`LlmAttemptSettled`) is appended once usage (or
//!    its absence) is known, before derived completion events are published.
//!
//! A start without a settlement is *unresolved*: it is not evidence that a
//! charge happened, nor that the cost was zero. Valuations are supported
//! token-cost estimates, not provider invoices.
//!
//! Payloads deliberately contain only identifiers, enums, token counts, rates
//! and USD amounts. They never carry prompts, reasoning text, credentials, or
//! endpoint URLs; routing identity is limited to [`non_secret_route`].
//!
//! # Lifecycle
//!
//! ```text
//! begin(start)  -> LlmAttemptStarted  (awaited; dispatch only after Ok)
//!   ... provider call; each retry is a new attempt ...
//! settle(..)    -> LlmAttemptSettled  (awaited; before LlmRequestEnd / history)
//! ```
//!
//! [`AttemptRecorder`] writes through the session's awaited ordered stream. If
//! a start cannot be persisted nothing is dispatched. If a settlement cannot
//! be persisted it is retained and re-appended, with the same attempt
//! identity, before any further attempt may begin; persistence failure is an
//! execution error, never a missing-price warning.
//!
//! # Identity and retries
//!
//! `attempt_id` is the idempotency key. Each dispatched retry has its own
//! `attempt_id` (and `attempt_index`) under one `operation_id`, so a rejected
//! attempt and the retry that succeeds are valued independently. `cycle_id`
//! is the initiating execution cycle and is kept even if the attempt settles
//! after a newer cycle starts. [`AccountingReducer`] ignores identical
//! duplicates (replay, remote redelivery) and rejects a contradictory record
//! for the same attempt with [`AccountingConflict`].
//!
//! # Pricing snapshot
//!
//! The start carries the catalog pricing (and an optional separate manual
//! budget pricing) bound at dispatch. The settlement carries the resulting
//! valuations, so replay never reprices: a model switch or catalog refresh
//! cannot change a recorded charge. Excluded (subscription/OAuth) attempts
//! have no pricing snapshot and never acquire catalog charges.
//!
//! # Uncertainty states
//!
//! * start without settlement: *unresolved* (not proof of a charge, nor of
//!   zero cost);
//! * `Partial` / `Unavailable` disposition: known subtotal only, no complete
//!   total; the reason persists even if a later retry succeeds;
//! * `NotSent` / `NotBillable`: resolved with no charge and no uncertainty;
//! * incomplete valuation (missing rate, unsupported dimension, ...): the
//!   valuation's reasons are retained;
//! * an all-excluded scope has no dollar total (not a known zero invoice).
//!
//! # Durability limits
//!
//! The journal is only as durable as its SQLite settings (file databases use
//! WAL with `synchronous=NORMAL`, see `docs/docs/agent/event-ordering.md`).
//! A crash after dispatch but before settlement leaves an unresolved start;
//! after a power loss the most recent commits can be lost. Accounting is a
//! supported token-cost estimate and cannot promise exact billing after an
//! unobservable crash.
//!
//! # Projection
//!
//! [`SessionAccounting`] hydrates a reducer from the journal in pages when the
//! session runtime first needs it, applies each newly committed record
//! incrementally, and is released with the runtime. It needs no accounting
//! tables or schema migration.
//!
//! # Example
//!
//! A start record as stored in the journal (the `documented_start_example`
//! test pins this JSON to the real serialization):
//!
//! ```json
//! {
//!   "type": "llm_attempt_started",
//!   "data": { "start": {
//!     "attempt": { "operation_id": "op-1", "attempt_id": "att-1",
//!                  "attempt_index": 0, "session_id": "sess-1",
//!                  "cycle_id": "cycle-1", "purpose": "main_response" },
//!     "mode": "streaming", "provider": "anthropic", "model": "claude-sonnet",
//!     "route": "https://api.example.com", "billing": "metered",
//!     "pricing": { "input": 3.0, "output": 15.0,
//!                  "cache_read": 0.3, "cache_write": 3.75 }
//!   } }
//! }
//! ```

mod budget_warning;
mod cycle;
mod dispatch;
#[cfg(test)]
mod failure_tests;
mod pricing;
mod projection;
mod recorder;
mod reducer;
#[cfg(test)]
mod retention_tests;
mod snapshot;

pub use budget_warning::{BudgetWarnings, budget_warning_message};
pub use cycle::CycleAccounting;
pub use dispatch::{
    ActiveAttempt, AttemptOperation, DispatchLedger, PricingLookup, accounting_llm_error,
    classify_error, merge_usage, run_attempt,
};
#[doc(hidden)]
pub use pricing::is_non_billable_oauth_provider;
pub use pricing::{pricing_for, resolve_billing_basis};
pub use projection::{PAGE_SIZE, SessionAccounting};
pub use recorder::{AccountingError, AttemptHandle, AttemptRecorder};
pub use reducer::{
    AccountingConflict, AccountingReducer, Applied, ComponentSums, ScopeTotals, TokenSums,
};
pub use snapshot::{
    ACCOUNTING_SNAPSHOT_VERSION, AccountingBillingScope, AccountingBudgetSnapshot,
    AccountingCycleSnapshot, AccountingScopeSnapshot, AccountingSnapshot,
};

pub use crate::core_staging::accounting::{
    AccountingReason, AttemptOutcome, AttemptPurpose, AttemptRef, AttemptSettlement, AttemptStart,
    BillingBasis, DispatchMode, PricingApplicability, RequestAccounting, UsageDisposition,
    non_secret_route,
};

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use querymt::providers::{BillingClass, value_usage};

    pub(crate) use crate::core_staging::accounting::samples::{
        sample_pricing, sample_ref, sample_settlement, sample_start,
    };

    #[test]
    fn start_round_trips() {
        let start = sample_start();
        let json = serde_json::to_string(&start).unwrap();
        assert_eq!(serde_json::from_str::<AttemptStart>(&json).unwrap(), start);
    }

    #[test]
    fn settlement_round_trips_and_keeps_valuation() {
        let settled = sample_settlement();
        let json = serde_json::to_string(&settled).unwrap();
        let back: AttemptSettlement = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settled);
        assert!(back.valuation.is_complete());
    }

    #[test]
    fn unavailable_usage_is_absent_not_zero() {
        let mut settled = sample_settlement();
        settled.disposition = UsageDisposition::Unavailable;
        settled.outcome = AttemptOutcome::TransportFailure;
        settled.usage = None;
        settled.valuation = value_usage(settled.pricing.as_ref(), None);
        let json = serde_json::to_value(&settled).unwrap();
        assert!(json.get("usage").is_none(), "usage must be omitted: {json}");
        assert!(!settled.valuation.is_complete());
        let back: AttemptSettlement = serde_json::from_value(json).unwrap();
        assert_eq!(back.usage, None);
    }

    #[test]
    fn enums_use_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_string(&AttemptPurpose::DelegationSummary).unwrap(),
            r#""delegation_summary""#
        );
        assert_eq!(
            serde_json::to_string(&UsageDisposition::NotBillable).unwrap(),
            r#""not_billable""#
        );
        assert_eq!(
            serde_json::to_string(&AttemptOutcome::OutputRejected).unwrap(),
            r#""output_rejected""#
        );
        assert_eq!(
            serde_json::to_string(&PricingApplicability::Inapplicable).unwrap(),
            r#""inapplicable""#
        );
    }

    /// Recursively collect every object key in a JSON value.
    fn keys(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    out.push(k.clone());
                    keys(v, out);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
            _ => {}
        }
    }

    #[test]
    fn payloads_carry_no_content_or_credential_fields() {
        let mut all = Vec::new();
        keys(&serde_json::to_value(sample_start()).unwrap(), &mut all);
        keys(
            &serde_json::to_value(sample_settlement()).unwrap(),
            &mut all,
        );
        for forbidden in [
            "prompt",
            "content",
            "text",
            "message",
            "reasoning_text",
            "api_key",
            "token",
            "secret",
            "authorization",
            "headers",
            "endpoint",
            "url",
            "base_url",
        ] {
            assert!(
                !all.iter().any(|k| k == forbidden),
                "forbidden key {forbidden} in {all:?}"
            );
        }
    }

    #[test]
    fn events_are_durable_and_round_trip_through_the_journal_shape() {
        use crate::events::{
            AgentEventKind, Durability, DurableEvent, EventOrigin, classify_durability,
        };

        let kinds = [
            AgentEventKind::LlmAttemptStarted {
                start: sample_start(),
            },
            AgentEventKind::LlmAttemptSettled {
                settlement: sample_settlement(),
            },
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            assert_eq!(classify_durability(&kind), Durability::Durable);
            let event = DurableEvent {
                event_id: format!("evt-{i}"),
                stream_seq: i as i64 + 1,
                session_seq: i as i64 + 1,
                session_id: "sess-1".into(),
                timestamp: 1_700_000_000,
                origin: EventOrigin::Local,
                source_node: None,
                kind,
            };
            let json = serde_json::to_string(&event).unwrap();
            let back: DurableEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }

    #[test]
    fn event_wire_tags_are_snake_case() {
        use crate::events::AgentEventKind;
        let started = serde_json::to_value(AgentEventKind::LlmAttemptStarted {
            start: sample_start(),
        })
        .unwrap();
        assert_eq!(started["type"], "llm_attempt_started");
        assert_eq!(started["data"]["start"]["attempt"]["attempt_id"], "att-1");
        let settled = serde_json::to_value(AgentEventKind::LlmAttemptSettled {
            settlement: sample_settlement(),
        })
        .unwrap();
        assert_eq!(settled["type"], "llm_attempt_settled");
        assert_eq!(settled["data"]["settlement"]["disposition"], "final");
    }

    #[test]
    fn documented_start_example() {
        use crate::events::AgentEventKind;
        let documented = serde_json::json!({
            "type": "llm_attempt_started",
            "data": { "start": {
                "attempt": { "operation_id": "op-1", "attempt_id": "att-1",
                             "attempt_index": 0, "session_id": "sess-1",
                             "cycle_id": "cycle-1", "purpose": "main_response" },
                "mode": "streaming", "provider": "anthropic", "model": "claude-sonnet",
                "route": "https://api.example.com", "billing": "metered",
                "pricing": { "input": 3.0, "output": 15.0,
                             "cache_read": 0.3, "cache_write": 3.75 }
            } }
        });
        let actual = serde_json::to_value(AgentEventKind::LlmAttemptStarted {
            start: sample_start(),
        })
        .unwrap();
        assert_eq!(actual, documented);
        let parsed: AgentEventKind = serde_json::from_value(documented).unwrap();
        assert!(matches!(parsed, AgentEventKind::LlmAttemptStarted { .. }));
    }

    #[test]
    fn documented_request_end_example() {
        use crate::events::{AgentEventKind, ExecutionMetrics};
        // Mirrors the JSON in docs/docs/agent/api_reference.md.
        let documented = serde_json::json!({
            "type": "llm_request_end",
            "data": {
                "usage": { "input_tokens": 100, "output_tokens": 10, "reasoning_tokens": 5,
                           "cache_read": 50, "cache_write": 20 },
                "tool_calls": 0,
                "finish_reason": "Stop",
                "cost_usd": 0.000615,
                "cumulative_cost_usd": 0.000615,
                "context_tokens": 185,
                "metrics": { "steps": 1, "turns": 1 },
                "accounting": {
                    "operation_id": "op-1",
                    "attempt_ids": ["att-1"],
                    "known_subtotal_usd": 0.000615,
                    "complete": true,
                    "reasons": []
                }
            }
        });
        let parsed: AgentEventKind = serde_json::from_value(documented.clone()).unwrap();
        let AgentEventKind::LlmRequestEnd {
            accounting: Some(acc),
            cost_usd,
            ..
        } = &parsed
        else {
            panic!("documented example must parse with accounting");
        };
        assert_eq!(acc.attempt_ids, vec!["att-1".to_string()]);
        assert!(acc.complete && acc.reasons.is_empty());
        assert_eq!(*cost_usd, Some(0.000615));
        // And it is what the reducer would publish for the sample settlement.
        let settled = sample_settlement();
        assert!((settled.valuation.known_subtotal_usd - 0.000615).abs() < 1e-9);
        // Serialization round-trips field-for-field.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), documented);
        let _ = ExecutionMetrics { steps: 1, turns: 1 };
    }

    #[test]
    fn request_end_without_accounting_still_parses_and_omits_the_field() {
        use crate::events::AgentEventKind;
        let legacy = serde_json::json!({
            "type": "llm_request_end",
            "data": {
                "usage": null, "tool_calls": 0, "finish_reason": null,
                "cost_usd": null, "cumulative_cost_usd": null, "context_tokens": 0,
                "metrics": { "steps": 1, "turns": 1 }
            }
        });
        let parsed: AgentEventKind = serde_json::from_value(legacy).unwrap();
        let json = serde_json::to_value(&parsed).unwrap();
        assert!(json["data"].get("accounting").is_none());
    }

    #[test]
    fn excluded_billing_start_carries_no_pricing_snapshot() {
        let mut start = sample_start();
        start.billing = BillingClass::Excluded;
        start.pricing = None;
        let json = serde_json::to_value(&start).unwrap();
        assert_eq!(json["billing"], "excluded");
        assert!(json.get("pricing").is_none());
        assert_eq!(serde_json::from_value::<AttemptStart>(json).unwrap(), start);
    }

    #[test]
    fn route_keeps_only_scheme_and_host() {
        let start = sample_start();
        assert_eq!(start.route.as_deref(), Some("https://api.example.com"));
        let json = serde_json::to_string(&start).unwrap();
        for leaked in ["user", "pw", "key=secret", "/v1/x"] {
            assert!(!json.contains(leaked), "{leaked} leaked in {json}");
        }
    }

    #[test]
    fn route_sanitizer_edge_cases() {
        assert_eq!(
            non_secret_route("HTTP://Host.Example:8080/p#frag").as_deref(),
            Some("http://host.example:8080")
        );
        assert_eq!(
            non_secret_route("https://a@b@host.example?x=1").as_deref(),
            Some("https://host.example")
        );
        assert_eq!(non_secret_route("file:///etc/passwd"), None);
        assert_eq!(non_secret_route("https://"), None);
        assert_eq!(non_secret_route("https://user@/path"), None);
        assert_eq!(non_secret_route("not a url"), None);
    }
}
