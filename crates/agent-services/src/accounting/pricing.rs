//! Catalog pricing lookups and billing classification.
//!
//! These were `SessionProvider::get_pricing` and `BillingBasis::resolve`; they
//! are free functions so the plain `BillingBasis` data type can live in core.

use super::BillingBasis;
use crate::model_info::get_model_info;
use querymt::providers::{BillingClass, ModelPricing};

/// Subscription/OAuth providers whose usage is never billed per token.
pub fn is_non_billable_oauth_provider(provider: &str) -> bool {
    matches!(provider, "codex" | "kimi-code")
}

/// Catalog pricing of a `(provider, model)` pair. Non-billable OAuth
/// providers return `None`.
pub fn pricing_for(provider: &str, model: &str) -> Option<ModelPricing> {
    if is_non_billable_oauth_provider(provider) {
        return None;
    }
    get_model_info(provider, model).map(|info| info.pricing)
}

/// Classify `provider/model` from the current catalog. Subscription and
/// OAuth-backed providers are `Excluded` and never carry catalog pricing.
pub fn resolve_billing_basis(provider: &str, model: &str, oauth_backed: bool) -> BillingBasis {
    if oauth_backed || is_non_billable_oauth_provider(provider) {
        return BillingBasis {
            billing: BillingClass::Excluded,
            pricing: None,
        };
    }
    let pricing = get_model_info(provider, model).map(|info| info.pricing);
    let billing = match &pricing {
        None => BillingClass::Unknown,
        Some(p) if p.is_explicit_zero() => BillingClass::ZeroPriced,
        Some(_) => BillingClass::Metered,
    };
    BillingBasis { billing, pricing }
}
