//! Account snapshot from the ChatGPT backend usage endpoint.
//!
//! The endpoint is the one the Codex CLI calls (`/backend-api/wham/usage`); it
//! is not a published API, so every field is read as optional.

use http::{Method, Request, Response, header::AUTHORIZATION};
use querymt::{
    error::LLMError,
    plugin::{CreditBalance, ProviderAccount, UsageLimit},
};
use serde::Deserialize;
use url::Url;

use crate::api::chatgpt_account_id;

/// The resolved-config fields the account request needs. Unknown fields are
/// ignored: the runtime passes the full chat configuration.
#[derive(Deserialize)]
struct AccountConfig {
    api_key: String,
    base_url: Option<Url>,
}

/// Builds `GET <base>/../wham/usage` for the ChatGPT account in the token.
///
/// # Errors
///
/// Returns [`LLMError::InvalidRequest`] when the token carries no ChatGPT
/// account claim, and a JSON or URL error for an unreadable configuration.
pub(crate) fn account_request(cfg: &str) -> Result<Option<Request<Vec<u8>>>, LLMError> {
    let cfg: AccountConfig = serde_json::from_str(cfg)?;
    let account_id = chatgpt_account_id(&cfg.api_key)?;
    let base_url = cfg.base_url.unwrap_or_else(crate::Codex::default_base_url);
    let url = base_url.join("../wham/usage")?;

    Ok(Some(
        Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .header(AUTHORIZATION, format!("Bearer {}", cfg.api_key))
            .header("ChatGPT-Account-ID", account_id)
            .header("originator", "codex_cli_rs")
            .header("accept", "application/json")
            .body(Vec::new())?,
    ))
}

#[derive(Deserialize)]
struct UsageBody {
    plan_type: Option<String>,
    rate_limit: Option<RateLimit>,
    additional_rate_limits: Option<Vec<AdditionalLimit>>,
    credits: Option<Credits>,
}

#[derive(Deserialize)]
struct RateLimit {
    limit_reached: Option<bool>,
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    used_percent: Option<f64>,
    limit_window_seconds: Option<u64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct AdditionalLimit {
    limit_name: Option<String>,
    metered_feature: Option<String>,
    rate_limit: Option<RateLimit>,
}

#[derive(Deserialize)]
struct Credits {
    unlimited: Option<bool>,
    balance: Option<String>,
}

/// Maps the usage body to a snapshot.
///
/// # Errors
///
/// Returns [`LLMError::ResponseFormatError`] when the body is not a JSON
/// object or a mapped field has the wrong type. The body is not echoed: it
/// carries the account's e-mail address.
pub(crate) fn parse_account(resp: &Response<Vec<u8>>) -> Result<ProviderAccount, LLMError> {
    let body: UsageBody =
        serde_json::from_slice(resp.body()).map_err(|e| LLMError::ResponseFormatError {
            message: format!("unreadable Codex usage response: {e}"),
            raw_response: String::new(),
        })?;

    let mut account = ProviderAccount::new();
    if let Some(plan) = body.plan_type {
        account = account.with_plan(plan);
    }
    if let Some(rate_limit) = body.rate_limit {
        push_windows(&mut account, rate_limit, "primary", "secondary", None);
    }
    for extra in body.additional_rate_limits.into_iter().flatten() {
        let (Some(feature), Some(rate_limit)) = (extra.metered_feature, extra.rate_limit) else {
            continue;
        };
        let secondary_id = format!("{feature}:secondary");
        push_windows(
            &mut account,
            rate_limit,
            &feature,
            &secondary_id,
            extra.limit_name.as_deref(),
        );
    }
    if let Some(credits) = body.credits {
        let mut balance = CreditBalance::new().with_unlimited(credits.unlimited.unwrap_or(false));
        if let Some(amount) = credits.balance {
            balance = balance.with_balance(amount);
        }
        account = account.with_credits(balance);
    }
    Ok(account)
}

/// Pushes the primary and secondary windows of one rate limit. The provider's
/// `limit_reached` flag applies to the primary window; the secondary window's
/// `reached` is derived from its percentage.
fn push_windows(
    account: &mut ProviderAccount,
    rate_limit: RateLimit,
    primary_id: &str,
    secondary_id: &str,
    scope: Option<&str>,
) {
    let windows = [
        (
            primary_id,
            rate_limit.primary_window,
            rate_limit.limit_reached,
        ),
        (secondary_id, rate_limit.secondary_window, None),
    ];
    for (id, window, reached) in windows {
        let Some(window) = window else { continue };
        let Some(used_percent) = window.used_percent else {
            continue;
        };
        let mut limit = UsageLimit::new(id, used_percent);
        if let Some(seconds) = window
            .limit_window_seconds
            .and_then(|s| u32::try_from(s).ok())
        {
            limit = limit.with_window_seconds(seconds);
        }
        if let Some(reset_at) = window.reset_at {
            limit = limit.with_resets_at(reset_at);
        }
        if let Some(reached) = reached {
            limit = limit.with_reached(reached);
        }
        if let Some(scope) = scope {
            limit = limit.with_scope(scope);
        }
        account.push_limit(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    fn oauth_token(claims: serde_json::Value) -> String {
        format!(
            "eyJ.{}.sig",
            URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
        )
    }

    fn chatgpt_token() -> String {
        oauth_token(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-1" }
        }))
    }

    fn response(body: &str) -> Response<Vec<u8>> {
        Response::new(body.as_bytes().to_vec())
    }

    #[test]
    fn codex_usage_body_becomes_a_snapshot() {
        let body = r#"{
            "email": "someone@example.com",
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 99,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 160666,
                    "reset_at": 1795798467
                },
                "secondary_window": null
            },
            "additional_rate_limits": [{
                "limit_name": "gpt-6-astra",
                "metered_feature": "codex_other",
                "rate_limit": {
                    "limit_reached": false,
                    "primary_window": {"used_percent": 70, "limit_window_seconds": 900, "reset_at": 789}
                }
            }],
            "credits": {"has_credits": true, "unlimited": false, "balance": "12701.63"}
        }"#;

        let account = parse_account(&response(body)).expect("parse");

        assert_eq!(account.plan.as_deref(), Some("pro"));
        assert_eq!(
            account.limits,
            [
                UsageLimit::new("primary", 99.0)
                    .with_window_seconds(604_800)
                    .with_resets_at(1_795_798_467)
                    .with_reached(false),
                UsageLimit::new("codex_other", 70.0)
                    .with_window_seconds(900)
                    .with_resets_at(789)
                    .with_reached(false)
                    .with_scope("gpt-6-astra"),
            ]
        );
        assert_eq!(
            account.credits,
            Some(CreditBalance::new().with_balance("12701.63"))
        );
    }

    #[test]
    fn codex_body_without_optional_sections_still_parses() {
        let body = r#"{"rate_limit": {"primary_window": {"used_percent": 40}}}"#;

        let account = parse_account(&response(body)).expect("parse");

        assert_eq!(account.plan, None);
        assert_eq!(account.credits, None);
        assert_eq!(account.limits, [UsageLimit::new("primary", 40.0)]);
    }

    #[test]
    fn codex_body_with_a_wrong_field_type_is_a_parse_error() {
        let error = parse_account(&response(r#"{"plan_type": 3}"#)).expect_err("wrong type");

        assert!(
            matches!(&error, LLMError::ResponseFormatError { raw_response, .. } if raw_response.is_empty()),
            "{error:?}"
        );
        assert!(parse_account(&response("[]")).is_err());
    }

    #[test]
    fn codex_token_without_an_account_claim_fails_before_any_request() {
        let cfg = serde_json::json!({ "api_key": oauth_token(serde_json::json!({"sub": "x"})) });

        let error = account_request(&cfg.to_string()).expect_err("no account claim");

        assert!(matches!(error, LLMError::InvalidRequest(_)), "{error:?}");
    }

    #[test]
    fn codex_custom_base_url_keeps_the_usage_path() {
        let cfg = serde_json::json!({
            "api_key": chatgpt_token(),
            "base_url": "https://proxy.example/backend-api/codex/",
            "model": "gpt-6-astra"
        });

        let req = account_request(&cfg.to_string())
            .expect("request")
            .expect("codex always has an account");

        assert_eq!(
            req.uri().to_string(),
            "https://proxy.example/backend-api/wham/usage"
        );
        assert_eq!(
            req.headers().get(AUTHORIZATION).unwrap(),
            &format!("Bearer {}", chatgpt_token())
        );
        assert_eq!(req.headers().get("ChatGPT-Account-ID").unwrap(), "acct-1");
    }

    #[test]
    fn codex_default_base_url_targets_chatgpt() {
        let cfg = serde_json::json!({ "api_key": chatgpt_token() });

        let req = account_request(&cfg.to_string()).unwrap().unwrap();

        assert_eq!(
            req.uri().to_string(),
            "https://chatgpt.com/backend-api/wham/usage"
        );
    }
}
