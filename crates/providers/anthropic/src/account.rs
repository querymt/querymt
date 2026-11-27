//! Account snapshot from the Claude OAuth usage endpoint.
//!
//! Only Claude subscription (OAuth) tokens have one. The endpoint is what
//! Claude Code calls (`/api/oauth/usage`); it is not a published API, so every
//! field is read as optional.

use http::{Method, Request, Response, header::AUTHORIZATION};
use querymt::{
    error::LLMError,
    plugin::{CreditBalance, ProviderAccount, UsageLimit, account::parse_rfc3339_utc},
};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::{Anthropic, AuthType, detect_auth_type};

const FIVE_HOURS: u32 = 5 * 60 * 60;
const SEVEN_DAYS: u32 = 7 * 24 * 60 * 60;

/// Builds `GET /api/oauth/usage` on the base-URL host for an OAuth token;
/// declines for API keys.
///
/// # Errors
///
/// Returns a JSON or URL error for an unreadable configuration, and
/// [`LLMError::InvalidRequest`] for an invalid `claude_code_version`.
pub(crate) fn account_request(cfg: &str) -> Result<Option<Request<Vec<u8>>>, LLMError> {
    let cfg: Value = serde_json::from_str(cfg)?;
    let Some(api_key) = cfg.get("api_key").and_then(Value::as_str) else {
        return Ok(None);
    };
    let explicit_auth_type = cfg
        .get("auth_type")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    if detect_auth_type(api_key, explicit_auth_type) != AuthType::OAuth {
        return Ok(None);
    }

    let base_url = match cfg.get("base_url").and_then(Value::as_str) {
        Some(base_url) => Url::parse(base_url)?,
        None => Anthropic::default_base_url(),
    };
    let url = base_url.join("/api/oauth/usage")?;
    let version = cfg
        .get("claude_code_version")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or(crate::auth::CLAUDE_CODE_VERSION);
    crate::auth::validate_client_version(version)?;

    Ok(Some(
        Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .header(AUTHORIZATION, format!("Bearer {api_key}"))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("anthropic-version", "2023-06-01")
            .header(
                "user-agent",
                format!("claude-cli/{version} (external, cli)"),
            )
            .header("accept", "application/json")
            .body(Vec::new())?,
    ))
}

#[derive(Deserialize)]
struct UsageBody {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
    limits: Option<Vec<ScopedLimit>>,
    spend: Option<Spend>,
}

#[derive(Deserialize)]
struct Window {
    utilization: Option<f64>,
    resets_at: Option<String>,
    /// Shape undocumented; any non-null value means the window blocks.
    locked_reason: Option<Value>,
}

#[derive(Deserialize)]
struct ScopedLimit {
    kind: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<LimitScope>,
}

#[derive(Deserialize)]
struct LimitScope {
    model: Option<ScopeModel>,
}

#[derive(Deserialize)]
struct ScopeModel {
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct Spend {
    /// Shape undocumented (observed `null`); read only when it matches the
    /// money object the endpoint uses for `spend.used`.
    balance: Option<Value>,
}

#[derive(Deserialize)]
struct Money {
    amount_minor: i64,
    currency: Option<String>,
    exponent: u32,
}

/// Maps the usage body to a snapshot.
///
/// # Errors
///
/// Returns [`LLMError::ResponseFormatError`] when the body is not a JSON
/// object or a mapped field has the wrong type.
pub(crate) fn parse_account(resp: &Response<Vec<u8>>) -> Result<ProviderAccount, LLMError> {
    let body: UsageBody =
        serde_json::from_slice(resp.body()).map_err(|e| LLMError::ResponseFormatError {
            message: format!("unreadable Claude usage response: {e}"),
            raw_response: String::new(),
        })?;

    let mut account = ProviderAccount::new();
    for (id, seconds, window) in [
        ("five_hour", FIVE_HOURS, body.five_hour),
        ("seven_day", SEVEN_DAYS, body.seven_day),
    ] {
        let Some(window) = window else { continue };
        let Some(utilization) = window.utilization else {
            continue;
        };
        let mut limit = UsageLimit::new(id, utilization).with_window_seconds(seconds);
        if let Some(reset) = window.resets_at.as_deref().and_then(parse_rfc3339_utc) {
            limit = limit.with_resets_at(reset);
        }
        if window.locked_reason.is_some_and(|reason| !reason.is_null()) {
            limit = limit.with_reached(true);
        }
        account.push_limit(limit);
    }
    for scoped in body.limits.into_iter().flatten() {
        let Some(model) = scoped
            .scope
            .and_then(|scope| scope.model)
            .and_then(|model| model.display_name)
        else {
            continue;
        };
        let (Some(kind), Some(percent)) = (scoped.kind, scoped.percent) else {
            continue;
        };
        let mut limit = UsageLimit::new(kind, percent).with_scope(model);
        if let Some(reset) = scoped.resets_at.as_deref().and_then(parse_rfc3339_utc) {
            limit = limit.with_resets_at(reset);
        }
        account.push_limit(limit);
    }
    if let Some(balance) = body
        .spend
        .and_then(|spend| spend.balance)
        .and_then(|value| serde_json::from_value::<Money>(value).ok())
    {
        let mut credits = CreditBalance::new().with_balance(format_minor(&balance));
        if let Some(currency) = balance.currency {
            credits = credits.with_currency(currency);
        }
        account = account.with_credits(credits);
    }
    Ok(account)
}

/// Formats a minor-unit amount (`1234`, exponent 2 → `"12.34"`).
fn format_minor(money: &Money) -> String {
    let sign = if money.amount_minor < 0 { "-" } else { "" };
    let abs = money.amount_minor.unsigned_abs();
    let Some(scale) = 10_u64
        .checked_pow(money.exponent)
        .filter(|_| money.exponent > 0)
    else {
        return format!("{sign}{abs}");
    };
    let width = usize::try_from(money.exponent).unwrap_or(0);
    format!("{sign}{}.{:0width$}", abs / scale, abs % scale)
}
