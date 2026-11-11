//! Unit tests: cache breakpoint budget and lifetime ordering.

use crate::cache_breakpoints::normalize_cache_breakpoints;
use crate::response::redacted_reasoning_item;
use crate::tests::support::{anthropic_for, body_of};
use crate::wire::request::{AnthropicMessage, MessageContent};
use crate::wire::system::{AnthropicSystemPrompt, CacheControlEphemeral, TextBlockParam};
use querymt::chat::{CacheHint, ChatMessage, ChatOutput};
use serde_json::{Value, json};

fn hint() -> CacheHint {
    CacheHint::Ephemeral { ttl_seconds: None }
}

fn hinted_user(text: &str) -> ChatMessage {
    ChatMessage::user().text(text).cache(hint()).build()
}

fn plain_user(text: &str) -> ChatMessage {
    ChatMessage::user().text(text).build()
}

/// `n` system sections; the 1-based indices in `hinted` carry a default hint.
fn system_sections(n: usize, hinted: &[usize]) -> AnthropicSystemPrompt {
    let sections: Vec<Value> = (1..=n)
        .map(|i| {
            if hinted.contains(&i) {
                json!({"text": format!("section {i}"), "cache": {"ephemeral": {}}})
            } else {
                json!({"text": format!("section {i}")})
            }
        })
        .collect();
    serde_json::from_value(Value::Array(sections)).expect("system prompt")
}

fn count_cache_control(value: &Value) -> usize {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, v)| usize::from(key == "cache_control") + count_cache_control(v))
            .sum(),
        Value::Array(items) => items.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

/// Indices of system blocks that carry a marker.
fn marked_system(body: &Value) -> Vec<usize> {
    body["system"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| b.get("cache_control").is_some())
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default()
}

/// Indices of messages with a marker on any content block.
fn marked_messages(body: &Value) -> Vec<usize> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, m)| count_cache_control(&m["content"]) > 0)
        .map(|(i, _)| i)
        .collect()
}

fn request_body(system: AnthropicSystemPrompt, messages: &[ChatMessage]) -> Value {
    let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    a.system = Some(system);
    body_of(&a, messages)
}

#[test]
fn five_requested_breakpoints_are_reduced_to_four() {
    let messages = [plain_user("a"), hinted_user("b"), hinted_user("c")];
    let body = request_body(system_sections(3, &[1, 2, 3]), &messages);

    assert_eq!(count_cache_control(&body), 4);
    assert_eq!(marked_system(&body), vec![0, 2]);
    assert_eq!(marked_messages(&body), vec![1, 2]);
}

#[test]
fn requests_within_the_limit_are_unchanged() {
    let messages = [plain_user("a"), hinted_user("b"), hinted_user("c")];
    let body = request_body(system_sections(3, &[1, 3]), &messages);

    assert_eq!(count_cache_control(&body), 4);
    assert_eq!(marked_system(&body), vec![0, 2]);
    assert_eq!(marked_messages(&body), vec![1, 2]);
}

#[test]
fn hints_that_place_no_marker_do_not_use_the_budget() {
    let redacted = {
        let output = ChatOutput {
            items: vec![redacted_reasoning_item("D".into())],
            ..ChatOutput::default()
        };
        let mut message = ChatMessage::from_assistant_output(output);
        message.cache = Some(hint());
        message
    };
    let empty = ChatMessage::user().cache(hint()).build();
    let messages = [hinted_user("a"), hinted_user("b"), redacted, empty];
    let body = request_body(system_sections(2, &[1, 2]), &messages);

    assert_eq!(count_cache_control(&body), 4);
    assert_eq!(marked_system(&body), vec![0, 1]);
    assert_eq!(marked_messages(&body), vec![0, 1]);
}

#[test]
fn middle_system_markers_are_dropped_first() {
    let messages = [plain_user("a"), hinted_user("b"), hinted_user("c")];
    let body = request_body(system_sections(4, &[1, 2, 3, 4]), &messages);

    assert_eq!(count_cache_control(&body), 4);
    assert_eq!(marked_system(&body), vec![0, 3]);
    assert_eq!(marked_messages(&body), vec![1, 2]);

    // The warning count comes from the return value; the log line itself is
    // checked by review because log capture is process-global.
    let marker = || {
        Some(CacheControlEphemeral {
            control_type: "ephemeral".into(),
            ttl: None,
        })
    };
    let mut system = AnthropicSystemPrompt::Blocks(
        (1..=4)
            .map(|i| TextBlockParam {
                block_type: "text".into(),
                text: format!("section {i}"),
                cache_control: marker(),
                citations: None,
            })
            .collect(),
    );
    let mut wire_messages: Vec<AnthropicMessage> = (0..2)
        .map(|_| AnthropicMessage {
            role: "user",
            content: vec![MessageContent::Text {
                content_type: "text",
                text: "m".into(),
                cache_control: marker(),
            }],
        })
        .collect();
    assert_eq!(
        normalize_cache_breakpoints(Some(&mut system), &mut wire_messages),
        2
    );
}

#[test]
fn older_history_markers_are_dropped_before_recent_ones() {
    let messages: Vec<ChatMessage> = (1..=10)
        .map(|i| {
            if [1, 5, 9, 10].contains(&i) {
                hinted_user(&format!("m{i}"))
            } else {
                plain_user(&format!("m{i}"))
            }
        })
        .collect();
    let body = request_body(system_sections(1, &[1]), &messages);

    assert_eq!(count_cache_control(&body), 4);
    assert_eq!(marked_system(&body), vec![0]);
    // Zero-based message indices for messages 5, 9 and 10; message 1 is gone.
    assert_eq!(marked_messages(&body), vec![4, 8, 9]);
}

#[test]
fn oauth_identity_blocks_do_not_shift_system_priority() {
    let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    a.api_key = "sk-ant-oat01-abc123".into();
    a.system = Some(system_sections(2, &[1, 2]));
    // Five requested markers force a drop, so priority decides what remains.
    let messages = [hinted_user("a"), hinted_user("b"), hinted_user("c")];
    let body = body_of(&a, &messages);

    let blocks = body["system"].as_array().unwrap();
    // Billing header + identity block precede the two configured sections.
    assert_eq!(blocks.len(), 4);
    assert_eq!(marked_system(&body), vec![2, 3]);
    assert_eq!(blocks[2]["text"], "section 1");
    assert_eq!(blocks[3]["text"], "section 2");
    assert_eq!(count_cache_control(&body), 4);
    assert_eq!(marked_messages(&body), vec![1, 2]);
}

#[test]
fn default_system_marker_before_a_one_hour_marker_is_raised() {
    let system: AnthropicSystemPrompt = serde_json::from_value(json!([
        {"text": "one", "cache": {"ephemeral": {}}},
        {"text": "two", "cache": {"ephemeral": {"ttl_seconds": 1800}}}
    ]))
    .unwrap();
    let body = request_body(system, &[plain_user("hi")]);

    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
}

#[test]
fn longer_lifetime_before_shorter_is_preserved() {
    let system: AnthropicSystemPrompt = serde_json::from_value(json!([
        {"text": "one", "cache": {"ephemeral": {"ttl_seconds": 1800}}}
    ]))
    .unwrap();
    let body = request_body(system, &[hinted_user("hi")]);

    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    let message_marker = &body["messages"][0]["content"][0]["cache_control"];
    assert_eq!(message_marker["type"], "ephemeral");
    assert!(message_marker.get("ttl").is_none());
}

#[test]
fn lifetime_raise_ignores_markers_that_were_dropped() {
    // Five markers are requested, so the middle system marker (the only 1h
    // one) is dropped; nothing may be raised on its behalf.
    let system: AnthropicSystemPrompt = serde_json::from_value(json!([
        {"text": "one", "cache": {"ephemeral": {}}},
        {"text": "two", "cache": {"ephemeral": {"ttl_seconds": 1800}}},
        {"text": "three", "cache": {"ephemeral": {}}}
    ]))
    .unwrap();
    let messages = [hinted_user("a"), hinted_user("b")];
    let body = request_body(system, &messages);

    assert_eq!(marked_system(&body), vec![0, 2]);
    assert!(body["system"][0]["cache_control"].get("ttl").is_none());
    assert!(body["system"][2]["cache_control"].get("ttl").is_none());
}
