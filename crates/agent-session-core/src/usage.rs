//! Context usage, token breakdown and cost.

use crate::core::SessionCore;
use crate::view::{ContextUsage, CostView, TokenUsageView};
use querymt_agent_protocol_types::accounting::AccountingSnapshot;
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
    /// Cost comes only from accounting data: the complete total when there is
    /// one, otherwise the known subtotal marked incomplete.
    pub(crate) fn set_accounting(&mut self, accounting: &AccountingSnapshot) {
        let session = &accounting.session;
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
        let querymt = meta.and_then(|meta| meta.get("querymt"));
        if let Some(usage) = querymt
            .and_then(|q| q.get("usage"))
            .and_then(|v| UsageMeta::deserialize(v).ok())
        {
            self.header.tokens = Some(TokenUsageView {
                latest_request: usage.latest_request,
                session_totals: usage.session_totals,
                partial: usage.partial,
            });
        }
        let accounting = querymt
            .and_then(|q| q.get("accounting"))
            .and_then(|v| AccountingSnapshot::deserialize(v).ok());
        match (accounting, cost) {
            (Some(accounting), _) => self.set_accounting(&accounting),
            (None, Some((amount, currency))) => {
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
