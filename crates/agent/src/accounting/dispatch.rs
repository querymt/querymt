//! Attempt-boundary helpers used where provider requests are dispatched.
//!
//! An [`AttemptOperation`] is one logical LLM operation (for example one main
//! request with its retries). Each physical provider request inside it is an
//! [`ActiveAttempt`]: `begin` durably records the start *before* the request
//! is sent, and exactly one of the `settle_*` methods records the outcome.
//!
//! * Settlement is explicit and awaited, and must run outside any future that
//!   a cancellation `select!` can drop.
//! * As a safety net, dropping an unsettled attempt (the owning future was
//!   dropped) settles it in a spawned task as cancelled with whatever usage
//!   was observed, rather than leaving only an unresolved start.
//! * Output validation happens after settlement, so a rejected summary or
//!   response still carries its cost.
//! * Billing class and the pricing snapshot are supplied by the caller at
//!   dispatch and are never re-read afterwards.

use super::{
    AccountingError, AccountingReducer, AttemptOutcome, AttemptPurpose, AttemptRecorder,
    AttemptRef, AttemptSettlement, AttemptStart, CycleAccounting, DispatchMode,
    PricingApplicability, ScopeTotals, UsageDisposition,
};
use crate::events::AgentEventKind;
use crate::session::provider::BillingBasis;
use parking_lot::Mutex;
use querymt::Usage;
use querymt::chat::{ChatOutput, ChatOutputProvenance};
use querymt::error::{LLMError, ProviderErrorKind};
use querymt::providers::{
    BillingClass, IncompleteReason, ModelPricing, UsageValuation, value_usage,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Looks up the catalog pricing of a `(provider, model)` pair. Excluded
/// (subscription/OAuth) providers must return `None`.
pub type PricingLookup = Arc<dyn Fn(&str, &str) -> Option<ModelPricing> + Send + Sync>;

fn catalog_lookup() -> PricingLookup {
    Arc::new(|provider, model| {
        crate::session::provider::SessionProvider::get_pricing(provider, model)
    })
}

/// True when `effective` is `requested` or a dated snapshot of it
/// (`requested` + `-` + digits/dashes, e.g. `gpt-4o` -> `gpt-4o-2024-08-06`).
/// Anything else is a different model and the bound pricing may not apply.
fn same_model(requested: &str, effective: &str) -> bool {
    if requested.eq_ignore_ascii_case(effective) {
        return true;
    }
    let (req, eff) = (
        requested.to_ascii_lowercase(),
        effective.to_ascii_lowercase(),
    );
    eff.strip_prefix(&req)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rest| {
            !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '-')
        })
}

/// Decide which pricing applies to the response actually served.
///
/// The pricing bound at dispatch applies unless the provider reports another
/// model. Then a trustworthy catalog basis for the effective model is bound
/// (`Rebound`); with none, the selected pricing is known-inapplicable and the
/// valuation is incomplete (`Inapplicable`). Excluded billing never gains
/// catalog pricing. Provider names are not compared: the logical provider
/// name need not match the implementation's reported name.
pub(crate) fn bind_pricing(
    basis: &BillingBasis,
    requested_model: &str,
    provenance: Option<&ChatOutputProvenance>,
    lookup: &dyn Fn(&str, &str) -> Option<ModelPricing>,
) -> (PricingApplicability, Option<ModelPricing>) {
    let selected = (PricingApplicability::Selected, basis.pricing.clone());
    let Some(p) = provenance else {
        return selected;
    };
    if basis.billing == BillingClass::Excluded
        || p.model.is_empty()
        || same_model(requested_model, &p.model)
    {
        return selected;
    }
    match lookup(&p.provider, &p.model) {
        Some(pricing) => (PricingApplicability::Rebound, Some(pricing)),
        None => (PricingApplicability::Inapplicable, None),
    }
}

struct OperationIdentity {
    operation_id: String,
    session_id: String,
    cycle_id: String,
    purpose: AttemptPurpose,
    provider: String,
    model: String,
}

/// What a dispatching component needs to record its attempts: the logical
/// operation (identity, cycle, purpose) and the billing basis bound for the
/// provider that will handle them. Required, never optional, by every
/// component that dispatches paid requests outside the main loop.
#[derive(Clone)]
pub struct DispatchLedger {
    pub operation: Arc<AttemptOperation>,
    pub basis: BillingBasis,
}

/// One logical operation; shared by reference across its retry attempts.
pub struct AttemptOperation {
    recorder: Arc<AttemptRecorder>,
    identity: Arc<OperationIdentity>,
    next_index: AtomicU32,
    lookup: PricingLookup,
    /// Manual budget pricing snapshotted onto every attempt at dispatch.
    budget_pricing: Option<ModelPricing>,
    /// The cycle this operation is charged to; its committed budget view is
    /// checked against `budget_cap_usd` before every paid attempt.
    cycle: CycleAccounting,
    budget_cap_usd: Option<f64>,
    /// Set when the budget gate refused a dispatch, so callers can report a
    /// price-limit stop instead of an execution failure.
    budget_refused: Mutex<Option<(f64, f64)>>,
    /// Records of this operation's attempts only, for operation-level totals.
    local: Arc<Mutex<AccountingReducer>>,
    attempt_ids: Mutex<Vec<String>>,
}

impl AttemptOperation {
    pub fn new(
        recorder: Arc<AttemptRecorder>,
        cycle: &CycleAccounting,
        purpose: AttemptPurpose,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            identity: Arc::new(OperationIdentity {
                operation_id: uuid::Uuid::now_v7().to_string(),
                session_id: cycle.session_id().to_string(),
                cycle_id: cycle.cycle_id().to_string(),
                purpose,
                provider: provider.into(),
                model: model.into(),
            }),
            recorder,
            next_index: AtomicU32::new(0),
            lookup: catalog_lookup(),
            budget_pricing: None,
            cycle: cycle.clone(),
            budget_cap_usd: None,
            budget_refused: Mutex::new(None),
            local: Arc::new(Mutex::new(AccountingReducer::new(cycle.session_id()))),
            attempt_ids: Mutex::new(Vec::new()),
        }
    }

    /// Accounting for this operation's attempts so far (all retries).
    pub fn totals(&self) -> ScopeTotals {
        self.local.lock().session().clone()
    }

    /// Snapshot this manual budget pricing onto each attempt at dispatch. The
    /// snapshot is recorded in the start and valued separately at settlement,
    /// so changing the configured rates later never reprices earlier
    /// attempts, and reported API spend keeps its own pricing basis.
    pub fn with_budget_pricing(mut self, pricing: Option<ModelPricing>) -> Self {
        self.budget_pricing = pricing;
        self
    }

    /// Stop dispatching paid attempts once this cycle's known budget spend
    /// reaches `cap`. Best effort: attempts already in flight can overshoot,
    /// and an incomplete budget still enforces against its known subtotal.
    pub fn with_budget_cap(mut self, cap_usd: Option<f64>) -> Self {
        self.budget_cap_usd = cap_usd;
        self
    }

    /// `(cap, spent)` if the budget gate refused a dispatch of this operation.
    pub fn budget_refusal(&self) -> Option<(f64, f64)> {
        *self.budget_refused.lock()
    }

    /// Replace the effective-model pricing lookup (tests, custom catalogs).
    pub fn with_pricing_lookup(mut self, lookup: PricingLookup) -> Self {
        self.lookup = lookup;
        self
    }

    pub fn operation_id(&self) -> &str {
        &self.identity.operation_id
    }

    /// Request-end accounting detail for this operation's attempts so far.
    pub fn request_accounting(&self) -> super::RequestAccounting {
        let totals = self.totals();
        super::RequestAccounting {
            operation_id: self.identity.operation_id.clone(),
            attempt_ids: self.attempt_ids.lock().clone(),
            known_subtotal_usd: totals.known_subtotal_usd,
            complete: totals.complete_total_usd().is_some(),
            reasons: totals.reasons(),
        }
    }

    fn observe_local(&self, kind: AgentEventKind) {
        if let Err(error) = self.local.lock().apply(&kind) {
            log::error!("operation accounting conflict: {error}");
        }
    }

    /// Durably record the start of the next physical attempt. Send the
    /// provider request only if this returns `Ok`.
    pub async fn begin(
        &self,
        mode: DispatchMode,
        basis: BillingBasis,
    ) -> Result<ActiveAttempt, AccountingError> {
        // The shared cycle-budget check for every paid dispatch (main,
        // retry, compaction, summary). Prior cycles do not count: the view
        // is this cycle's committed accounting only.
        if let Some(cap) = self.budget_cap_usd {
            let spent = self.cycle.cycle_totals().budget_known_subtotal_usd;
            if spent >= cap {
                *self.budget_refused.lock() = Some((cap, spent));
                return Err(AccountingError::BudgetExhausted {
                    cap_usd: cap,
                    spent_usd: spent,
                });
            }
        }
        let id = &self.identity;
        let attempt = AttemptRef {
            operation_id: id.operation_id.clone(),
            attempt_id: uuid::Uuid::now_v7().to_string(),
            attempt_index: self.next_index.fetch_add(1, Ordering::SeqCst),
            session_id: id.session_id.clone(),
            cycle_id: id.cycle_id.clone(),
            purpose: id.purpose,
        };
        let start = AttemptStart {
            attempt: attempt.clone(),
            mode,
            provider: id.provider.clone(),
            model: id.model.clone(),
            route: None,
            billing: basis.billing,
            pricing: basis.pricing.clone(),
            // Subscription-excluded attempts are never reclassified as
            // metered API spend by a manual override.
            budget_pricing: (basis.billing != BillingClass::Excluded)
                .then(|| self.budget_pricing.clone())
                .flatten(),
        };
        let budget_pricing = start.budget_pricing.clone();
        let handle = self.recorder.begin(start.clone()).await?;
        self.attempt_ids.lock().push(attempt.attempt_id.clone());
        self.observe_local(AgentEventKind::LlmAttemptStarted { start });
        Ok(ActiveAttempt {
            recorder: self.recorder.clone(),
            attempt,
            basis,
            requested_model: id.model.clone(),
            lookup: self.lookup.clone(),
            budget_pricing,
            local: self.local.clone(),
            handle: Some(handle),
            usage: None,
        })
    }
}

/// A begun, not yet settled physical attempt.
#[must_use = "a begun attempt must be settled"]
pub struct ActiveAttempt {
    recorder: Arc<AttemptRecorder>,
    attempt: AttemptRef,
    basis: BillingBasis,
    requested_model: String,
    lookup: PricingLookup,
    budget_pricing: Option<ModelPricing>,
    local: Arc<Mutex<AccountingReducer>>,
    handle: Option<super::AttemptHandle>,
    /// Best usage observed so far (streamed partials).
    usage: Option<Usage>,
}

/// Disposition and outcome for a provider error.
///
/// `response_started` is true once any response bytes were received: after
/// that, billing is never "confirmed not billable".
pub fn classify_error(
    error: &LLMError,
    response_started: bool,
    usage: Option<&Usage>,
) -> (UsageDisposition, AttemptOutcome) {
    let uncertain = || {
        if usage.is_some() {
            UsageDisposition::Partial
        } else {
            UsageDisposition::Unavailable
        }
    };
    if matches!(error, LLMError::Cancelled) {
        return (uncertain(), AttemptOutcome::Cancelled);
    }
    if response_started {
        let outcome = match error {
            LLMError::ResponseFormatError { .. } => AttemptOutcome::OutputRejected,
            _ => AttemptOutcome::TransportFailure,
        };
        return (uncertain(), outcome);
    }
    match error {
        // Rejected locally, before anything could be sent.
        LLMError::InvalidRequest(_)
        | LLMError::ToolConfigError(_)
        | LLMError::InvalidUrl(_)
        | LLMError::NotImplemented(_) => (UsageDisposition::NotSent, AttemptOutcome::LocalFailure),
        // The provider answered with a definitive rejection.
        LLMError::AuthError(_) | LLMError::RateLimited { .. } => (
            UsageDisposition::NotBillable,
            AttemptOutcome::ProviderRejected,
        ),
        LLMError::HttpStatus { status_code, .. } if (400..500).contains(status_code) => (
            UsageDisposition::NotBillable,
            AttemptOutcome::ProviderRejected,
        ),
        LLMError::ProviderResponseError(failure)
            if matches!(
                failure.kind(),
                ProviderErrorKind::RateLimited
                    | ProviderErrorKind::QuotaExceeded
                    | ProviderErrorKind::ContextWindowExceeded
                    | ProviderErrorKind::Authentication
                    | ProviderErrorKind::InvalidRequest
                    | ProviderErrorKind::UnsupportedOperation
            ) =>
        {
            (
                UsageDisposition::NotBillable,
                AttemptOutcome::ProviderRejected,
            )
        }
        // Server errors and unclassified failures may or may not have been
        // billed: unknown, never zero.
        LLMError::ProviderResponseError(_) | LLMError::HttpStatus { .. } => (
            UsageDisposition::Unavailable,
            AttemptOutcome::ProviderRejected,
        ),
        LLMError::ResponseFormatError { .. } => (
            UsageDisposition::Unavailable,
            AttemptOutcome::OutputRejected,
        ),
        _ => (
            UsageDisposition::Unavailable,
            AttemptOutcome::TransportFailure,
        ),
    }
}

/// Build a settlement from what is known. Pure; used by every settle path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_settlement(
    attempt: &AttemptRef,
    basis: &BillingBasis,
    disposition: UsageDisposition,
    outcome: AttemptOutcome,
    usage: Option<Usage>,
    provenance: Option<&ChatOutputProvenance>,
    requested_model: &str,
    lookup: &dyn Fn(&str, &str) -> Option<ModelPricing>,
) -> AttemptSettlement {
    let (applicability, pricing) = bind_pricing(basis, requested_model, provenance, lookup);
    let valuation = match basis.billing {
        BillingClass::Excluded => UsageValuation::excluded(),
        _ if applicability == PricingApplicability::Inapplicable => {
            // The bound rates are known not to apply: claim no amounts.
            let mut v = value_usage(None, usage.as_ref());
            v.billing = basis.billing;
            v.incomplete
                .retain(|r| !matches!(r, IncompleteReason::PricingUnavailable));
            v.incomplete.insert(
                0,
                IncompleteReason::PricingInapplicable {
                    effective_model: provenance.map(|p| p.model.clone()).unwrap_or_default(),
                },
            );
            v
        }
        _ => value_usage(pricing.as_ref(), usage.as_ref()),
    };
    AttemptSettlement {
        attempt: attempt.clone(),
        disposition,
        outcome,
        usage,
        effective_provider: provenance.map(|p| p.provider.clone()),
        effective_model: provenance.map(|p| p.model.clone()),
        pricing_applicability: applicability,
        pricing,
        valuation,
        budget_valuation: None,
    }
}

/// Attach the separate manual-budget valuation, using the pricing snapshotted
/// at dispatch and the same usage and calculator as the API valuation.
fn with_budget_valuation(
    mut settlement: AttemptSettlement,
    budget_pricing: Option<&ModelPricing>,
) -> AttemptSettlement {
    if let Some(pricing) = budget_pricing
        && settlement.valuation.billing != BillingClass::Excluded
    {
        let mut valuation = value_usage(Some(pricing), settlement.usage.as_ref());
        // Same billing classification as the attempt it values.
        valuation.billing = settlement.valuation.billing;
        settlement.budget_valuation = Some(valuation);
    }
    settlement
}

impl ActiveAttempt {
    pub fn attempt_id(&self) -> &str {
        &self.attempt.attempt_id
    }

    /// Remember usage seen before the attempt ends (for example streamed
    /// partials), used if the attempt is interrupted. Takes the field-wise
    /// maximum so a later snapshot never lowers earlier counts.
    pub fn observe_usage(&mut self, usage: Option<Usage>) {
        if let Some(usage) = usage {
            self.usage = Some(match self.usage.take() {
                Some(previous) => previous.merge_max(usage),
                None => usage,
            });
        }
    }

    async fn settle(
        mut self,
        disposition: UsageDisposition,
        outcome: AttemptOutcome,
        usage: Option<Usage>,
        provenance: Option<&ChatOutputProvenance>,
    ) -> Result<(), AccountingError> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let settlement = build_settlement(
            &self.attempt,
            &self.basis,
            disposition,
            outcome,
            usage,
            provenance,
            &self.requested_model,
            &*self.lookup,
        );
        let settlement = with_budget_valuation(settlement, self.budget_pricing.as_ref());
        self.recorder.settle(handle, settlement.clone()).await?;
        if let Err(error) = self
            .local
            .lock()
            .apply(&AgentEventKind::LlmAttemptSettled { settlement })
        {
            log::error!("operation accounting conflict: {error}");
        }
        Ok(())
    }

    /// The provider returned an output: final usage if it reported any.
    pub async fn settle_output(self, output: &ChatOutput) -> Result<(), AccountingError> {
        let disposition = if output.usage.is_some() {
            UsageDisposition::Final
        } else {
            UsageDisposition::Unavailable
        };
        self.settle(
            disposition,
            AttemptOutcome::Succeeded,
            output.usage.clone(),
            output.provenance.as_ref(),
        )
        .await
    }

    /// The response was received but failed validation. Its usage is still
    /// accountable. `interrupted` marks usage that may be incomplete.
    pub async fn settle_rejected_output(
        self,
        output: &ChatOutput,
        interrupted: bool,
    ) -> Result<(), AccountingError> {
        let usage = output.usage.clone();
        let disposition = match (&usage, interrupted) {
            (None, _) => UsageDisposition::Unavailable,
            (Some(_), true) => UsageDisposition::Partial,
            (Some(_), false) => UsageDisposition::Final,
        };
        self.settle(
            disposition,
            AttemptOutcome::OutputRejected,
            usage,
            output.provenance.as_ref(),
        )
        .await
    }

    /// The attempt ended in a provider/transport error or cancellation.
    pub async fn settle_error(
        mut self,
        error: &LLMError,
        response_started: bool,
        partial: Option<Usage>,
    ) -> Result<(), AccountingError> {
        self.observe_usage(partial);
        let usage = self.usage.clone();
        let (disposition, outcome) = classify_error(error, response_started, usage.as_ref());
        // Definitive non-billable outcomes carry no usage.
        let usage = matches!(
            disposition,
            UsageDisposition::NotSent | UsageDisposition::NotBillable
        )
        .then_some(None)
        .unwrap_or(usage);
        self.settle(disposition, outcome, usage, None).await
    }
}

/// Merge two optional usage observations field-wise (never lowering counts).
pub fn merge_usage(a: Option<Usage>, b: Option<Usage>) -> Option<Usage> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.merge_max(b)),
        (a, b) => a.or(b),
    }
}

/// Map an accounting failure to a non-retryable provider-call error, so an
/// attempt closure can fail the call without the retry loop re-dispatching.
pub fn accounting_llm_error(error: AccountingError) -> LLMError {
    LLMError::GenericError(format!("accounting failure: {error}"))
}

/// Run one non-streaming provider request as a recorded attempt.
///
/// The start is durable before `call` is polled. `call` runs under a
/// cancellation `select!`, but recording stays outside it, so cancelling
/// still settles the attempt. Output validation is the caller's job and
/// happens after settlement.
pub async fn run_attempt<F, Fut>(
    op: &AttemptOperation,
    basis: BillingBasis,
    cancel: &tokio_util::sync::CancellationToken,
    call: F,
) -> Result<ChatOutput, LLMError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<ChatOutput, LLMError>>,
{
    let attempt = op
        .begin(DispatchMode::NonStreaming, basis)
        .await
        .map_err(accounting_llm_error)?;
    let result = tokio::select! {
        result = call() => result,
        _ = cancel.cancelled() => Err(LLMError::Cancelled),
    };
    match result {
        Ok(output) => {
            attempt
                .settle_output(&output)
                .await
                .map_err(accounting_llm_error)?;
            Ok(output)
        }
        Err(error) => {
            attempt
                .settle_error(&error, false, None)
                .await
                .map_err(accounting_llm_error)?;
            Err(error)
        }
    }
}

impl Drop for ActiveAttempt {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let usage = self.usage.take();
        let disposition = if usage.is_some() {
            UsageDisposition::Partial
        } else {
            UsageDisposition::Unavailable
        };
        let settlement = build_settlement(
            &self.attempt,
            &self.basis,
            disposition,
            AttemptOutcome::Cancelled,
            usage,
            None,
            &self.requested_model,
            &*self.lookup,
        );
        let settlement = with_budget_valuation(settlement, self.budget_pricing.as_ref());
        let recorder = self.recorder.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = recorder.settle(handle, settlement).await {
                    log::error!("failed to settle abandoned attempt: {error}");
                }
            });
        } else {
            log::error!(
                "attempt {} dropped outside a runtime; start remains unresolved",
                self.attempt.attempt_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::SessionAccounting;
    use crate::event_fanout::EventFanout;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session_event_stream::SessionEventStream;
    use querymt::error::ProviderFailure;
    use querymt::providers::ModelPricing;

    fn pricing() -> ModelPricing {
        ModelPricing {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: Some(0.3),
            cache_write: Some(3.75),
            ..Default::default()
        }
    }

    fn metered() -> BillingBasis {
        BillingBasis {
            billing: BillingClass::Metered,
            pricing: Some(pricing()),
        }
    }

    fn usage() -> Usage {
        Usage {
            input_tokens: 100,
            output_tokens: 10,
            reasoning_tokens: 5,
            cache_read: 50,
            cache_write: 20,
            cache_write_tiers: None,
        }
    }

    fn cost() -> f64 {
        (100.0 * 3.0 + 15.0 * 15.0 + 50.0 * 0.3 + 20.0 * 3.75) / 1e6
    }

    fn output(with_usage: bool) -> ChatOutput {
        let mut out = ChatOutput::from_projections(
            None,
            Some("ok".into()),
            None,
            None,
            Some(querymt::chat::FinishReason::Stop),
        );
        out.usage = with_usage.then(usage);
        out
    }

    async fn fixture() -> (AttemptOperation, Arc<SessionAccounting>) {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let journal = storage.event_journal();
        let accounting = SessionAccounting::hydrate("sess-1", journal.clone())
            .await
            .unwrap();
        let stream = SessionEventStream::new("sess-1", journal, Arc::new(EventFanout::new()));
        let recorder = Arc::new(AttemptRecorder::with_projection(stream, accounting.clone()));
        let cycle = CycleAccounting::with_id(accounting.clone(), "cycle-1");
        let op = AttemptOperation::new(recorder, &cycle, AttemptPurpose::MainResponse, "p", "m");
        (op, accounting)
    }

    fn totals(a: &SessionAccounting) -> crate::accounting::ScopeTotals {
        a.read(|r| r.session().clone())
    }

    #[test]
    fn classification_table() {
        use UsageDisposition::*;
        let rate = LLMError::RateLimited {
            message: "m".into(),
            retry_after_secs: None,
        };
        let http = |status_code| LLMError::HttpStatus {
            status_code,
            message: "m".into(),
            retry_after_secs: None,
        };
        let failure =
            |kind| LLMError::ProviderResponseError(Box::new(ProviderFailure::new(kind, "m")));
        let transport = LLMError::HttpError("reset".into());
        let u = usage();

        // Before any response bytes.
        assert_eq!(classify_error(&rate, false, None).0, NotBillable);
        assert_eq!(
            classify_error(&LLMError::AuthError("x".into()), false, None).0,
            NotBillable
        );
        assert_eq!(classify_error(&http(400), false, None).0, NotBillable);
        assert_eq!(classify_error(&http(503), false, None).0, Unavailable);
        assert_eq!(
            classify_error(
                &failure(ProviderErrorKind::ContextWindowExceeded),
                false,
                None
            )
            .0,
            NotBillable
        );
        assert_eq!(
            classify_error(&failure(ProviderErrorKind::ServerOverloaded), false, None).0,
            Unavailable
        );
        assert_eq!(
            classify_error(&failure(ProviderErrorKind::UnknownPermanent), false, None).0,
            Unavailable
        );
        assert_eq!(
            classify_error(&LLMError::InvalidRequest("x".into()), false, None),
            (NotSent, AttemptOutcome::LocalFailure)
        );
        assert_eq!(
            classify_error(&transport, false, None),
            (Unavailable, AttemptOutcome::TransportFailure)
        );

        // After the response started, a rejection is never confirmed free.
        assert_eq!(classify_error(&rate, true, None).0, Unavailable);
        assert_eq!(classify_error(&transport, true, Some(&u)).0, Partial);

        // Cancellation preserves available partial usage.
        assert_eq!(
            classify_error(&LLMError::Cancelled, true, Some(&u)),
            (Partial, AttemptOutcome::Cancelled)
        );
        assert_eq!(
            classify_error(&LLMError::Cancelled, false, None),
            (Unavailable, AttemptOutcome::Cancelled)
        );
    }

    #[tokio::test]
    async fn output_settlement_prices_all_categories_once() {
        let (op, acct) = fixture().await;
        let attempt = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        assert_eq!(totals(&acct).unresolved_attempts, 1);
        attempt.settle_output(&output(true)).await.unwrap();
        let t = totals(&acct);
        assert_eq!((t.unresolved_attempts, t.priced_attempts), (0, 1));
        assert!((t.complete_total_usd().unwrap() - cost()).abs() < 1e-12);
    }

    /// One session accounting shared by one operation per budget snapshot,
    /// each in its own cycle.
    async fn fixture_with_budgets(
        budgets: Vec<Option<ModelPricing>>,
    ) -> (Vec<AttemptOperation>, Arc<SessionAccounting>) {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let journal = storage.event_journal();
        let accounting = SessionAccounting::hydrate("sess-1", journal.clone())
            .await
            .unwrap();
        let stream = SessionEventStream::new("sess-1", journal, Arc::new(EventFanout::new()));
        let recorder = Arc::new(AttemptRecorder::with_projection(stream, accounting.clone()));
        let ops = budgets
            .into_iter()
            .enumerate()
            .map(|(i, budget)| {
                let cycle = CycleAccounting::with_id(accounting.clone(), format!("cycle-{i}"));
                AttemptOperation::new(
                    recorder.clone(),
                    &cycle,
                    AttemptPurpose::MainResponse,
                    "p",
                    "m",
                )
                .with_budget_pricing(budget)
            })
            .collect();
        (ops, accounting)
    }

    fn manual(input: f64, output: f64, cache: Option<(f64, f64)>) -> ModelPricing {
        ModelPricing {
            input: Some(input),
            output: Some(output),
            cache_read: cache.map(|c| c.0),
            cache_write: cache.map(|c| c.1),
            source: Some(querymt::providers::PricingSource::Manual),
            ..Default::default()
        }
    }

    /// Operations sharing one session accounting: `(cycle id, purpose)` each.
    async fn capped_ops(
        cap: Option<f64>,
        budget: Option<ModelPricing>,
        specs: &[(&str, AttemptPurpose)],
    ) -> (Vec<AttemptOperation>, Arc<SessionAccounting>) {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let journal = storage.event_journal();
        let accounting = SessionAccounting::hydrate("sess-1", journal.clone())
            .await
            .unwrap();
        let stream = SessionEventStream::new("sess-1", journal, Arc::new(EventFanout::new()));
        let recorder = Arc::new(AttemptRecorder::with_projection(stream, accounting.clone()));
        let ops = specs
            .iter()
            .map(|(cycle_id, purpose)| {
                let cycle = CycleAccounting::with_id(accounting.clone(), *cycle_id);
                AttemptOperation::new(recorder.clone(), &cycle, *purpose, "p", "m")
                    .with_budget_pricing(budget.clone())
                    .with_budget_cap(cap)
            })
            .collect();
        (ops, accounting)
    }

    async fn spend(op: &AttemptOperation) {
        let attempt = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        attempt.settle_output(&output(true)).await.unwrap();
    }

    #[tokio::test]
    async fn cache_and_auxiliary_charges_exhaust_the_cycle_cap() {
        // Input+output alone is 525e-6; the cache charges bring it to 615e-6.
        let cap = 600e-6;
        let (ops, acct) = capped_ops(
            Some(cap),
            None,
            &[
                ("c1", AttemptPurpose::MainResponse),
                ("c1", AttemptPurpose::Compaction),
                ("c1", AttemptPurpose::DelegationSummary),
            ],
        )
        .await;
        spend(&ops[0]).await;
        assert!(totals(&acct).budget_known_subtotal_usd >= cap);

        for op in &ops {
            let refused = op.begin(DispatchMode::NonStreaming, metered()).await;
            assert!(
                matches!(refused, Err(AccountingError::BudgetExhausted { .. })),
                "main retry, compaction and summary dispatch are all blocked"
            );
            assert!(op.budget_refusal().is_some());
        }
    }

    #[tokio::test]
    async fn auxiliary_spend_alone_can_block_the_next_main_attempt() {
        let cap = 600e-6;
        let (mut ops, _acct) = capped_ops(
            Some(cap),
            None,
            &[
                ("c1", AttemptPurpose::Compaction),
                ("c1", AttemptPurpose::MainResponse),
            ],
        )
        .await;
        spend(&ops[0]).await;
        let main = ops.remove(1);
        assert!(matches!(
            main.begin(DispatchMode::NonStreaming, metered()).await,
            Err(AccountingError::BudgetExhausted { .. })
        ));
    }

    #[tokio::test]
    async fn refused_dispatch_records_no_attempt() {
        let (mut ops, acct) = capped_ops(None, None, &[("c1", AttemptPurpose::MainResponse)]).await;
        let op = ops.remove(0);
        spend(&op).await;
        let before = acct.read(|r| r.attempt_count());
        let capped = op.with_budget_cap(Some(1e-9));
        assert!(
            capped
                .begin(DispatchMode::Streaming, metered())
                .await
                .is_err()
        );
        assert_eq!(acct.read(|r| r.attempt_count()), before);
    }

    #[tokio::test]
    async fn known_partial_spend_still_stops_at_the_cap() {
        // Manual rates without cache rates: the budget view is partial, but
        // its known subtotal (525e-6) still counts against the cap.
        let cap = 500e-6;
        let (ops, acct) = capped_ops(
            Some(cap),
            Some(manual(3.0, 15.0, None)),
            &[("c1", AttemptPurpose::MainResponse)],
        )
        .await;
        spend(&ops[0]).await;
        let t = totals(&acct);
        assert_eq!(t.budget_complete_total_usd(), None, "budget is incomplete");
        assert!(t.budget_known_subtotal_usd >= cap);
        assert!(matches!(
            ops[0].begin(DispatchMode::NonStreaming, metered()).await,
            Err(AccountingError::BudgetExhausted { .. })
        ));
    }

    #[tokio::test]
    async fn prior_cycle_spend_alone_does_not_exhaust_a_new_cycle() {
        let cap = 600e-6;
        let (mut ops, acct) = capped_ops(
            Some(cap),
            None,
            &[
                ("old", AttemptPurpose::MainResponse),
                ("new", AttemptPurpose::MainResponse),
            ],
        )
        .await;
        let new_cycle = ops.remove(1);
        let old_cycle = ops.remove(0).with_budget_cap(None);
        spend(&old_cycle).await;
        spend(&old_cycle).await;
        assert!(
            totals(&acct).budget_known_subtotal_usd > cap,
            "session spend exceeds the cap"
        );
        assert!(
            new_cycle
                .begin(DispatchMode::NonStreaming, metered())
                .await
                .is_ok(),
            "a new cycle starts from zero cycle spend"
        );
    }

    #[tokio::test]
    async fn budget_snapshot_is_valued_separately_with_the_shared_calculator() {
        let rates = manual(1.0, 2.0, Some((0.1, 1.0)));
        let (ops, acct) = fixture_with_budgets(vec![Some(rates.clone())]).await;
        let attempt = ops[0]
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        attempt.settle_output(&output(true)).await.unwrap();
        let t = totals(&acct);

        // Reported API spend keeps its own (catalog) pricing basis.
        assert!((t.known_subtotal_usd - cost()).abs() < 1e-12);
        assert!((t.complete_total_usd().unwrap() - cost()).abs() < 1e-12);

        // The budget view is exactly what the shared calculator yields for
        // the same usage and the snapshotted rates, cache categories included.
        let expected = value_usage(Some(&rates), Some(&usage()));
        assert!(expected.known_subtotal_usd > 0.0);
        assert!((t.budget_known_subtotal_usd - expected.known_subtotal_usd).abs() < 1e-12);
        assert!(
            (t.budget_known_subtotal_usd - (100.0 + 15.0 * 2.0 + 50.0 * 0.1 + 20.0) / 1e6).abs()
                < 1e-12
        );
        assert!(t.budget_complete_total_usd().is_some());
    }

    #[tokio::test]
    async fn changing_manual_rates_does_not_reprice_earlier_budget_charges() {
        let first = manual(1.0, 2.0, Some((0.1, 1.0)));
        let second = manual(10.0, 20.0, Some((1.0, 10.0)));
        let (ops, acct) =
            fixture_with_budgets(vec![Some(first.clone()), Some(second.clone())]).await;

        let a = ops[0]
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        a.settle_output(&output(true)).await.unwrap();
        let after_first = totals(&acct).budget_known_subtotal_usd;

        let b = ops[1]
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        b.settle_output(&output(true)).await.unwrap();
        let t = totals(&acct);

        let first_cost = value_usage(Some(&first), Some(&usage())).known_subtotal_usd;
        let second_cost = value_usage(Some(&second), Some(&usage())).known_subtotal_usd;
        assert!((after_first - first_cost).abs() < 1e-12);
        assert!(
            (t.budget_known_subtotal_usd - (first_cost + second_cost)).abs() < 1e-12,
            "the earlier charge stays at its own snapshot"
        );
        // API spend is unaffected by manual rates.
        assert!((t.known_subtotal_usd - 2.0 * cost()).abs() < 1e-12);
    }

    #[tokio::test]
    async fn manual_budget_without_cache_rates_is_partial_not_input_priced() {
        let rates = manual(1.0, 2.0, None);
        let (ops, acct) = fixture_with_budgets(vec![Some(rates)]).await;
        let attempt = ops[0]
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        attempt.settle_output(&output(true)).await.unwrap();
        let t = totals(&acct);
        // Input and output only: cache tokens are not priced at the input rate.
        assert!((t.budget_known_subtotal_usd - (100.0 + 15.0 * 2.0) / 1e6).abs() < 1e-12);
        assert_eq!(t.budget_complete_total_usd(), None);
        assert_eq!(t.budget_incomplete_attempts, 1);
        assert!(
            t.complete_total_usd().is_some(),
            "API view is complete and unaffected"
        );
        // The budget view names the missing cache rates, while the API view
        // (complete) has no reasons: this is what the warning reports.
        let reasons = t.budget_reasons();
        assert!(
            reasons.iter().any(|r| matches!(
                r,
                crate::accounting::AccountingReason::Valuation {
                    reason: querymt::providers::IncompleteReason::MissingRate { .. }
                }
            )),
            "{reasons:?}"
        );
        assert!(t.reasons().is_empty(), "{:?}", t.reasons());
    }

    #[tokio::test]
    async fn excluded_attempt_gets_no_manual_budget_valuation() {
        let (ops, acct) =
            fixture_with_budgets(vec![Some(manual(1.0, 2.0, Some((0.1, 1.0))))]).await;
        let excluded = BillingBasis {
            billing: BillingClass::Excluded,
            pricing: None,
        };
        let attempt = ops[0]
            .begin(DispatchMode::NonStreaming, excluded)
            .await
            .unwrap();
        attempt.settle_output(&output(true)).await.unwrap();
        let t = totals(&acct);
        assert_eq!(t.known_subtotal_usd, 0.0);
        assert_eq!(
            t.budget_known_subtotal_usd, 0.0,
            "a manual override never reclassifies subscription usage as metered"
        );
    }

    #[tokio::test]
    async fn retries_are_separate_attempts_under_one_operation() {
        let (op, acct) = fixture().await;
        let first = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        let first_id = first.attempt_id().to_string();
        first
            .settle_error(
                &LLMError::RateLimited {
                    message: "m".into(),
                    retry_after_secs: None,
                },
                false,
                None,
            )
            .await
            .unwrap();
        let second = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        assert_ne!(first_id, second.attempt_id());
        second.settle_output(&output(true)).await.unwrap();
        let t = totals(&acct);
        assert_eq!(t.priced_attempts, 1, "rejected retry adds no charge");
        assert!((t.complete_total_usd().unwrap() - cost()).abs() < 1e-12);
        assert_eq!(acct.read(|r| r.attempt_count()), 2);
    }

    #[tokio::test]
    async fn usage_bearing_failure_stays_priced_and_uncertain() {
        let (op, acct) = fixture().await;
        let mut attempt = op.begin(DispatchMode::Streaming, metered()).await.unwrap();
        attempt.observe_usage(Some(usage()));
        attempt
            .settle_error(&LLMError::HttpError("reset".into()), true, None)
            .await
            .unwrap();
        let t = totals(&acct);
        assert!((t.known_subtotal_usd - cost()).abs() < 1e-12);
        assert_eq!(
            t.complete_total_usd(),
            None,
            "partial usage is never complete"
        );
        assert!(
            t.reasons()
                .contains(&crate::accounting::AccountingReason::PartialUsage)
        );
    }

    #[tokio::test]
    async fn unconfirmed_transport_failure_is_uncertain_not_free() {
        let (op, acct) = fixture().await;
        let attempt = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        attempt
            .settle_error(&LLMError::HttpError("reset".into()), false, None)
            .await
            .unwrap();
        let t = totals(&acct);
        assert_eq!(t.complete_total_usd(), None);
        assert!(
            t.reasons()
                .contains(&crate::accounting::AccountingReason::UsageUnavailable)
        );
    }

    #[tokio::test]
    async fn local_rejection_and_confirmed_rejection_are_not_uncertain() {
        let (op, acct) = fixture().await;
        for error in [
            LLMError::InvalidRequest("bad".into()),
            LLMError::AuthError("nope".into()),
        ] {
            let attempt = op
                .begin(DispatchMode::NonStreaming, metered())
                .await
                .unwrap();
            attempt.settle_error(&error, false, None).await.unwrap();
        }
        let t = totals(&acct);
        assert_eq!((t.unresolved_attempts, t.priced_attempts), (0, 0));
        assert!(t.reasons().is_empty());
    }

    #[tokio::test]
    async fn rejected_output_keeps_available_cost() {
        let (op, acct) = fixture().await;
        let attempt = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        attempt
            .settle_rejected_output(&output(true), false)
            .await
            .unwrap();
        assert!((totals(&acct).complete_total_usd().unwrap() - cost()).abs() < 1e-12);
    }

    #[tokio::test]
    async fn excluded_billing_never_acquires_catalog_charges() {
        let (op, acct) = fixture().await;
        let basis = BillingBasis {
            billing: BillingClass::Excluded,
            pricing: None,
        };
        let attempt = op.begin(DispatchMode::NonStreaming, basis).await.unwrap();
        attempt.settle_output(&output(true)).await.unwrap();
        let t = totals(&acct);
        assert_eq!((t.excluded_attempts, t.priced_attempts), (1, 0));
        assert_eq!(t.known_subtotal_usd, 0.0);
        assert_eq!(t.complete_total_usd(), None);
    }

    #[tokio::test]
    async fn dropped_attempt_is_settled_as_cancelled_with_observed_usage() {
        let (op, acct) = fixture().await;
        {
            let mut attempt = op.begin(DispatchMode::Streaming, metered()).await.unwrap();
            attempt.observe_usage(Some(usage()));
            // Owning future dropped without an explicit settlement.
        }
        for _ in 0..200 {
            if totals(&acct).unresolved_attempts == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let t = totals(&acct);
        assert_eq!(t.unresolved_attempts, 0);
        assert!((t.known_subtotal_usd - cost()).abs() < 1e-12);
        assert!(
            t.reasons()
                .contains(&crate::accounting::AccountingReason::PartialUsage)
        );
    }

    fn provenance(model: &str) -> ChatOutputProvenance {
        ChatOutputProvenance {
            provider: "other".into(),
            model: model.into(),
            endpoint: String::new(),
            protocol: "responses".into(),
        }
    }

    fn no_catalog(_: &str, _: &str) -> Option<ModelPricing> {
        None
    }

    #[test]
    fn same_model_accepts_dated_snapshots_only() {
        assert!(same_model("gpt-4o", "gpt-4o"));
        assert!(same_model("GPT-4o", "gpt-4o"));
        assert!(same_model("gpt-4o", "gpt-4o-2024-08-06"));
        assert!(!same_model("gpt-4o", "gpt-4o-mini"));
        assert!(!same_model("gpt-4", "gpt-4o"));
        assert!(!same_model("gpt-4o", "gpt-4o-"));
    }

    #[test]
    fn matching_or_missing_provenance_keeps_selected_pricing() {
        let basis = metered();
        for p in [
            None,
            Some(provenance("m")),
            Some(provenance("m-2024-08-06")),
        ] {
            let (applicability, bound) = bind_pricing(&basis, "m", p.as_ref(), &no_catalog);
            assert_eq!(applicability, PricingApplicability::Selected);
            assert_eq!(bound, basis.pricing);
        }
    }

    #[test]
    fn different_effective_model_rebinds_only_to_a_known_basis() {
        let basis = metered();
        let cheap = ModelPricing {
            input: Some(1.0),
            output: Some(2.0),
            ..Default::default()
        };
        let lookup = {
            let cheap = cheap.clone();
            move |_: &str, model: &str| (model == "other-model").then(|| cheap.clone())
        };
        let (a, bound) = bind_pricing(&basis, "m", Some(&provenance("other-model")), &lookup);
        assert_eq!((a, bound), (PricingApplicability::Rebound, Some(cheap)));

        let (a, bound) = bind_pricing(&basis, "m", Some(&provenance("unknown")), &lookup);
        assert_eq!((a, bound), (PricingApplicability::Inapplicable, None));
    }

    #[test]
    fn excluded_billing_never_rebinds_to_catalog_pricing() {
        let basis = BillingBasis {
            billing: BillingClass::Excluded,
            pricing: None,
        };
        let any = |_: &str, _: &str| Some(pricing());
        let (a, bound) = bind_pricing(&basis, "m", Some(&provenance("other")), &any);
        assert_eq!((a, bound), (PricingApplicability::Selected, None));
    }

    #[tokio::test]
    async fn incompatible_effective_model_makes_the_valuation_incomplete() {
        let (op, acct) = fixture().await;
        let op = op.with_pricing_lookup(Arc::new(no_catalog));
        let attempt = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        let mut out = output(true);
        out.provenance = Some(provenance("unpriced-model"));
        attempt.settle_output(&out).await.unwrap();

        let t = totals(&acct);
        assert_eq!(
            t.known_subtotal_usd, 0.0,
            "selected rates must not be applied"
        );
        assert_eq!(t.complete_total_usd(), None);
        assert_eq!(t.incomplete_attempts, 1);
        assert!(
            t.reasons()
                .contains(&crate::accounting::AccountingReason::Valuation {
                    reason: IncompleteReason::PricingInapplicable {
                        effective_model: "unpriced-model".into()
                    }
                })
        );
    }

    #[tokio::test]
    async fn rebound_effective_pricing_values_the_attempt_and_is_recorded() {
        let (op, acct) = fixture().await;
        let cheap = ModelPricing {
            input: Some(1.0),
            output: Some(2.0),
            cache_read: Some(0.1),
            cache_write: Some(1.25),
            ..Default::default()
        };
        let lookup = {
            let cheap = cheap.clone();
            move |_: &str, _: &str| Some(cheap.clone())
        };
        let op = op.with_pricing_lookup(Arc::new(lookup));
        let attempt = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        let mut out = output(true);
        out.provenance = Some(provenance("served-model"));
        let settlement = build_settlement(
            &attempt.attempt,
            &attempt.basis,
            UsageDisposition::Final,
            AttemptOutcome::Succeeded,
            out.usage.clone(),
            out.provenance.as_ref(),
            "m",
            &*attempt.lookup,
        );
        assert_eq!(
            settlement.pricing_applicability,
            PricingApplicability::Rebound
        );
        assert_eq!(settlement.pricing.as_ref(), Some(&cheap));
        attempt.settle_output(&out).await.unwrap();
        let expected = (100.0 * 1.0 + 15.0 * 2.0 + 50.0 * 0.1 + 20.0 * 1.25) / 1e6;
        assert!((totals(&acct).complete_total_usd().unwrap() - expected).abs() < 1e-12);
    }

    #[tokio::test]
    async fn effective_provenance_is_recorded_on_settlement() {
        let (op, _acct) = fixture().await;
        let attempt = op
            .begin(DispatchMode::NonStreaming, metered())
            .await
            .unwrap();
        let mut out = output(true);
        out.provenance = Some(ChatOutputProvenance {
            provider: "openai".into(),
            model: "effective".into(),
            endpoint: "https://secret.example/v1?key=SECRET".into(),
            protocol: "responses".into(),
        });
        let settlement = build_settlement(
            &attempt.attempt,
            &attempt.basis,
            UsageDisposition::Final,
            AttemptOutcome::Succeeded,
            out.usage.clone(),
            out.provenance.as_ref(),
            "m",
            &*attempt.lookup,
        );
        assert_eq!(settlement.effective_model.as_deref(), Some("effective"));
        let json = serde_json::to_string(&settlement).unwrap();
        assert!(!json.contains("SECRET"), "endpoint must not be recorded");
        attempt.settle_output(&out).await.unwrap();
    }
}
