//! Unit tests: thinking_items.

use crate::tests::support::{parse_response, replay_assistant_content, stream_output};
use querymt::chat::{ChatOutputItem, FinishReason, StructuredStreamEvent};

#[test]
fn streaming_omitted_thinking_block_emits_signature() {
    let (output, _) = stream_output(&[
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read_file"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}"#,
    ]);
    let ChatOutputItem::Reasoning(r) = &output.items[0] else {
        panic!("expected reasoning first: {:?}", output.items);
    };
    assert_eq!(r.signature.as_deref(), Some("SIG"));
    assert!(r.visible_text().is_empty());
    assert!(matches!(output.items[1], ChatOutputItem::FunctionCall(_)));
}

#[test]
fn streaming_consecutive_thinking_blocks_are_separate_items() {
    let (output, _) = stream_output(&[
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"one"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"S1"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"progress"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"two"}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"signature_delta","signature":"S2"}}"#,
        r#"{"type":"content_block_stop","index":2}"#,
    ]);
    let sigs: Vec<_> = output
        .items
        .iter()
        .filter_map(|i| match i {
            ChatOutputItem::Reasoning(r) => r.signature.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(sigs, vec!["S1", "S2"]);
}

#[test]
fn streaming_redacted_thinking_block() {
    let (output, chunks) = stream_output(&[
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"OPAQUE"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read_file"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
    ]);
    // Started and completed immediately, exactly once, despite the later stop.
    let completed_reasoning = chunks
        .iter()
        .filter(|c| {
            matches!(
                c,
                querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: ChatOutputItem::Reasoning(_),
                })
            )
        })
        .count();
    assert_eq!(completed_reasoning, 1);
    let ChatOutputItem::Reasoning(r) = &output.items[0] else {
        panic!("expected reasoning");
    };
    assert_eq!(r.encrypted_content.as_deref(), Some("OPAQUE"));
    assert!(r.visible_text().is_empty());
    assert!(!format!("{chunks:?}").contains("OPAQUE"));
}

#[test]
fn non_streaming_items_in_content_order_with_signatures() {
    let output = parse_response(serde_json::json!({
        "content": [
            {"type": "thinking", "thinking": "one", "signature": "S1"},
            {"type": "redacted_thinking", "data": "D"},
            {"type": "thinking", "thinking": "", "signature": "S2"},
            {"type": "text", "text": "answer"},
            {"type": "tool_use", "id": "t1", "name": "read_file", "input": {"a": 1}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 600,
                  "output_tokens_details": {"thinking_tokens": 500}}
    }));
    assert_eq!(output.items.len(), 5);
    let ChatOutputItem::Reasoning(r0) = &output.items[0] else {
        panic!()
    };
    assert_eq!(r0.signature.as_deref(), Some("S1"));
    assert_eq!(r0.visible_text(), "one");
    let ChatOutputItem::Reasoning(r1) = &output.items[1] else {
        panic!()
    };
    assert_eq!(r1.encrypted_content.as_deref(), Some("D"));
    assert!(r1.signature.is_none());
    let ChatOutputItem::Reasoning(r2) = &output.items[2] else {
        panic!()
    };
    assert_eq!(r2.signature.as_deref(), Some("S2"));
    assert!(matches!(output.items[3], ChatOutputItem::Message(_)));
    assert!(matches!(output.items[4], ChatOutputItem::FunctionCall(_)));
    assert_eq!(output.text().as_deref(), Some("answer"));
    assert_eq!(output.finish_reason, Some(FinishReason::ToolCalls));
    let usage = output.usage.unwrap();
    assert_eq!(usage.output_tokens, 100);
    assert_eq!(usage.reasoning_tokens, 500);
}

#[test]
fn non_streaming_thinking_round_trips_through_replay() {
    let output = parse_response(serde_json::json!({
        "content": [
            {"type": "thinking", "thinking": "", "signature": "S"},
            {"type": "redacted_thinking", "data": "D"},
            {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}
        ],
        "stop_reason": "tool_use"
    }));
    let content = replay_assistant_content(output.items);
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["signature"], "S");
    assert_eq!(content[1]["type"], "redacted_thinking");
    assert_eq!(content[1]["data"], "D");
    assert_eq!(content[2]["type"], "tool_use");
}
