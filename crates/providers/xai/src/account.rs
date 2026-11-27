//! Account snapshot from the Grok CLI credits endpoint.
//!
//! Only xAI OAuth (Grok subscription) tokens have one. The endpoint lives on
//! the Grok CLI proxy, not on `api.x.ai`, and is what the Grok CLI calls; it is
//! not a published API, so every field is read as optional.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use http::{Method, Request, Response, header::AUTHORIZATION};
use querymt::{
    error::LLMError,
    plugin::{CreditBalance, ProviderAccount, UsageLimit, account::parse_rfc3339_utc},
};
use serde::Deserialize;
use serde_json::Value;

/// The host of the credits endpoint, declared through `account_hosts`.
pub(crate) const ACCOUNT_HOST: &str = "cli-chat-proxy.grok.com";
const ACCOUNT_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const XAI_OAUTH_ISSUER: &str = "https://auth.x.ai";

#[derive(Deserialize)]
struct AccountConfig {
    #[serde(default)]
    api_key: String,
}

/// Returns the subject claim of an xAI OAuth access token, or `None` for any
/// other credential (API keys, tokens from other issuers). The runtime does
/// not set `auth_type`, so the token itself is the signal.
fn xai_oauth_subject(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    if claims.get("iss").and_then(Value::as_str) != Some(XAI_OAUTH_ISSUER) {
        return None;
    }
    claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Builds the credits request for an xAI OAuth token; declines otherwise.
///
/// # Errors
///
/// Returns a JSON error for an unreadable configuration.
pub(crate) fn account_request(cfg: &str) -> Result<Option<Request<Vec<u8>>>, LLMError> {
    let cfg: AccountConfig = serde_json::from_str(cfg)?;
    let Some(user_id) = xai_oauth_subject(&cfg.api_key) else {
        return Ok(None);
    };

    // TODO: these two headers copy the Grok CLI. In one probe (2026-11-25) the
    // endpoint answered with the bearer token alone; consider identifying as
    // QueryMT in `X-XAI-Token-Auth`, or dropping both headers.
    Ok(Some(
        Request::builder()
            .method(Method::GET)
            .uri(ACCOUNT_URL)
            .header(AUTHORIZATION, format!("Bearer {}", cfg.api_key))
            .header("X-XAI-Token-Auth", "xai-grok-cli")
            .header("x-userid", user_id)
            .header("accept", "application/json")
            .body(Vec::new())?,
    ))
}

#[derive(Deserialize)]
struct BillingBody {
    config: Option<BillingConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingConfig {
    current_period: Option<Period>,
    credit_usage_percent: Option<f64>,
    product_usage: Option<Vec<ProductUsage>>,
    prepaid_balance: Option<Cents>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Period {
    #[serde(rename = "type")]
    kind: Option<String>,
    start: Option<String>,
    end: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductUsage {
    product: Option<String>,
    usage_percent: Option<f64>,
}

#[derive(Deserialize)]
struct Cents {
    val: Option<i64>,
}

/// Maps the credits body to a snapshot.
///
/// # Errors
///
/// Returns [`LLMError::ResponseFormatError`] when the body is not a JSON
/// object or a mapped field has the wrong type.
pub(crate) fn parse_account(resp: &Response<Vec<u8>>) -> Result<ProviderAccount, LLMError> {
    let body: BillingBody =
        serde_json::from_slice(resp.body()).map_err(|e| LLMError::ResponseFormatError {
            message: format!("unreadable Grok credits response: {e}"),
            raw_response: String::new(),
        })?;
    let mut account = ProviderAccount::new();
    let Some(config) = body.config else {
        return Ok(account);
    };

    let period = config.current_period.unwrap_or(Period {
        kind: None,
        start: None,
        end: None,
    });
    let start = period.start.as_deref().and_then(parse_rfc3339_utc);
    let end = period.end.as_deref().and_then(parse_rfc3339_utc);
    let window = match (start, end) {
        (Some(start), Some(end)) => u32::try_from(end.saturating_sub(start)).ok(),
        _ => None,
    };
    let with_period = |mut limit: UsageLimit| {
        if let Some(label) = &period.kind {
            limit = limit.with_label(label.clone());
        }
        if let Some(seconds) = window {
            limit = limit.with_window_seconds(seconds);
        }
        if let Some(end) = end {
            limit = limit.with_resets_at(end);
        }
        limit
    };

    if let Some(percent) = config.credit_usage_percent {
        account.push_limit(with_period(UsageLimit::new("weekly", percent)));
    }
    for usage in config.product_usage.into_iter().flatten() {
        let (Some(product), Some(percent)) = (usage.product, usage.usage_percent) else {
            continue;
        };
        account.push_limit(with_period(
            UsageLimit::new(format!("weekly:{product}"), percent).with_scope(product),
        ));
    }
    if let Some(cents) = config.prepaid_balance.and_then(|b| b.val) {
        account = account.with_credits(
            CreditBalance::new()
                .with_balance(format_cents(cents))
                .with_currency("USD"),
        );
    }
    Ok(account)
}

/// Formats an amount in cents as a decimal string (`1234` → `"12.34"`).
fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: Value) -> String {
        format!(
            "eyJ.{}.sig",
            URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
        )
    }

    fn request_for(api_key: &str) -> Option<Request<Vec<u8>>> {
        let cfg = serde_json::json!({ "api_key": api_key, "model": "grok-4" });
        account_request(&cfg.to_string()).expect("request")
    }

    #[test]
    fn xai_api_key_is_unsupported() {
        assert!(request_for("xai-abc123").is_none());
    }

    #[test]
    fn xai_ignores_a_token_from_another_issuer() {
        let other = token(serde_json::json!({"iss": "https://auth.example.com", "sub": "u1"}));

        assert!(request_for(&other).is_none());
    }

    #[test]
    fn xai_oauth_request_carries_the_grok_cli_headers() {
        let oauth = token(serde_json::json!({"iss": "https://auth.x.ai", "sub": "user-7"}));

        let req = request_for(&oauth).expect("xai oauth token has an account");

        assert_eq!(req.uri().to_string(), ACCOUNT_URL);
        assert_eq!(req.uri().host(), Some(ACCOUNT_HOST));
        assert_eq!(
            req.headers().get(AUTHORIZATION).unwrap(),
            &format!("Bearer {oauth}")
        );
        assert_eq!(
            req.headers().get("X-XAI-Token-Auth").unwrap(),
            "xai-grok-cli"
        );
        assert_eq!(req.headers().get("x-userid").unwrap(), "user-7");
    }

    #[test]
    fn grok_credits_body_becomes_a_snapshot() {
        let body = r#"{"config":{
            "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY",
                "start":"2026-11-22T16:16:02.926+00:00","end":"2026-11-29T16:16:02.926+00:00"},
            "creditUsagePercent":22.0,
            "productUsage":[{"product":"Api","usagePercent":15.0},{"product":"GrokChat","usagePercent":7.0}],
            "isUnifiedBillingUser":true,
            "prepaidBalance":{"val":0}}}"#;
        let end = parse_rfc3339_utc("2026-11-29T16:16:02Z").unwrap();

        let account = parse_account(&Response::new(body.as_bytes().to_vec())).expect("parse");

        let period = |limit: UsageLimit| {
            limit
                .with_label("USAGE_PERIOD_TYPE_WEEKLY")
                .with_window_seconds(604_800)
                .with_resets_at(end)
        };
        assert_eq!(
            account.limits,
            [
                period(UsageLimit::new("weekly", 22.0)),
                period(UsageLimit::new("weekly:Api", 15.0).with_scope("Api")),
                period(UsageLimit::new("weekly:GrokChat", 7.0).with_scope("GrokChat")),
            ]
        );
        assert_eq!(
            account.credits,
            Some(
                CreditBalance::new()
                    .with_balance("0.00")
                    .with_currency("USD")
            )
        );
        assert_eq!(account.plan, None);
    }

    #[test]
    fn grok_body_without_a_period_still_parses() {
        let body = r#"{"config":{"creditUsagePercent":5}}"#;

        let account = parse_account(&Response::new(body.as_bytes().to_vec())).expect("parse");

        assert_eq!(account.limits, [UsageLimit::new("weekly", 5.0)]);
        assert_eq!(account.credits, None);
    }

    #[test]
    fn xai_declares_the_grok_cli_proxy_as_its_account_host() {
        use querymt::plugin::HTTPLLMProviderFactory;

        assert_eq!(
            HTTPLLMProviderFactory::account_hosts(&crate::XaiFactory),
            ["cli-chat-proxy.grok.com"]
        );
    }

    #[test]
    fn cents_format_as_decimal() {
        assert_eq!(format_cents(0), "0.00");
        assert_eq!(format_cents(1234), "12.34");
        assert_eq!(format_cents(-5), "-0.05");
    }
}
