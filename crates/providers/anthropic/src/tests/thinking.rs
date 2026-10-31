//! Unit tests: thinking.

use crate::config::Anthropic;
use crate::model::ThinkingClass;
use crate::tests::support::{anthropic_for, body, test_tool, user_msgs};
use querymt::chat::http::HTTPChatProvider;
use querymt::chat::{ChatCall, ReasoningEffort, ToolChoice};
use querymt::error::LLMError;

#[test]
fn test_reasoning_budget_tokens_from_config_is_used_for_thinking_budget() {
    use querymt::chat::ChatMessage;

    let cfg = serde_json::json!({
        "api_key": "sk-ant-api03-test",
        "model": "claude-3-7-sonnet-20250219",
        "max_tokens": 2500,
        "reasoning_effort": "high",
        "reasoning_budget_tokens": 1024
    });

    let anthropic: Anthropic = serde_json::from_value(cfg)
        .expect("reasoning_budget_tokens should be accepted in Anthropic config");

    let messages = vec![
        ChatMessage::user()
            .text("How many r in strawberry?")
            .build(),
    ];
    let req = anthropic
        .prepare_chat_request(&ChatCall::new(&messages).maybe_tools(None), false)
        .map(|p| p.request)
        .expect("chat request should build");

    let body: serde_json::Value =
        serde_json::from_slice(req.body()).expect("request body should be valid JSON");
    assert_eq!(body["max_tokens"], serde_json::json!(2500));
    assert_eq!(body["thinking"]["type"], serde_json::json!("enabled"));
    assert_eq!(body["thinking"]["budget_tokens"], serde_json::json!(1024));
}

#[test]
fn adaptive_thinking_config_field_in_schema_and_factory() {
    let schema = serde_json::to_value(schemars::schema_for!(Anthropic)).unwrap();
    assert!(schema["properties"]["adaptive_thinking"].is_object());
    let cfg = serde_json::json!({
        "api_key": "sk-ant-api03-test",
        "model": "my-proxy-model",
        "max_tokens": 4096,
        "adaptive_thinking": true
    });
    let a: Anthropic = serde_json::from_value(cfg).unwrap();
    assert_eq!(a.adaptive_thinking, Some(true));
    let none: Anthropic = serde_json::from_value(serde_json::json!({
        "api_key": "k", "model": "m", "max_tokens": 1
    }))
    .unwrap();
    assert_eq!(none.adaptive_thinking, None);
}

#[test]
fn adaptive_override_applies_in_both_directions() {
    let mut a = anthropic_for("my-proxy-model", None, 32_000);
    assert_eq!(a.thinking_class(), ThinkingClass::Budget);
    a.adaptive_thinking = Some(true);
    assert_eq!(a.thinking_class(), ThinkingClass::OptInAdaptive);
    assert_eq!(body(&a)["thinking"]["type"], "adaptive");

    let mut b = anthropic_for("claude-opus-4-8", None, 32_000);
    assert_eq!(b.thinking_class(), ThinkingClass::OptInAdaptive);
    b.adaptive_thinking = Some(false);
    assert_eq!(b.thinking_class(), ThinkingClass::Budget);
    assert!(body(&b).get("thinking").is_none());
}

#[test]
fn predicates_ignore_the_override() {
    // Override forces budget, but the model still rejects sampling params.
    let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    a.adaptive_thinking = Some(false);
    a.temperature = Some(0.2);
    a.top_p = Some(0.99);
    let b = body(&a);
    assert!(b.get("temperature").is_none());
    assert!(b.get("top_p").is_none());

    // Override forces adaptive on an old model: forced tool choice allowed.
    let mut c = anthropic_for("claude-sonnet-4-5", None, 32_000);
    c.adaptive_thinking = Some(true);
    c.tools = Some(vec![test_tool("x")]);
    c.tool_choice = Some(ToolChoice::Tool("x".into()));
    assert!(
        c.prepare_chat_request(&ChatCall::new(&user_msgs()).maybe_tools(None), false)
            .map(|p| p.request)
            .is_ok()
    );
}

#[test]
fn auto_effort_by_class() {
    let b = body(&anthropic_for("claude-sonnet-5-5", None, 32_000));
    assert!(b.get("thinking").is_none());
    assert!(b.get("output_config").is_none());

    let b = body(&anthropic_for("claude-opus-4-8", None, 32_000));
    assert_eq!(b["thinking"]["type"], "adaptive");
    assert!(b["thinking"].get("budget_tokens").is_none());
    assert!(b.get("output_config").is_none());

    let b = body(&anthropic_for("claude-sonnet-4-5", None, 32_000));
    assert!(b.get("thinking").is_none());
    assert!(b.get("output_config").is_none());
}

#[test]
fn explicit_effort_by_class() {
    for model in ["claude-opus-5", "claude-opus-4-6", "claude-sonnet-5-5"] {
        for (effort, name) in [
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::Max, "max"),
        ] {
            let b = body(&anthropic_for(model, Some(effort), 32_000));
            assert_eq!(b["thinking"]["type"], "adaptive", "{model}");
            assert!(b["thinking"].get("budget_tokens").is_none());
            assert_eq!(b["output_config"]["effort"], name, "{model}");
        }
    }
    let b = body(&anthropic_for(
        "claude-sonnet-4-5",
        Some(ReasoningEffort::Medium),
        32_000,
    ));
    assert_eq!(b["thinking"]["type"], "enabled");
    assert_eq!(b["thinking"]["budget_tokens"], 8_000);
    assert!(b.get("output_config").is_none());
}

#[test]
fn budgets_are_fitted_to_max_tokens() {
    let b = body(&anthropic_for(
        "claude-sonnet-4-5",
        Some(ReasoningEffort::Max),
        32_000,
    ));
    assert_eq!(b["thinking"]["budget_tokens"], 16_000);

    let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
    a.reasoning_budget_tokens = Some(20_000);
    assert_eq!(body(&a)["thinking"]["budget_tokens"], 16_000);

    let b = body(&anthropic_for(
        "claude-sonnet-4-5",
        Some(ReasoningEffort::Low),
        1_500,
    ));
    assert!(b.get("thinking").is_none());

    // Minimum budget applies when the configured budget is tiny.
    let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::Low), 4_000);
    a.reasoning_budget_tokens = Some(100);
    assert_eq!(body(&a)["thinking"]["budget_tokens"], 1_024);
}

#[test]
fn sampling_filter_cases() {
    // Newer model: nothing is sent.
    let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    a.temperature = Some(0.2);
    a.top_p = Some(0.9);
    a.top_k = Some(40);
    let b = body(&a);
    assert!(b.get("temperature").is_none());
    assert!(b.get("top_p").is_none());
    assert!(b.get("top_k").is_none());

    // Opt-in adaptive in auto: thinking is sent, temperature is dropped.
    let mut a = anthropic_for("claude-sonnet-4-6", None, 32_000);
    a.temperature = Some(0.2);
    let b = body(&a);
    assert_eq!(b["thinking"]["type"], "adaptive");
    assert!(b.get("temperature").is_none());

    // Budget model without thinking keeps configured values.
    let mut a = anthropic_for("claude-sonnet-4-5", None, 32_000);
    a.temperature = Some(0.2);
    a.top_k = Some(40);
    let b = body(&a);
    assert!((b["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    assert_eq!(b["top_k"], 40);

    // Budget model with thinking: top_p only within 0.95..=1.
    let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
    a.temperature = Some(0.2);
    a.top_k = Some(40);
    a.top_p = Some(0.9);
    let b = body(&a);
    assert_eq!(b["thinking"]["type"], "enabled");
    assert!(b.get("top_p").is_none());
    assert!(b.get("temperature").is_none());
    assert!(b.get("top_k").is_none());
    a.top_p = Some(0.97);
    let b = body(&a);
    assert!((b["top_p"].as_f64().unwrap() - 0.97).abs() < 1e-6);

    // Opus 4.7 rejects sampling params too, even without opt-in thinking.
    let mut a = anthropic_for("claude-opus-4-7", None, 32_000);
    a.temperature = Some(0.5);
    assert!(body(&a).get("temperature").is_none());
}

#[test]
fn forced_tool_choice_rules() {
    let tools = vec![test_tool("x")];

    // Budget model: thinking dropped, tool_choice kept.
    let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
    a.tools = Some(tools.clone());
    a.tool_choice = Some(ToolChoice::Any);
    let b = body(&a);
    assert_eq!(b["tool_choice"]["type"], "any");
    assert!(b.get("thinking").is_none());
    assert!(b.get("output_config").is_none());

    // Model that rejects forced tool use: error before sending.
    let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    a.tools = Some(tools.clone());
    a.tool_choice = Some(ToolChoice::Tool("x".into()));
    let err = a
        .prepare_chat_request(&ChatCall::new(&user_msgs()).maybe_tools(None), false)
        .map(|p| p.request)
        .unwrap_err();
    assert!(matches!(err, LLMError::InvalidRequest(ref m) if m.contains("claude-sonnet-5-5")));

    // Other adaptive model: sent unchanged.
    let mut a = anthropic_for("claude-opus-4-8", None, 32_000);
    a.tools = Some(tools.clone());
    a.tool_choice = Some(ToolChoice::Any);
    let b = body(&a);
    assert_eq!(b["tool_choice"]["type"], "any");
    assert_eq!(b["thinking"]["type"], "adaptive");

    // `auto` is never treated as forced.
    let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    a.tools = Some(tools);
    a.tool_choice = Some(ToolChoice::Auto);
    assert!(
        a.prepare_chat_request(&ChatCall::new(&user_msgs()).maybe_tools(None), false)
            .map(|p| p.request)
            .is_ok()
    );
}
