//! Unit tests: config.

use crate::config::AuthType;
use crate::tests::support::{anthropic_for, body, options_body, test_anthropic, user_msgs};
use querymt::chat::http::HTTPChatProvider;
use querymt::chat::{ChatCall, GenerationOptionSupport, GenerationOptions, ReasoningEffort};

#[test]
fn test_oauth_token_detection() {
    let anthropic = test_anthropic("sk-ant-oat01-abc123");
    assert_eq!(anthropic.determine_auth_type(), AuthType::OAuth);
}

#[test]
fn test_api_key_detection() {
    let anthropic = test_anthropic("sk-ant-api03-xyz789");
    assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
}

#[test]
fn test_explicit_auth_type_override() {
    // Even with an oat token, explicit auth_type should take precedence
    let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
    anthropic.auth_type = Some(AuthType::ApiKey);
    assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
}

#[test]
fn test_fallback_to_api_key_for_unknown_format() {
    let anthropic = test_anthropic("sk-ant-unknown-format");
    // Should default to API key and print warning
    assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
}

#[test]
fn test_version_number_flexibility() {
    // Test with different version numbers
    let anthropic_oat99 = test_anthropic("sk-ant-oat99-future");
    assert_eq!(anthropic_oat99.determine_auth_type(), AuthType::OAuth);

    let anthropic_api15 = test_anthropic("sk-ant-api15-future");
    assert_eq!(anthropic_api15.determine_auth_type(), AuthType::ApiKey);
}

#[test]
fn provider_reports_per_request_option_support() {
    let a = anthropic_for("claude-sonnet-4-5", None, 32_000);
    assert_eq!(a.capabilities().options, GenerationOptionSupport::ALL);
}

#[test]
fn max_tokens_override_applies_to_one_request_only() {
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    let options = GenerationOptions::default().with_max_tokens(128_000);

    assert_eq!(options_body(&a, &options, false)["max_tokens"], 128_000);
    assert_eq!(options_body(&a, &options, true)["max_tokens"], 128_000);
    // The next request, made without options, uses the configured value.
    assert_eq!(
        options_body(&a, &GenerationOptions::NONE, false)["max_tokens"],
        32_000
    );
    assert_eq!(a.max_tokens, 32_000);
}

#[test]
fn empty_options_produce_an_identical_request() {
    let a = anthropic_for("claude-opus-4-8", Some(ReasoningEffort::High), 32_000);
    let plain = a
        .prepare_chat_request(&ChatCall::new(&user_msgs()), false)
        .unwrap()
        .request;
    let with_none = a
        .prepare_chat_request(
            &ChatCall::new(&user_msgs()).options(GenerationOptions::NONE),
            false,
        )
        .unwrap()
        .request;
    assert_eq!(plain.body(), with_none.body());
}

#[test]
fn effort_override_changes_adaptive_output_config() {
    let a = anthropic_for("claude-sonnet-5-5", Some(ReasoningEffort::High), 32_000);
    let low = GenerationOptions::default().with_reasoning_effort(ReasoningEffort::Low);

    let b = options_body(&a, &low, false);
    assert_eq!(b["thinking"]["type"], "adaptive");
    assert_eq!(b["output_config"]["effort"], "low");
    assert_eq!(body(&a)["output_config"]["effort"], "high");
}

#[test]
fn effort_override_enables_thinking_when_none_configured() {
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    assert!(body(&a).get("thinking").is_none());

    let options = GenerationOptions::default().with_reasoning_effort(ReasoningEffort::Medium);
    let b = options_body(&a, &options, false);
    assert_eq!(b["output_config"]["effort"], "medium");
}

#[test]
fn effort_override_replaces_budget_for_budget_models() {
    let a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
    assert_eq!(body(&a)["thinking"]["budget_tokens"], 16_000);

    let low = GenerationOptions::default().with_reasoning_effort(ReasoningEffort::Low);
    assert_eq!(
        options_body(&a, &low, false)["thinking"]["budget_tokens"],
        1_024
    );
}

#[test]
fn effort_override_beats_configured_explicit_budget() {
    let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
    a.reasoning_budget_tokens = Some(12_000);
    assert_eq!(body(&a)["thinking"]["budget_tokens"], 12_000);

    let low = GenerationOptions::default().with_reasoning_effort(ReasoningEffort::Low);
    assert_eq!(
        options_body(&a, &low, false)["thinking"]["budget_tokens"],
        1_024
    );
    // Without an effort override the configured budget still applies.
    let tokens = GenerationOptions::default().with_max_tokens(64_000);
    assert_eq!(
        options_body(&a, &tokens, false)["thinking"]["budget_tokens"],
        12_000
    );
}

#[test]
fn max_tokens_override_refits_the_thinking_budget() {
    let a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 20_000);
    assert_eq!(body(&a)["thinking"]["budget_tokens"], 10_000);

    let larger = GenerationOptions::default().with_max_tokens(64_000);
    assert_eq!(
        options_body(&a, &larger, false)["thinking"]["budget_tokens"],
        16_000
    );

    // A cap too small for the minimum budget omits thinking entirely.
    let tiny = GenerationOptions::default().with_max_tokens(1_500);
    let b = options_body(&a, &tiny, false);
    assert!(b.get("thinking").is_none());
    assert_eq!(b["max_tokens"], 1_500);
}
