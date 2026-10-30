//! Unit tests: replay.

use crate::response::redacted_reasoning_item;
use crate::tests::support::{CAPTURED_WARNINGS, CaptureLogger, replay_assistant_content};
use querymt::chat::{
    ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem,
    ChatReasoningItem, ChatReasoningPart, ChatRole,
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
