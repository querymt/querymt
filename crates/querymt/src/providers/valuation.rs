//! Pure, usage-aware valuation of one provider attempt.
//!
//! A [`UsageValuation`] is a *supported token-cost estimate*, not a reconciled
//! provider invoice. It reports per-category USD components, a finite
//! `known_subtotal_usd`, and an optional `complete_total_usd` that is `Some`
//! only when every category with nonzero usage could be priced.
//!
//! # Conventions
//!
//! * Rates are USD per million tokens.
//! * Top-level usage categories are exclusive (see [`Usage`]): ordinary output
//!   plus reasoning is priced once at the output rate, `input_tokens` is the
//!   uncached portion, and `cache_read` / `cache_write` are separate.
//! * Cache-write duration tiers *partition* `cache_write`; they are never added
//!   to it. The catalog `cache_write` rate is the base write rate: it prices
//!   five-minute writes and writes with no (or no one-hour) breakdown. Only
//!   reported one-hour writes need the explicit `cache_write_1h` rate, which
//!   the catalog does not publish; without it just that portion is missing.
//! * A `None` component means "unavailable"; `Some(0.0)` means "known zero". A
//!   category with zero tokens is always known zero and needs no rate.
//! * Valuation is a pure function of its inputs: repeating it never
//!   accumulates charges.

use serde::{Deserialize, Serialize};

use super::types::ModelPricing;
use crate::{CacheWriteTiers, Usage};

const TOKENS_PER_RATE_UNIT: f64 = 1_000_000.0;

/// A price dimension that a rate or a charge can refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingCategory {
    Input,
    Output,
    CacheRead,
    /// Base-rate cache writes (five-minute or unknown duration).
    CacheWrite,
    /// Reported one-hour cache writes.
    #[serde(rename = "cache_write_1h")]
    CacheWrite1h,
}

/// How an attempt is billed, independent of whether it could be priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingClass {
    /// Metered API usage with at least one nonzero applicable rate.
    Metered,
    /// Applicable rates are present and explicitly zero.
    ZeroPriced,
    /// Excluded by provider/authentication policy (e.g. subscription/OAuth).
    /// Never a known zero-dollar invoice.
    Excluded,
    /// No applicable pricing basis is known.
    Unknown,
}

/// Why a valuation cannot claim a complete total.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IncompleteReason {
    /// The attempt reported no usage.
    UsageUnavailable,
    /// No applicable pricing basis exists.
    PricingUnavailable,
    /// A category with nonzero usage has no rate.
    MissingRate { category: PricingCategory },
    /// A category with nonzero usage has a negative or non-finite rate.
    InvalidRate { category: PricingCategory },
    /// Returned write-tier counts disagree with the aggregate write count.
    ContradictoryWriteDetail,
    /// The pricing is known to carry a modifier or charge that cannot be valued.
    UnsupportedDimension { dimension: String },
    /// A monetary amount was not representable as a finite value.
    AmountOverflow,
    /// The provider served a different model than the one the pricing was
    /// bound for, and no applicable pricing basis is known for it.
    PricingInapplicable { effective_model: String },
}

/// USD charge per category. See the module docs for `None` vs `Some(0.0)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ComponentCharges {
    pub input_usd: Option<f64>,
    /// Ordinary output plus reasoning, priced once.
    pub output_usd: Option<f64>,
    pub cache_read_usd: Option<f64>,
    /// Base-rate writes: `cache_write` minus reported one-hour writes.
    pub cache_write_usd: Option<f64>,
    /// Reported one-hour writes at the explicit one-hour rate.
    pub cache_write_1h_usd: Option<f64>,
}

impl ComponentCharges {
    fn each(&self) -> [Option<f64>; 5] {
        [
            self.input_usd,
            self.output_usd,
            self.cache_read_usd,
            self.cache_write_usd,
            self.cache_write_1h_usd,
        ]
    }
}

/// Result of valuing one attempt's usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageValuation {
    pub billing: BillingClass,
    pub components: ComponentCharges,
    /// Sum of independently known components; always finite and nonnegative.
    pub known_subtotal_usd: f64,
    /// `Some` only for metered/zero-priced billing with no incomplete reasons.
    pub complete_total_usd: Option<f64>,
    pub incomplete: Vec<IncompleteReason>,
}

impl UsageValuation {
    /// An attempt excluded from metered API cost. Carries no charges and no
    /// incompleteness: it is out of scope, not unknown.
    pub fn excluded() -> Self {
        Self {
            billing: BillingClass::Excluded,
            components: ComponentCharges::default(),
            known_subtotal_usd: 0.0,
            complete_total_usd: None,
            incomplete: Vec::new(),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.complete_total_usd.is_some()
    }
}

/// Insertion-ordered, de-duplicated reasons (the list is tiny).
#[derive(Default)]
struct Reasons(Vec<IncompleteReason>);

impl Reasons {
    fn note(&mut self, reason: IncompleteReason) {
        if !self.0.contains(&reason) {
            self.0.push(reason);
        }
    }
}

pub(super) fn is_valid_rate(rate: f64) -> bool {
    rate.is_finite() && rate >= 0.0
}

fn charge(
    tokens: u64,
    rate: Option<f64>,
    category: PricingCategory,
    reasons: &mut Reasons,
) -> Option<f64> {
    if tokens == 0 {
        return Some(0.0);
    }
    let Some(rate) = rate else {
        reasons.note(IncompleteReason::MissingRate { category });
        return None;
    };
    if !is_valid_rate(rate) {
        reasons.note(IncompleteReason::InvalidRate { category });
        return None;
    }
    // Scale the rate first so a large rate cannot overflow before division.
    let amount = tokens as f64 * (rate / TOKENS_PER_RATE_UNIT);
    if amount.is_finite() {
        Some(amount)
    } else {
        reasons.note(IncompleteReason::AmountOverflow);
        None
    }
}

/// Split `cache_write` into (base-rate tokens, one-hour tokens), or `None`
/// when the returned breakdown contradicts the aggregate.
///
/// Absent detail is not a contradiction: writes without a one-hour count are
/// base-rate writes. A five-minute count is only checked for consistency;
/// any remainder of unknown duration is still priced at the base rate.
fn split_writes(total: u32, tiers: CacheWriteTiers) -> Option<(u64, u64)> {
    let total = u64::from(total);
    let five = tiers.five_minute.map(u64::from);
    let hour = tiers.one_hour.map(u64::from);
    let consistent = match (five, hour) {
        (Some(f), Some(h)) => f + h == total,
        (Some(t), None) | (None, Some(t)) => t <= total,
        (None, None) => true,
    };
    consistent.then(|| {
        let hour = hour.unwrap_or(0);
        (total - hour, hour)
    })
}

/// Value cache writes as (base-rate charge, one-hour charge).
fn value_writes(
    pricing: &ModelPricing,
    total: u32,
    tiers: CacheWriteTiers,
    reasons: &mut Reasons,
) -> (Option<f64>, Option<f64>) {
    let Some((base, hour)) = split_writes(total, tiers) else {
        reasons.note(IncompleteReason::ContradictoryWriteDetail);
        return (None, None);
    };
    // Evaluate both so every reason is reported. The one-hour portion never
    // falls back to the base rate: that would understate it.
    let base_usd = charge(
        base,
        pricing.cache_write,
        PricingCategory::CacheWrite,
        reasons,
    );
    let hour_usd = charge(
        hour,
        pricing.cache_write_1h,
        PricingCategory::CacheWrite1h,
        reasons,
    );
    (base_usd, hour_usd)
}

fn classify(pricing: Option<&ModelPricing>) -> BillingClass {
    match pricing {
        None => BillingClass::Unknown,
        Some(p) if p.is_explicit_zero() => BillingClass::ZeroPriced,
        Some(_) => BillingClass::Metered,
    }
}

/// Value one attempt's usage against an applicable pricing basis.
///
/// * `pricing = None` yields [`BillingClass::Unknown`] and no complete total.
/// * `usage = None` yields no charges and [`IncompleteReason::UsageUnavailable`].
///
/// For excluded billing use [`UsageValuation::excluded`] instead.
pub fn value_usage(pricing: Option<&ModelPricing>, usage: Option<&Usage>) -> UsageValuation {
    let billing = classify(pricing);
    let mut reasons = Reasons::default();
    let default_pricing;
    let rates = match pricing {
        Some(p) => p,
        None => {
            reasons.note(IncompleteReason::PricingUnavailable);
            default_pricing = ModelPricing::default();
            &default_pricing
        }
    };
    for dimension in &rates.unsupported_dimensions {
        reasons.note(IncompleteReason::UnsupportedDimension {
            dimension: dimension.clone(),
        });
    }

    if let Some(u) = usage {
        // Prompt size is the sum of the exclusive prompt-side categories.
        let prompt_tokens =
            u64::from(u.input_tokens) + u64::from(u.cache_read) + u64::from(u.cache_write);
        for name in rates.triggered_dimensions(prompt_tokens, u64::from(u.reasoning_tokens)) {
            reasons.note(IncompleteReason::UnsupportedDimension {
                dimension: name.to_string(),
            });
        }
    }

    let components = match usage {
        None => {
            reasons.note(IncompleteReason::UsageUnavailable);
            ComponentCharges::default()
        }
        Some(u) => {
            let (cache_write_usd, cache_write_1h_usd) = value_writes(
                rates,
                u.cache_write,
                u.cache_write_tiers.unwrap_or_default(),
                &mut reasons,
            );
            ComponentCharges {
                input_usd: charge(
                    u64::from(u.input_tokens),
                    rates.input,
                    PricingCategory::Input,
                    &mut reasons,
                ),
                // Widened before adding: u32 + u32 cannot wrap in u64.
                output_usd: charge(
                    u64::from(u.output_tokens) + u64::from(u.reasoning_tokens),
                    rates.output,
                    PricingCategory::Output,
                    &mut reasons,
                ),
                cache_read_usd: charge(
                    u64::from(u.cache_read),
                    rates.cache_read,
                    PricingCategory::CacheRead,
                    &mut reasons,
                ),
                cache_write_usd,
                cache_write_1h_usd,
            }
        }
    };

    // With no pricing basis the per-category "missing rate" reasons are noise;
    // the single PricingUnavailable reason is the cause.
    let incomplete = if pricing.is_none() {
        reasons
            .0
            .into_iter()
            .filter(|r| !matches!(r, IncompleteReason::MissingRate { .. }))
            .collect()
    } else {
        reasons.0
    };

    let mut incomplete: Vec<IncompleteReason> = incomplete;
    let mut known_subtotal_usd: f64 = components.each().into_iter().flatten().sum();
    if !known_subtotal_usd.is_finite() {
        known_subtotal_usd = f64::MAX;
        if !incomplete.contains(&IncompleteReason::AmountOverflow) {
            incomplete.push(IncompleteReason::AmountOverflow);
        }
    }

    let complete_total_usd = (matches!(billing, BillingClass::Metered | BillingClass::ZeroPriced)
        && incomplete.is_empty())
    .then_some(known_subtotal_usd);

    UsageValuation {
        billing,
        components,
        known_subtotal_usd,
        complete_total_usd,
        incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing() -> ModelPricing {
        ModelPricing {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: Some(0.3),
            cache_write: Some(3.75),
            cache_write_1h: Some(6.0),
            ..Default::default()
        }
    }

    fn usage(input: u32, output: u32, reasoning: u32, read: u32, write: u32) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: reasoning,
            cache_read: read,
            cache_write: write,
            cache_write_tiers: None,
        }
    }

    fn tiered(mut usage: Usage, tiers: CacheWriteTiers) -> Usage {
        usage.cache_write_tiers = Some(tiers);
        usage
    }

    fn approx(actual: f64, expected: f64) {
        // Relative tolerance: rate-first scaling differs from the test's
        // expression order by a few ulps at large magnitudes.
        assert!(
            (actual - expected).abs() <= 1e-12 * expected.abs().max(1.0),
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn reasoning_is_priced_once_at_the_output_rate() {
        let v = value_usage(Some(&pricing()), Some(&usage(0, 100, 900, 0, 0)));
        approx(v.components.output_usd.unwrap(), 1000.0 * 15.0 / 1e6);
        assert!(v.is_complete());
    }

    #[test]
    fn prices_uncached_input_cache_reads_and_base_rate_writes() {
        // No duration breakdown: all writes are priced at the catalog base rate.
        let p = pricing();
        let v = value_usage(Some(&p), Some(&usage(100, 0, 0, 600, 300)));
        approx(v.components.input_usd.unwrap(), 100.0 * 3.0 / 1e6);
        approx(v.components.cache_read_usd.unwrap(), 600.0 * 0.3 / 1e6);
        approx(v.components.cache_write_usd.unwrap(), 300.0 * 3.75 / 1e6);
        approx(
            v.complete_total_usd.unwrap(),
            (300.0 + 180.0 + 1125.0) / 1e6,
        );
    }

    #[test]
    fn mixed_write_tiers_use_tier_rates_and_do_not_double_count() {
        let tiers = CacheWriteTiers {
            five_minute: Some(200),
            one_hour: Some(100),
        };
        let v = value_usage(
            Some(&pricing()),
            Some(&tiered(usage(0, 0, 0, 0, 300), tiers)),
        );
        approx(v.components.cache_write_usd.unwrap(), 200.0 * 3.75 / 1e6);
        approx(v.components.cache_write_1h_usd.unwrap(), 100.0 * 6.0 / 1e6);
        approx(
            v.complete_total_usd.unwrap(),
            (200.0 * 3.75 + 100.0 * 6.0) / 1e6,
        );
    }

    #[test]
    fn single_known_tier_covering_the_aggregate_implies_zero_other_tier() {
        let tiers = CacheWriteTiers {
            five_minute: Some(300),
            one_hour: None,
        };
        let v = value_usage(
            Some(&pricing()),
            Some(&tiered(usage(0, 0, 0, 0, 300), tiers)),
        );
        approx(v.components.cache_write_usd.unwrap(), 300.0 * 3.75 / 1e6);
        assert!(v.is_complete());
    }

    #[test]
    fn unused_missing_rate_does_not_make_the_estimate_incomplete() {
        let p = ModelPricing {
            input: Some(3.0),
            output: Some(15.0),
            ..Default::default()
        };
        let v = value_usage(Some(&p), Some(&usage(10, 10, 0, 0, 0)));
        assert!(v.is_complete());
        assert_eq!(v.components.cache_write_usd, Some(0.0));
    }

    #[test]
    fn used_missing_rate_keeps_known_subtotal_but_no_complete_total() {
        let p = ModelPricing {
            input: Some(3.0),
            output: Some(15.0),
            ..Default::default()
        };
        let v = value_usage(Some(&p), Some(&usage(100, 100, 0, 50, 0)));
        approx(v.known_subtotal_usd, (300.0 + 1500.0) / 1e6);
        assert_eq!(v.complete_total_usd, None);
        assert_eq!(v.components.cache_read_usd, None);
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::MissingRate {
                category: PricingCategory::CacheRead
            }]
        );
    }

    #[test]
    fn explicit_zero_rate_is_known_zero_not_missing() {
        let p = ModelPricing {
            input: Some(1.0),
            output: Some(1.0),
            cache_read: Some(0.0),
            ..Default::default()
        };
        let v = value_usage(Some(&p), Some(&usage(0, 0, 0, 500, 0)));
        assert_eq!(v.components.cache_read_usd, Some(0.0));
        assert!(v.is_complete());
    }

    #[test]
    fn contradictory_write_detail_is_flagged_and_never_double_counted() {
        let tiers = CacheWriteTiers {
            five_minute: Some(200),
            one_hour: Some(200),
        };
        let v = value_usage(
            Some(&pricing()),
            Some(&tiered(usage(100, 0, 0, 0, 300), tiers)),
        );
        assert_eq!(v.components.cache_write_usd, None);
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::ContradictoryWriteDetail]
        );
        // Independently calculable charges survive.
        approx(v.known_subtotal_usd, 100.0 * 3.0 / 1e6);

        // Tiers that undershoot the aggregate when both are present also contradict.
        let tiers = CacheWriteTiers {
            five_minute: Some(100),
            one_hour: Some(100),
        };
        let v = value_usage(
            Some(&pricing()),
            Some(&tiered(usage(0, 0, 0, 0, 300), tiers)),
        );
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::ContradictoryWriteDetail]
        );
    }

    #[test]
    fn writes_without_one_hour_detail_need_only_the_catalog_rate() {
        // Catalog-shaped pricing: base write rate, no one-hour rate.
        let mut p = pricing();
        p.cache_write_1h = None;
        let v = value_usage(Some(&p), Some(&usage(100, 0, 0, 0, 300)));
        approx(v.components.cache_write_usd.unwrap(), 300.0 * 3.75 / 1e6);
        assert_eq!(v.components.cache_write_1h_usd, Some(0.0));
        assert!(v.is_complete());

        // A five-minute count below the aggregate leaves a remainder of
        // unknown duration, still priced at the base rate.
        let tiers = CacheWriteTiers {
            five_minute: Some(100),
            one_hour: None,
        };
        let v = value_usage(Some(&p), Some(&tiered(usage(0, 0, 0, 0, 300), tiers)));
        approx(v.components.cache_write_usd.unwrap(), 300.0 * 3.75 / 1e6);
        assert!(v.is_complete());

        // An explicit zero one-hour count needs no one-hour rate.
        let tiers = CacheWriteTiers {
            five_minute: Some(300),
            one_hour: Some(0),
        };
        let v = value_usage(Some(&p), Some(&tiered(usage(0, 0, 0, 0, 300), tiers)));
        assert!(v.is_complete());
    }

    #[test]
    fn writes_with_no_write_rate_at_all_report_a_missing_rate() {
        let p = ModelPricing {
            input: Some(1.0),
            output: Some(1.0),
            ..Default::default()
        };
        let v = value_usage(Some(&p), Some(&usage(0, 0, 0, 0, 10)));
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::MissingRate {
                category: PricingCategory::CacheWrite
            }]
        );
    }

    #[test]
    fn one_hour_writes_without_a_one_hour_rate_are_partial_not_base_priced() {
        let mut p = pricing();
        p.cache_write_1h = None;
        let tiers = CacheWriteTiers {
            five_minute: Some(200),
            one_hour: Some(100),
        };
        let v = value_usage(Some(&p), Some(&tiered(usage(10, 0, 0, 0, 300), tiers)));
        // The five-minute portion and other components stay known.
        approx(v.components.cache_write_usd.unwrap(), 200.0 * 3.75 / 1e6);
        assert_eq!(v.components.cache_write_1h_usd, None);
        approx(v.known_subtotal_usd, (10.0 * 3.0 + 200.0 * 3.75) / 1e6);
        assert_eq!(v.complete_total_usd, None);
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::MissingRate {
                category: PricingCategory::CacheWrite1h
            }]
        );
    }

    #[test]
    fn registry_dimensions_block_completeness_only_when_usage_triggers_them() {
        use crate::providers::types::{DimensionApplicability, PricingDimension};
        let mut p = pricing();
        p.dimensions = vec![PricingDimension {
            name: "context_over_200k".into(),
            applies: DimensionApplicability::ContextOver { tokens: 200_000 },
        }];
        // Prompt = input + cache reads + cache writes, at the threshold.
        let below = value_usage(Some(&p), Some(&usage(100_000, 10, 0, 100_000, 0)));
        assert!(below.is_complete());
        let above = value_usage(Some(&p), Some(&usage(100_001, 10, 0, 100_000, 0)));
        assert_eq!(above.complete_total_usd, None);
        assert!(above.known_subtotal_usd > 0.0);
        assert_eq!(
            above.incomplete,
            vec![IncompleteReason::UnsupportedDimension {
                dimension: "context_over_200k".into()
            }]
        );
    }

    #[test]
    fn known_unsupported_dimension_blocks_a_complete_total() {
        let mut p = pricing();
        p.unsupported_dimensions = vec!["long_context".into()];
        let v = value_usage(Some(&p), Some(&usage(10, 10, 0, 0, 0)));
        assert_eq!(v.complete_total_usd, None);
        assert!(v.known_subtotal_usd > 0.0);
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::UnsupportedDimension {
                dimension: "long_context".into()
            }]
        );
    }

    #[test]
    fn invalid_rates_are_rejected_without_a_complete_total() {
        for bad in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut p = pricing();
            p.output = Some(bad);
            let v = value_usage(Some(&p), Some(&usage(10, 10, 0, 0, 0)));
            assert_eq!(v.complete_total_usd, None, "rate {bad}");
            assert_eq!(v.components.output_usd, None);
            assert!(v.known_subtotal_usd.is_finite() && v.known_subtotal_usd >= 0.0);
            assert_eq!(
                v.incomplete,
                vec![IncompleteReason::InvalidRate {
                    category: PricingCategory::Output
                }]
            );
        }
    }

    #[test]
    fn token_sums_are_widened_and_monetary_overflow_is_explicit() {
        // u32::MAX + u32::MAX must not wrap.
        let v = value_usage(Some(&pricing()), Some(&usage(0, u32::MAX, u32::MAX, 0, 0)));
        approx(
            v.components.output_usd.unwrap(),
            (u64::from(u32::MAX) * 2) as f64 * 15.0 / 1e6,
        );

        let mut p = pricing();
        p.output = Some(f64::MAX);
        let v = value_usage(Some(&p), Some(&usage(0, u32::MAX, u32::MAX, 0, 0)));
        assert_eq!(v.components.output_usd, None);
        assert_eq!(v.complete_total_usd, None);
        assert!(v.incomplete.contains(&IncompleteReason::AmountOverflow));
        assert!(v.known_subtotal_usd.is_finite());
    }

    #[test]
    fn subtotal_overflow_is_clamped_and_reported() {
        let p = ModelPricing {
            // Each component is ~0.54 * f64::MAX; their sum overflows.
            input: Some(f64::MAX / 8_000.0),
            output: Some(f64::MAX / 8_000.0),
            cache_read: Some(f64::MAX / 8_000.0),
            cache_write: Some(f64::MAX / 8_000.0),
            ..Default::default()
        };
        let u = usage(u32::MAX, u32::MAX, 0, u32::MAX, u32::MAX);
        let v = value_usage(Some(&p), Some(&u));
        assert!(v.known_subtotal_usd.is_finite());
        assert_eq!(v.complete_total_usd, None);
        assert!(v.incomplete.contains(&IncompleteReason::AmountOverflow));
    }

    #[test]
    fn repeated_valuation_is_identical() {
        let u = usage(10, 20, 30, 40, 0);
        let first = value_usage(Some(&pricing()), Some(&u));
        let second = value_usage(Some(&pricing()), Some(&u));
        assert_eq!(first, second);
        approx(first.components.cache_read_usd.unwrap(), 40.0 * 0.3 / 1e6);
    }

    #[test]
    fn excluded_billing_is_neither_zero_invoice_nor_incomplete() {
        let v = UsageValuation::excluded();
        assert_eq!(v.billing, BillingClass::Excluded);
        assert_eq!(v.complete_total_usd, None);
        assert!(v.incomplete.is_empty());
        assert_eq!(v.known_subtotal_usd, 0.0);
    }

    #[test]
    fn unknown_pricing_and_missing_usage_are_distinct_reasons() {
        let u = usage(10, 10, 0, 0, 0);
        let v = value_usage(None, Some(&u));
        assert_eq!(v.billing, BillingClass::Unknown);
        assert_eq!(v.complete_total_usd, None);
        assert_eq!(v.incomplete, vec![IncompleteReason::PricingUnavailable]);

        let v = value_usage(Some(&pricing()), None);
        assert_eq!(v.complete_total_usd, None);
        assert_eq!(v.known_subtotal_usd, 0.0);
        assert_eq!(v.incomplete, vec![IncompleteReason::UsageUnavailable]);
    }

    #[test]
    fn explicit_zero_pricing_is_zero_priced_and_complete() {
        let p = ModelPricing {
            input: Some(0.0),
            output: Some(0.0),
            ..Default::default()
        };
        let v = value_usage(Some(&p), Some(&usage(10, 10, 0, 0, 0)));
        assert_eq!(v.billing, BillingClass::ZeroPriced);
        assert_eq!(v.complete_total_usd, Some(0.0));
    }

    #[test]
    fn valuation_round_trips_through_json() {
        let v = value_usage(Some(&pricing()), Some(&usage(1, 2, 3, 4, 5)));
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"cache_write_1h_usd\""));
        assert_eq!(serde_json::from_str::<UsageValuation>(&json).unwrap(), v);
    }
}
