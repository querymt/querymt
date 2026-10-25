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
//!   to it.
//! * A `None` component means "unavailable"; `Some(0.0)` means "known zero". A
//!   category with zero tokens is always known zero and needs no rate.
//! * Valuation is a pure function of its inputs: repeating it never
//!   accumulates charges.

use serde::{Deserialize, Serialize};

use super::types::ModelPricing;
use crate::Usage;

const TOKENS_PER_RATE_UNIT: f64 = 1_000_000.0;

/// A price dimension that a rate or a charge can refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingCategory {
    Input,
    Output,
    CacheRead,
    /// Aggregate cache writes, regardless of duration.
    CacheWrite,
    #[serde(rename = "cache_write_5m")]
    CacheWrite5m,
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
    /// Writes are priced per duration but the response gave no usable breakdown.
    MissingWriteDuration,
    /// Returned write-tier counts disagree with the aggregate write count.
    ContradictoryWriteDetail,
    /// The pricing is known to carry a modifier or charge that cannot be valued.
    UnsupportedDimension { dimension: String },
    /// A monetary amount was not representable as a finite value.
    AmountOverflow,
}

/// Returned cache-write duration breakdown. `None` is *absent*, which is
/// distinct from an explicit `Some(0)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheWriteTiers {
    pub five_minute: Option<u32>,
    pub one_hour: Option<u32>,
}

/// USD charge per category. See the module docs for `None` vs `Some(0.0)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ComponentCharges {
    pub input_usd: Option<f64>,
    /// Ordinary output plus reasoning, priced once.
    pub output_usd: Option<f64>,
    pub cache_read_usd: Option<f64>,
    /// Aggregate write charge, derived from tier counts when they are resolved.
    pub cache_write_usd: Option<f64>,
}

impl ComponentCharges {
    fn each(&self) -> [Option<f64>; 4] {
        [
            self.input_usd,
            self.output_usd,
            self.cache_read_usd,
            self.cache_write_usd,
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

/// How the returned tier counts relate to the aggregate write count.
enum WriteSplit {
    /// No usable breakdown; price the aggregate if authoritative.
    Aggregate,
    /// Consistent partition of the aggregate.
    Tiers {
        five_minute: u32,
        one_hour: u32,
    },
    Contradictory,
}

fn resolve_write_split(total: u32, tiers: CacheWriteTiers) -> WriteSplit {
    let known = u64::from(tiers.five_minute.unwrap_or(0)) + u64::from(tiers.one_hour.unwrap_or(0));
    let total = u64::from(total);
    match (tiers.five_minute, tiers.one_hour) {
        (None, None) => WriteSplit::Aggregate,
        _ if known > total => WriteSplit::Contradictory,
        (Some(five_minute), Some(one_hour)) if known == total => WriteSplit::Tiers {
            five_minute,
            one_hour,
        },
        (Some(_), Some(_)) => WriteSplit::Contradictory,
        // One tier absent: the remainder is provably zero only if the known
        // tier already covers the aggregate; otherwise the breakdown is unusable.
        (five_minute, one_hour) if known == total => WriteSplit::Tiers {
            five_minute: five_minute.unwrap_or(0),
            one_hour: one_hour.unwrap_or(0),
        },
        _ => WriteSplit::Aggregate,
    }
}

fn value_writes(
    pricing: &ModelPricing,
    total: u32,
    tiers: CacheWriteTiers,
    reasons: &mut Reasons,
) -> Option<f64> {
    match resolve_write_split(total, tiers) {
        WriteSplit::Contradictory => {
            reasons.note(IncompleteReason::ContradictoryWriteDetail);
            None
        }
        WriteSplit::Aggregate if total == 0 => Some(0.0),
        WriteSplit::Aggregate => {
            if pricing.cache_write_covers_all_durations {
                charge(
                    u64::from(total),
                    pricing.cache_write,
                    PricingCategory::CacheWrite,
                    reasons,
                )
            } else if pricing.has_write_rate() {
                reasons.note(IncompleteReason::MissingWriteDuration);
                None
            } else {
                reasons.note(IncompleteReason::MissingRate {
                    category: PricingCategory::CacheWrite,
                });
                None
            }
        }
        WriteSplit::Tiers {
            five_minute,
            one_hour,
        } => {
            // Evaluate both tiers so every reason is reported.
            let five = charge_tier(
                pricing,
                five_minute,
                pricing.cache_write_5m,
                PricingCategory::CacheWrite5m,
                reasons,
            );
            let hour = charge_tier(
                pricing,
                one_hour,
                pricing.cache_write_1h,
                PricingCategory::CacheWrite1h,
                reasons,
            );
            Some(five? + hour?)
        }
    }
}

fn charge_tier(
    pricing: &ModelPricing,
    tokens: u32,
    tier_rate: Option<f64>,
    tier_category: PricingCategory,
    reasons: &mut Reasons,
) -> Option<f64> {
    let (rate, category) = match tier_rate {
        Some(_) => (tier_rate, tier_category),
        // An aggregate rate substitutes only when pricing says it covers every duration.
        None if pricing.cache_write_covers_all_durations => {
            (pricing.cache_write, PricingCategory::CacheWrite)
        }
        None => (None, tier_category),
    };
    charge(u64::from(tokens), rate, category, reasons)
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
pub fn value_usage(
    pricing: Option<&ModelPricing>,
    usage: Option<&Usage>,
    write_tiers: CacheWriteTiers,
) -> UsageValuation {
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

    let components = match usage {
        None => {
            reasons.note(IncompleteReason::UsageUnavailable);
            ComponentCharges::default()
        }
        Some(u) => ComponentCharges {
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
            cache_write_usd: value_writes(rates, u.cache_write, write_tiers, &mut reasons),
        },
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
            cache_write_5m: Some(3.75),
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
        }
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
        let v = value_usage(
            Some(&pricing()),
            Some(&usage(0, 100, 900, 0, 0)),
            Default::default(),
        );
        approx(v.components.output_usd.unwrap(), 1000.0 * 15.0 / 1e6);
        assert!(v.is_complete());
    }

    #[test]
    fn prices_uncached_input_cache_reads_and_authoritative_aggregate_writes() {
        let mut p = pricing();
        p.cache_write_covers_all_durations = true;
        let v = value_usage(
            Some(&p),
            Some(&usage(100, 0, 0, 600, 300)),
            Default::default(),
        );
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
        // Even with an authoritative aggregate rate, tiers take precedence.
        let mut p = pricing();
        p.cache_write_covers_all_durations = true;
        let v = value_usage(Some(&p), Some(&usage(0, 0, 0, 0, 300)), tiers);
        approx(
            v.components.cache_write_usd.unwrap(),
            (200.0 * 3.75 + 100.0 * 6.0) / 1e6,
        );
        assert!(v.is_complete());
    }

    #[test]
    fn single_known_tier_covering_the_aggregate_implies_zero_other_tier() {
        let tiers = CacheWriteTiers {
            five_minute: Some(300),
            one_hour: None,
        };
        let v = value_usage(Some(&pricing()), Some(&usage(0, 0, 0, 0, 300)), tiers);
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
        let v = value_usage(Some(&p), Some(&usage(10, 10, 0, 0, 0)), Default::default());
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
        let v = value_usage(
            Some(&p),
            Some(&usage(100, 100, 0, 50, 0)),
            Default::default(),
        );
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
        let v = value_usage(Some(&p), Some(&usage(0, 0, 0, 500, 0)), Default::default());
        assert_eq!(v.components.cache_read_usd, Some(0.0));
        assert!(v.is_complete());
    }

    #[test]
    fn contradictory_write_detail_is_flagged_and_never_double_counted() {
        let tiers = CacheWriteTiers {
            five_minute: Some(200),
            one_hour: Some(200),
        };
        let v = value_usage(Some(&pricing()), Some(&usage(100, 0, 0, 0, 300)), tiers);
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
        let v = value_usage(Some(&pricing()), Some(&usage(0, 0, 0, 0, 300)), tiers);
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::ContradictoryWriteDetail]
        );
    }

    #[test]
    fn missing_write_detail_is_unresolved_not_assumed_cheapest() {
        let v = value_usage(
            Some(&pricing()),
            Some(&usage(100, 0, 0, 0, 300)),
            Default::default(),
        );
        assert_eq!(v.components.cache_write_usd, None);
        assert_eq!(v.incomplete, vec![IncompleteReason::MissingWriteDuration]);
        approx(v.known_subtotal_usd, 100.0 * 3.0 / 1e6);

        // A partial breakdown that does not cover the aggregate is also unusable.
        let tiers = CacheWriteTiers {
            five_minute: Some(100),
            one_hour: None,
        };
        let v = value_usage(Some(&pricing()), Some(&usage(0, 0, 0, 0, 300)), tiers);
        assert_eq!(v.incomplete, vec![IncompleteReason::MissingWriteDuration]);
    }

    #[test]
    fn writes_with_no_write_rate_at_all_report_a_missing_rate() {
        let p = ModelPricing {
            input: Some(1.0),
            output: Some(1.0),
            ..Default::default()
        };
        let v = value_usage(Some(&p), Some(&usage(0, 0, 0, 0, 10)), Default::default());
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::MissingRate {
                category: PricingCategory::CacheWrite
            }]
        );
    }

    #[test]
    fn missing_tier_rate_is_not_replaced_by_a_non_authoritative_aggregate() {
        let mut p = pricing();
        p.cache_write_1h = None;
        let tiers = CacheWriteTiers {
            five_minute: Some(200),
            one_hour: Some(100),
        };
        let v = value_usage(Some(&p), Some(&usage(0, 0, 0, 0, 300)), tiers);
        assert_eq!(v.components.cache_write_usd, None);
        assert_eq!(
            v.incomplete,
            vec![IncompleteReason::MissingRate {
                category: PricingCategory::CacheWrite1h
            }]
        );
    }

    #[test]
    fn known_unsupported_dimension_blocks_a_complete_total() {
        let mut p = pricing();
        p.unsupported_dimensions = vec!["long_context".into()];
        let v = value_usage(Some(&p), Some(&usage(10, 10, 0, 0, 0)), Default::default());
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
            let v = value_usage(Some(&p), Some(&usage(10, 10, 0, 0, 0)), Default::default());
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
        let v = value_usage(
            Some(&pricing()),
            Some(&usage(0, u32::MAX, u32::MAX, 0, 0)),
            Default::default(),
        );
        approx(
            v.components.output_usd.unwrap(),
            (u64::from(u32::MAX) * 2) as f64 * 15.0 / 1e6,
        );

        let mut p = pricing();
        p.output = Some(f64::MAX);
        let v = value_usage(
            Some(&p),
            Some(&usage(0, u32::MAX, u32::MAX, 0, 0)),
            Default::default(),
        );
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
            cache_write_covers_all_durations: true,
            ..Default::default()
        };
        let u = usage(u32::MAX, u32::MAX, 0, u32::MAX, u32::MAX);
        let v = value_usage(Some(&p), Some(&u), Default::default());
        assert!(v.known_subtotal_usd.is_finite());
        assert_eq!(v.complete_total_usd, None);
        assert!(v.incomplete.contains(&IncompleteReason::AmountOverflow));
    }

    #[test]
    fn repeated_valuation_is_identical() {
        let u = usage(10, 20, 30, 40, 0);
        let first = value_usage(Some(&pricing()), Some(&u), Default::default());
        let second = value_usage(Some(&pricing()), Some(&u), Default::default());
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
        let v = value_usage(None, Some(&u), Default::default());
        assert_eq!(v.billing, BillingClass::Unknown);
        assert_eq!(v.complete_total_usd, None);
        assert_eq!(v.incomplete, vec![IncompleteReason::PricingUnavailable]);

        let v = value_usage(Some(&pricing()), None, Default::default());
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
        let v = value_usage(Some(&p), Some(&usage(10, 10, 0, 0, 0)), Default::default());
        assert_eq!(v.billing, BillingClass::ZeroPriced);
        assert_eq!(v.complete_total_usd, Some(0.0));
    }

    #[test]
    fn valuation_round_trips_through_json() {
        let v = value_usage(
            Some(&pricing()),
            Some(&usage(1, 2, 3, 4, 5)),
            Default::default(),
        );
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"missing_write_duration\""));
        assert_eq!(serde_json::from_str::<UsageValuation>(&json).unwrap(), v);
    }
}
