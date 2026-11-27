//! Unit tests: account snapshot (Claude OAuth usage endpoint).

use crate::factory::AnthropicFactory;
use http::{Response, header::AUTHORIZATION};
use querymt::plugin::{HTTPLLMProviderFactory, UsageLimit, account::parse_rfc3339_utc};

fn request_for(api_key: &str) -> Option<http::Request<Vec<u8>>> {
    let cfg = serde_json::json!({ "api_key": api_key, "model": "claude-sonnet-4" });
    AnthropicFactory
        .account_request(&cfg.to_string())
        .expect("account request")
}

fn parse(body: &str) -> querymt::plugin::ProviderAccount {
    AnthropicFactory
        .parse_account(Response::new(body.as_bytes().to_vec()))
        .expect("parse")
}

#[test]
fn anthropic_api_key_is_unsupported() {
    assert!(request_for("sk-ant-api03-xyz789").is_none());
}

#[test]
fn anthropic_oauth_request_carries_the_oauth_headers() {
    let req = request_for("sk-ant-oat01-abc123").expect("oauth token has an account");

    assert_eq!(
        req.uri().to_string(),
        "https://api.anthropic.com/api/oauth/usage"
    );
    assert_eq!(
        req.headers().get(AUTHORIZATION).unwrap(),
        "Bearer sk-ant-oat01-abc123"
    );
    assert_eq!(
        req.headers().get("anthropic-beta").unwrap(),
        "oauth-2025-04-20"
    );
    assert_eq!(
        req.headers().get("anthropic-version").unwrap(),
        "2023-06-01"
    );
    assert!(req.headers().get("x-api-key").is_none());
}

#[test]
fn claude_usage_body_becomes_a_snapshot() {
    let body = r#"{
        "five_hour": {"utilization": 24.0, "resets_at": "2026-11-25T22:09:59.989045+00:00", "locked_reason": null},
        "seven_day": {"utilization": 40.0, "resets_at": "2026-11-29T03:59:59.989065+00:00", "locked_reason": null},
        "seven_day_opus": null,
        "limits": [
            {"kind": "session", "percent": 24, "resets_at": "2026-11-25T22:09:59.989045+00:00", "scope": null},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 10, "severity": "normal",
             "resets_at": "2026-11-29T03:59:59.989197+00:00",
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
        ],
        "spend": {"used": {"amount_minor": 0, "currency": "USD", "exponent": 2}, "balance": null}
    }"#;
    let five_hour_reset = parse_rfc3339_utc("2026-11-25T22:09:59Z").unwrap();
    let seven_day_reset = parse_rfc3339_utc("2026-11-29T03:59:59Z").unwrap();

    let account = parse(body);

    assert_eq!(
        account.limits,
        [
            UsageLimit::new("five_hour", 24.0)
                .with_window_seconds(18_000)
                .with_resets_at(five_hour_reset),
            UsageLimit::new("seven_day", 40.0)
                .with_window_seconds(604_800)
                .with_resets_at(seven_day_reset),
            UsageLimit::new("weekly_scoped", 10.0)
                .with_scope("Fable")
                .with_resets_at(seven_day_reset),
        ]
    );
    assert!(account.limits.iter().all(|limit| !limit.reached));
    assert_eq!(account.credits, None);
    assert_eq!(account.plan, None);
}

#[test]
fn unparseable_reset_time_leaves_the_limit_without_a_reset() {
    let account =
        parse(r#"{"five_hour": {"utilization": 24, "resets_at": "2026-11-25T22:09:59+02:00"}}"#);

    assert_eq!(
        account.limits,
        [UsageLimit::new("five_hour", 24.0).with_window_seconds(18_000)]
    );
}

#[test]
fn locked_window_is_reached() {
    let account = parse(r#"{"seven_day": {"utilization": 80, "locked_reason": "weekly_limit"}}"#);

    assert!(account.limits[0].reached);
}

#[test]
fn spend_balance_in_minor_units_becomes_credits() {
    let account = parse(
        r#"{"spend": {"balance": {"amount_minor": 1234, "currency": "USD", "exponent": 2}}}"#,
    );

    let credits = account.credits.expect("credits");
    assert_eq!(credits.balance.as_deref(), Some("12.34"));
    assert_eq!(credits.currency.as_deref(), Some("USD"));
}

#[test]
fn claude_body_with_a_wrong_field_type_is_a_parse_error() {
    let error = AnthropicFactory
        .parse_account(Response::new(br#"{"five_hour": "full"}"#.to_vec()))
        .expect_err("wrong type");

    assert!(
        matches!(&error, querymt::error::LLMError::ResponseFormatError { raw_response, .. } if raw_response.is_empty()),
        "{error:?}"
    );
}
