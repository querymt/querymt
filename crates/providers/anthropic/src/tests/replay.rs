//! Unit tests: replay.

use crate::response::redacted_reasoning_item;
use crate::tests::support::{
    CAPTURED_WARNINGS, CaptureLogger, anthropic_for, body_of, replay_assistant_content,
};
use querymt::chat::{
    CacheHint, ChatFunctionCallItem, ChatMessage, ChatMessageItem, ChatMessagePart, ChatOutput,
    ChatOutputItem, ChatReasoningItem, ChatReasoningPart, ChatRole, MediaKind, MediaPart,
    MediaSource,
};
use serde_json::Value;

fn reasoning_item(text: &str, signature: Option<&str>) -> ChatOutputItem {
    ChatOutputItem::Reasoning(ChatReasoningItem {
        id: None,
        summary: Vec::new(),
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ChatReasoningPart::text(text)]
        },
        encrypted_content: None,
        signature: signature.map(str::to_string),
        status: None,
        extensions: Default::default(),
    })
}

fn call_item(id: &str) -> ChatOutputItem {
    ChatOutputItem::FunctionCall(ChatFunctionCallItem {
        item_id: None,
        call_id: id.to_string(),
        name: "read_file".into(),
        arguments: "{}".into(),
        status: None,
        extensions: Default::default(),
    })
}

fn unparseable_call_item(id: &str, arguments: &str) -> ChatOutputItem {
    ChatOutputItem::FunctionCall(ChatFunctionCallItem {
        item_id: None,
        call_id: id.to_string(),
        name: "multiedit".into(),
        arguments: arguments.into(),
        status: None,
        extensions: Default::default(),
    })
}

#[test]
fn replay_of_unparseable_arguments_sends_empty_input_and_warns_with_call_id_only() {
    let _ = log::set_logger(&CaptureLogger);
    log::set_max_level(log::LevelFilter::Warn);
    let secret = r#"{"edits": ["SECRET-CONTENT"#;

    let raw = unparseable_call_item("toolu_bad", secret);
    let content = replay_assistant_content(vec![raw.clone()]);

    assert_eq!(content[0]["type"], "tool_use");
    assert_eq!(content[0]["id"], "toolu_bad");
    assert_eq!(content[0]["input"], serde_json::json!({}));
    // The replay placeholder never rewrites the stored call.
    let ChatOutputItem::FunctionCall(stored) = &raw else {
        unreachable!()
    };
    assert_eq!(stored.arguments, secret);

    let warnings = CAPTURED_WARNINGS.lock().unwrap();
    let ours: Vec<&String> = warnings
        .iter()
        .filter(|w| w.contains("toolu_bad"))
        .collect();
    assert_eq!(ours.len(), 1, "{warnings:?}");
    assert!(
        !ours[0].contains("SECRET-CONTENT"),
        "raw arguments must not be logged: {}",
        ours[0]
    );
}

#[test]
fn replay_of_non_object_arguments_sends_empty_input() {
    let content = replay_assistant_content(vec![unparseable_call_item("toolu_arr", "[1,2]")]);
    assert_eq!(content[0]["input"], serde_json::json!({}));
}

#[test]
fn replay_of_object_arguments_is_unchanged() {
    let content = replay_assistant_content(vec![unparseable_call_item(
        "toolu_ok",
        r#"{"file_path":"a.rs"}"#,
    )]);
    assert_eq!(
        content[0]["input"],
        serde_json::json!({"file_path": "a.rs"})
    );
}

#[test]
fn replay_sends_empty_signed_thinking_before_tool_use() {
    let content =
        replay_assistant_content(vec![reasoning_item("", Some("S")), call_item("toolu_1")]);
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "");
    assert_eq!(content[0]["signature"], "S");
    assert_eq!(content[1]["type"], "tool_use");
}

#[test]
fn replay_keeps_multiple_blocks_in_order_and_skips_unsigned() {
    let content = replay_assistant_content(vec![
        reasoning_item("one", Some("S1")),
        reasoning_item("unsigned", None),
        ChatOutputItem::Message(ChatMessageItem {
            id: None,
            role: ChatRole::Assistant,
            phase: None,
            status: None,
            parts: vec![ChatMessagePart::Text {
                text: "progress".into(),
                annotations: Vec::new(),
                extensions: Default::default(),
            }],
            extensions: Default::default(),
        }),
        reasoning_item("two", Some("S2")),
    ]);
    let thinking: Vec<&Value> = content.iter().filter(|c| c["type"] == "thinking").collect();
    assert_eq!(thinking.len(), 2);
    assert_eq!(thinking[0]["signature"], "S1");
    assert_eq!(thinking[0]["thinking"], "one");
    assert_eq!(thinking[1]["signature"], "S2");
    assert_eq!(thinking[1]["thinking"], "two");
}

/// Assistant message output item holding one text part.
fn text_message_output(text: &str) -> ChatOutputItem {
    crate::response::text_message_item(text.to_string())
}

#[test]
fn replay_does_not_duplicate_reasoning_as_text_and_keeps_order() {
    let content = replay_assistant_content(vec![
        reasoning_item("one", Some("S1")),
        reasoning_item("unsigned", None),
        text_message_output("progress"),
        call_item("toolu_1"),
    ]);
    let shape: Vec<(&str, Option<&str>)> = content
        .iter()
        .map(|c| {
            (
                c["type"].as_str().unwrap(),
                c["text"].as_str().or(c["thinking"].as_str()),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("thinking", Some("one")),
            ("text", Some("progress")),
            ("tool_use", None),
        ]
    );
}

#[test]
fn thinking_only_assistant_turn_never_carries_cache_control() {
    let output = ChatOutput {
        items: vec![reasoning_item("t", Some("S"))],
        ..ChatOutput::default()
    };
    let mut assistant = ChatMessage::from_assistant_output(output);
    assistant.cache = Some(CacheHint::Ephemeral { ttl_seconds: None });
    let messages = vec![ChatMessage::user().text("go").build(), assistant];
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    let b = body_of(&a, &messages);
    let content = b["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "thinking");
    assert!(content[0].get("cache_control").is_none());
}

#[test]
fn cache_control_skips_trailing_thinking_for_earlier_block() {
    let output = ChatOutput {
        items: vec![
            text_message_output("progress"),
            reasoning_item("t", Some("S")),
        ],
        ..ChatOutput::default()
    };
    let mut assistant = ChatMessage::from_assistant_output(output);
    assistant.cache = Some(CacheHint::Ephemeral { ttl_seconds: None });
    let messages = vec![ChatMessage::user().text("go").build(), assistant];
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    let b = body_of(&a, &messages);
    let content = b["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert!(content[0].get("cache_control").is_some());
    assert_eq!(content[1]["type"], "thinking");
    assert!(content[1].get("cache_control").is_none());
}

/// Wire content blocks of a cache-hinted user message holding one image.
fn user_image_content(media: MediaPart) -> Vec<Value> {
    let message = ChatMessage::user()
        .attachment(media)
        .cache(CacheHint::Ephemeral { ttl_seconds: None })
        .build();
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    let b = body_of(&a, &[message]);
    b["messages"][0]["content"].as_array().unwrap().clone()
}

#[test]
fn image_url_uses_url_source_and_keeps_cache_control() {
    let media = MediaPart::new(
        MediaKind::Image,
        None,
        MediaSource::Url {
            url: "https://example.com/a.png".into(),
        },
    )
    .unwrap();
    let content = user_image_content(media);
    assert_eq!(content[0]["type"], "image");
    assert_eq!(content[0]["source"]["type"], "url");
    assert_eq!(content[0]["source"]["url"], "https://example.com/a.png");
    assert!(content[0].get("cache_control").is_some());
}

#[test]
fn base64_data_url_image_becomes_base64_source() {
    let media = MediaPart::new(
        MediaKind::Image,
        None,
        MediaSource::DataUrl {
            url: "data:image/png;base64,iVBORw0KGgo=".into(),
        },
    )
    .unwrap();
    let content = user_image_content(media);
    assert_eq!(content[0]["type"], "image");
    assert_eq!(content[0]["source"]["type"], "base64");
    assert_eq!(content[0]["source"]["media_type"], "image/png");
    assert_eq!(content[0]["source"]["data"], "iVBORw0KGgo=");
    assert!(content[0].get("cache_control").is_some());
}

#[test]
fn completion_is_reported_as_unsupported_not_a_panic() {
    use querymt::completion::CompletionRequest;
    use querymt::completion::http::HTTPCompletionProvider;
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    let req = CompletionRequest::builder("hi").build();
    assert!(a.complete_request(&req).is_err());
    let resp = http::Response::builder().body(Vec::new()).unwrap();
    assert!(a.parse_complete(resp).is_err());
}

#[test]
fn replay_sends_redacted_thinking_before_tool_use() {
    let content = replay_assistant_content(vec![
        redacted_reasoning_item("D".to_string()),
        call_item("toolu_1"),
    ]);
    assert_eq!(content[0]["type"], "redacted_thinking");
    assert_eq!(content[0]["data"], "D");
    assert_eq!(content[1]["type"], "tool_use");
    // Unmarked encrypted content from another provider is not replayed.
    let mut other = reasoning_item("", None);
    if let ChatOutputItem::Reasoning(r) = &mut other {
        r.encrypted_content = Some("foreign".into());
    }
    let content = replay_assistant_content(vec![other, call_item("toolu_2")]);
    assert!(content.iter().all(|c| c["type"] != "redacted_thinking"));
}

#[test]
fn portable_projection_still_strips_signature_and_redacted_data() {
    let output = ChatOutput {
        items: vec![
            reasoning_item("t", Some("S")),
            redacted_reasoning_item("D".into()),
        ],
        ..ChatOutput::default()
    };
    let portable = output.clone().into_portable();
    let dump = serde_json::to_string(&portable).unwrap();
    assert!(!dump.contains("\"S\""), "{dump}");
    assert!(!dump.contains("\"D\""), "{dump}");
}
