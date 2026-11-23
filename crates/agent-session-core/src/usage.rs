//! Context usage, token breakdown and cost.

use crate::core::SessionCore;
use crate::view::{ContextUsage, CostView, TokenUsageView};
use querymt_agent_protocol_types::accounting::{AccountingBillingScope, AccountingSnapshot};
use querymt_agent_protocol_types::replay::{
    ACCOUNTING_META_KEY, QUERYMT_META_NAMESPACE, USAGE_META_KEY,
};
use querymt_agent_protocol_types::session::history::UsageBreakdown;
use serde::Deserialize;
use serde_json::Value;

/// `_meta.querymt.usage` (v1).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMeta {
    latest_request: Option<UsageBreakdown>,
    session_totals: Option<UsageBreakdown>,
    #[serde(default)]
    partial: bool,
}

impl SessionCore {
    /// The accounting snapshot is kept as received and the cost is derived
    /// from it: the complete total when there is one, otherwise the known
    /// subtotal marked incomplete. A scope that covers no model call leaves
    /// both empty: live the agent sends no accounting for such a session, so
    /// a reload must not show an incomplete zero.
    pub(crate) fn set_accounting(&mut self, accounting: &AccountingSnapshot) {
        let session = &accounting.session;
        let no_model_calls = session.billing == AccountingBillingScope::Empty
            && session.priced_attempts == 0
            && session.excluded_attempts == 0
            && session.unresolved_attempts == 0
            && session.incomplete_attempts == 0
            && session.unaccounted_generations == 0;
        if no_model_calls {
            self.header.cost = None;
            self.header.accounting = None;
            return;
        }
        self.header.accounting = Some(accounting.clone());
        self.header.cost = Some(match session.complete_total_usd {
            Some(amount) if session.complete => CostView {
                amount,
                currency: "USD".to_owned(),
                complete: true,
            },
            _ => CostView {
                amount: session.known_subtotal_usd,
                currency: "USD".to_owned(),
                complete: false,
            },
        });
    }

    /// The token breakdown of `_meta.querymt.usage`, when present. Carried by
    /// `usage_update` and by the metadata-only `session_info_update`.
    pub(crate) fn set_token_usage(&mut self, querymt: Option<&Value>) {
        if let Some(usage) = querymt
            .and_then(|q| q.get(USAGE_META_KEY))
            .and_then(|v| UsageMeta::deserialize(v).ok())
        {
            self.header.tokens = Some(TokenUsageView {
                latest_request: usage.latest_request,
                session_totals: usage.session_totals,
                partial: usage.partial,
            });
        }
    }

    /// `usage_update`: context usage replaces the old value; the optional
    /// `_meta.querymt` carries the token breakdown and accounting.
    pub(crate) fn usage_update(
        &mut self,
        used: u64,
        size: u64,
        cost: Option<(f64, &str)>,
        meta: Option<&serde_json::Map<String, Value>>,
    ) {
        self.header.context = Some(ContextUsage { used, size });
        let querymt = meta.and_then(|meta| meta.get(QUERYMT_META_NAMESPACE));
        self.set_token_usage(querymt);
        let accounting = querymt
            .and_then(|q| q.get(ACCOUNTING_META_KEY))
            .and_then(|v| AccountingSnapshot::deserialize(v).ok());
        match (accounting, cost) {
            (Some(accounting), _) => self.set_accounting(&accounting),
            (None, Some((amount, currency))) => {
                // A cost the accounting does not back.
                self.header.accounting = None;
                self.header.cost = Some(CostView {
                    amount,
                    currency: currency.to_owned(),
                    complete: true,
                });
            }
            (None, None) => {}
        }
    }
}
