//! Provider account snapshots: plan, usage limits and credits that a provider
//! reports for the credential it runs under.
//!
//! A factory answers an account query with an [`AccountReport`]. HTTP factories
//! build the request with
//! [`HTTPLLMProviderFactory::account_request`](super::HTTPLLMProviderFactory::account_request)
//! and parse the response with
//! [`HTTPLLMProviderFactory::parse_account`](super::HTTPLLMProviderFactory::parse_account);
//! the host performs the request.

use serde::{Deserialize, Serialize};
use std::time::UNIX_EPOCH;

/// What a factory answers to an account query.
///
/// In-process only: the plugin boundary carries an optional request and a
/// [`ProviderAccount`], never this enum.
#[derive(Debug, Clone, PartialEq)]
pub enum AccountReport {
    /// This provider, or this credential, has no account data (most inference
    /// API keys, local models).
    Unsupported,
    /// The provider reported a snapshot.
    Account(ProviderAccount),
}

/// Plan, usage limits and credits for one credential.
#[cfg_attr(feature = "type-generation", typeshare::typeshare)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct ProviderAccount {
    /// Plan or tier as the provider names it (`"pro"`); `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Rolling or calendar limits; empty for providers that only report credits.
    #[serde(default)]
    pub limits: Vec<UsageLimit>,
    /// Credit balance, when the provider reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<CreditBalance>,
}

impl ProviderAccount {
    /// An empty snapshot: no plan, no limits, no credits.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the plan name.
    #[must_use]
    pub fn with_plan(mut self, plan: impl Into<String>) -> Self {
        self.plan = Some(plan.into());
        self
    }

    /// Appends a usage limit.
    pub fn push_limit(&mut self, limit: UsageLimit) {
        self.limits.push(limit);
    }

    /// Sets the credit balance.
    #[must_use]
    pub fn with_credits(mut self, credits: CreditBalance) -> Self {
        self.credits = Some(credits);
        self
    }
}

/// One usage allowance, reported as the percentage already consumed.
#[cfg_attr(feature = "type-generation", typeshare::typeshare)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageLimit {
    /// Provider-stable key: `"primary"`, `"five_hour"`, `"weekly"`, a metered feature id.
    pub id: String,
    /// Human label when the provider gives one; clients fall back to `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Model or product the limit applies to; `None` means all usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Percent of the allowance consumed; never negative, may exceed 100.
    pub used_percent: f64,
    /// Length of the window in seconds, when it has a fixed length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<u32>,
    /// Unix seconds (UTC) at which the window resets.
    #[cfg_attr(
        feature = "type-generation",
        typeshare(serialized_as = "Option<number>")
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// The limit blocks requests now.
    pub reached: bool,
}

impl UsageLimit {
    /// A limit at `used_percent`; negative and NaN values become 0.
    ///
    /// `reached` starts as `used_percent >= 100`; use
    /// [`with_reached`](Self::with_reached) when the provider reports its own flag.
    #[must_use]
    pub fn new(id: impl Into<String>, used_percent: f64) -> Self {
        let used_percent = used_percent.max(0.0);
        Self {
            id: id.into(),
            label: None,
            scope: None,
            used_percent,
            window_seconds: None,
            resets_at: None,
            reached: used_percent >= 100.0,
        }
    }

    /// Sets the human label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Sets the model or product the limit applies to.
    #[must_use]
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Sets the window length.
    #[must_use]
    pub fn with_window_seconds(mut self, seconds: u32) -> Self {
        self.window_seconds = Some(seconds);
        self
    }

    /// Sets the reset time in Unix seconds.
    #[must_use]
    pub fn with_resets_at(mut self, unix_seconds: i64) -> Self {
        self.resets_at = Some(unix_seconds);
        self
    }

    /// Replaces the derived `reached` with the provider's own flag.
    #[must_use]
    pub fn with_reached(mut self, reached: bool) -> Self {
        self.reached = reached;
        self
    }
}

/// A credit balance in the provider's unit.
#[cfg_attr(feature = "type-generation", typeshare::typeshare)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct CreditBalance {
    /// Decimal string (`"12701.63"`); `None` when the provider only reports has/has-not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    /// ISO 4217 code when the balance is money.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// The credential has no credit ceiling.
    #[serde(default)]
    pub unlimited: bool,
}

impl CreditBalance {
    /// A balance with no amount, no currency and a ceiling.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the decimal amount.
    #[must_use]
    pub fn with_balance(mut self, balance: impl Into<String>) -> Self {
        self.balance = Some(balance.into());
        self
    }

    /// Sets the ISO 4217 currency code.
    #[must_use]
    pub fn with_currency(mut self, currency: impl Into<String>) -> Self {
        self.currency = Some(currency.into());
        self
    }

    /// Sets whether the credential has no credit ceiling.
    #[must_use]
    pub fn with_unlimited(mut self, unlimited: bool) -> Self {
        self.unlimited = unlimited;
        self
    }
}

/// Parses a UTC RFC 3339 timestamp into Unix seconds.
///
/// Accepts `Z`, `+00:00` or no offset, with or without fractional seconds
/// (dropped). Returns `None` for any other offset or a malformed value.
#[must_use]
pub fn parse_rfc3339_utc(value: &str) -> Option<i64> {
    let time = humantime::parse_rfc3339_weak(value).ok()?;
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_across_the_plugin_boundary() {
        let mut account = ProviderAccount::new().with_plan("pro").with_credits(
            CreditBalance::new()
                .with_balance("12.50")
                .with_currency("USD"),
        );
        account.push_limit(UsageLimit::new("weekly:Api", 15.0).with_scope("Api"));
        account.push_limit(
            UsageLimit::new("primary", 120.0)
                .with_window_seconds(604_800)
                .with_resets_at(1_795_798_467),
        );

        let wire = serde_json::to_value(&account).expect("serialize");
        assert!(wire["limits"][0].get("resets_at").is_none());
        assert!(wire.get("plan").is_some());
        let back: ProviderAccount = serde_json::from_value(wire).expect("deserialize");

        assert_eq!(back, account);
        assert_eq!(back.limits[1].used_percent, 120.0);
        assert_eq!(back.limits[0].window_seconds, None);
    }

    #[test]
    fn empty_snapshot_deserializes_from_an_empty_object() {
        let account: ProviderAccount = serde_json::from_str("{}").expect("deserialize");
        assert_eq!(account, ProviderAccount::new());
    }

    #[test]
    fn reached_is_derived_when_the_provider_has_no_flag() {
        assert!(UsageLimit::new("five_hour", 100.0).reached);
        assert!(!UsageLimit::new("five_hour", 99.0).reached);
        assert!(
            !UsageLimit::new("five_hour", 100.0)
                .with_reached(false)
                .reached
        );
    }

    #[test]
    fn used_percent_is_never_negative() {
        assert_eq!(UsageLimit::new("x", -3.0).used_percent, 0.0);
        assert_eq!(UsageLimit::new("x", f64::NAN).used_percent, 0.0);
    }

    #[test]
    fn rfc3339_parses_utc_forms_only() {
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:10Z"), Some(10));
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:10+00:00"), Some(10));
        assert_eq!(
            parse_rfc3339_utc("2026-11-29T16:16:02.926+00:00"),
            parse_rfc3339_utc("2026-11-29T16:16:02Z")
        );
        assert_eq!(
            parse_rfc3339_utc("2026-11-25T22:09:59.989045+00:00"),
            parse_rfc3339_utc("2026-11-25T22:09:59Z")
        );
        assert_eq!(parse_rfc3339_utc("2026-11-25T22:09:59+02:00"), None);
        assert_eq!(parse_rfc3339_utc("not a date"), None);
    }
}
