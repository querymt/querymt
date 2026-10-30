//! Unit tests: stream_blocks.

use crate::response::text_message_item;
use crate::tests::support::{
    accumulate, collect_chunks, completions, delta, stream_output, terminal_index, test_anthropic,
};
use querymt::chat::{
    ChatOutputItem, ChatOutputStatus, FinishReason, LENGTH_CUTOFF_DETAIL, StructuredStreamEvent,
};

fn start(index: usize, id: &str) -> String {
    format!(
        r#"data: {{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{id}","name":"edit"}}}}"#
    )
}

fn stop(index: usize) -> String {
    format!(r#"data: {{"type":"content_block_stop","index":{index}}}"#)
}

fn message_delta(reason: &str) -> String {
    format!(r#"data: {{"type":"message_delta","delta":{{"stop_reason":"{reason}"}}}}"#)
}

fn run(lines: Vec<String>) -> Vec<querymt::chat::StreamChunk> {
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    collect_chunks(&test_anthropic("sk-ant-api03-test"), &refs)
}

#[test]
fn normal_empty_input_completes_as_empty_object_unmarked() {
    let chunks = run(vec![start(0, "t1"), stop(0), message_delta("tool_use")]);
    assert_eq!(completions(&chunks), [("t1".into(), "{}".into(), false)]);
}

#[test]
fn valid_object_input_is_unmarked() {
    let chunks = run(vec![
        start(0, "t1"),
        delta(0, r#"{"file_path":"a.rs"}"#),
        stop(0),
        message_delta("tool_use"),
    ]);
    assert_eq!(
        completions(&chunks),
        [("t1".into(), r#"{"file_path":"a.rs"}"#.into(), false)]
    );
}

#[test]
fn malformed_and_non_object_input_is_marked_with_raw_buffer_preserved() {
    let chunks = run(vec![
        start(0, "bad"),
        delta(0, r#"{"edits": ["#),
        stop(0),
        start(1, "arr"),
        delta(1, "[1,2]"),
        stop(1),
        message_delta("tool_use"),
    ]);
    assert_eq!(
        completions(&chunks),
        [
            ("bad".into(), r#"{"edits": ["#.into(), true),
            ("arr".into(), "[1,2]".into(), true),
        ]
    );
}

#[test]
fn max_tokens_marks_a_valid_object_block_that_was_still_open() {
    let chunks = run(vec![
        start(0, "t1"),
        delta(0, r#"{"file_path":"a.rs"}"#),
        message_delta("max_tokens"),
    ]);
    assert_eq!(
        completions(&chunks),
        [("t1".into(), r#"{"file_path":"a.rs"}"#.into(), true)]
    );
    let terminal = terminal_index(&chunks);
    let complete = chunks
        .iter()
        .position(|c| {
            matches!(
                c,
                querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ItemCompleted { .. })
            )
        })
        .unwrap();
    assert!(complete < terminal, "completion precedes the terminal");
    assert!(matches!(
        &chunks[terminal],
        querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
            status: ChatOutputStatus::Incomplete,
            finish_reason: Some(FinishReason::Length),
            detail: Some(detail),
            ..
        }) if detail == LENGTH_CUTOFF_DETAIL
    ));
}

#[test]
fn max_tokens_flushes_every_open_block_in_index_order_once() {
    let chunks = run(vec![
        start(1, "second"),
        start(0, "first"),
        delta(0, r#"{"file_path": "a"#),
        message_delta("max_tokens"),
        // Late stops for already-flushed blocks must not duplicate completions.
        stop(0),
        stop(1),
    ]);
    let terminal = terminal_index(&chunks);
    assert_eq!(
        completions(&chunks),
        [
            ("first".into(), r#"{"file_path": "a"#.into(), true),
            ("second".into(), String::new(), true),
        ]
    );
    assert_eq!(terminal, chunks.len() - 1, "nothing follows the terminal");
    match accumulate(&chunks) {
        querymt::chat::ChatStreamFinish::Incomplete { detail, .. } => {
            assert_eq!(detail.as_deref(), Some(LENGTH_CUTOFF_DETAIL));
        }
        other => panic!("expected incomplete outcome, got {other:?}"),
    }
}

#[test]
fn blocks_completed_before_max_tokens_are_not_invalidated() {
    let chunks = run(vec![
        start(0, "done"),
        delta(0, r#"{"file_path":"a.rs"}"#),
        stop(0),
        start(1, "open"),
        delta(1, r#"{"file_path":"#),
        message_delta("max_tokens"),
    ]);
    assert_eq!(
        completions(&chunks),
        [
            ("done".into(), r#"{"file_path":"a.rs"}"#.into(), false),
            ("open".into(), r#"{"file_path":"#.into(), true),
        ]
    );
}

#[test]
fn non_length_stop_reasons_do_not_flush_open_blocks() {
    let chunks = run(vec![start(0, "t1"), message_delta("tool_use")]);
    assert!(completions(&chunks).is_empty());
    // The genuinely unfinished block is a protocol violation, not a pass.
    assert!(matches!(
        accumulate(&chunks),
        querymt::chat::ChatStreamFinish::Failed { .. }
    ));
}

#[test]
fn parallel_tool_calls_at_high_indexes_keep_their_own_identity() {
    let chunks = run(vec![
        start(2, "first"),
        start(3, "second"),
        delta(2, r#"{"a":1}"#),
        delta(3, r#"{"b":2}"#),
        stop(2),
        stop(3),
        message_delta("tool_use"),
    ]);
    let output = match accumulate(&chunks) {
        querymt::chat::ChatStreamFinish::Completed(output) => output,
        other => panic!("expected completed outcome, got {other:?}"),
    };
    let calls: Vec<(&str, &str)> = output
        .items
        .iter()
        .filter_map(|item| match item {
            ChatOutputItem::FunctionCall(c) => Some((c.call_id.as_str(), c.arguments.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(calls, [("first", r#"{"a":1}"#), ("second", r#"{"b":2}"#)]);
    assert_eq!(output.finish_reason, Some(FinishReason::ToolCalls));
}

#[test]
fn server_tool_block_is_opaque_and_does_not_fail_the_response() {
    let chunks = run(vec![
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}}"#.to_string(),
        delta(1, r#"{"query":"rust"}"#),
        stop(1),
        start(2, "local"),
        delta(2, r#"{"path":"a"}"#),
        stop(2),
        message_delta("tool_use"),
    ]);
    let output = match accumulate(&chunks) {
        querymt::chat::ChatStreamFinish::Completed(output) => output,
        other => panic!("expected completed outcome, got {other:?}"),
    };
    assert_eq!(output.items.len(), 2);
    let ChatOutputItem::Opaque(opaque) = &output.items[0] else {
        panic!("expected opaque item: {:?}", output.items);
    };
    assert_eq!(opaque.original_type, "server_tool_use");
    assert_eq!(opaque.payload["id"], "srv_1");
    // Parsed input replaces the start's empty `input`, as in non-streaming.
    assert_eq!(
        opaque.payload["input"],
        serde_json::json!({"query": "rust"})
    );
    assert!(opaque.payload.get("input_json").is_none());
    let calls: Vec<&str> = output
        .items
        .iter()
        .filter_map(|item| match item {
            ChatOutputItem::FunctionCall(c) => Some(c.call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["local"]);
}

#[test]
fn opaque_block_with_non_object_input_keeps_raw_input_json() {
    let chunks = run(vec![
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_1","input":{}}}"#.to_string(),
        delta(0, r#"{"query":"#),
        stop(0),
        message_delta("end_turn"),
    ]);
    let output = match accumulate(&chunks) {
        querymt::chat::ChatStreamFinish::Completed(output) => output,
        other => panic!("expected completed outcome, got {other:?}"),
    };
    let ChatOutputItem::Opaque(opaque) = &output.items[0] else {
        panic!("expected opaque item: {:?}", output.items);
    };
    assert_eq!(opaque.payload["input_json"], r#"{"query":"#);
}

#[test]
fn repeated_message_start_emits_no_second_metadata() {
    let message_start =
        r#"data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5}}}"#
            .to_string();
    // Before the terminal and after it.
    let chunks = run(vec![
        message_start.clone(),
        message_start.clone(),
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
        stop(0),
        message_delta("end_turn"),
        message_start,
    ]);
    let metadata = chunks
        .iter()
        .filter(|c| {
            matches!(
                c,
                querymt::chat::StreamChunk::Structured(
                    StructuredStreamEvent::ResponseMetadata { .. }
                )
            )
        })
        .count();
    assert_eq!(metadata, 1);
    assert!(accumulate(&chunks).is_completed());
}

#[test]
fn consecutive_text_blocks_stay_separate_items() {
    let (output, _) = stream_output(&[
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"B"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
    ]);
    assert_eq!(
        output.items,
        [text_message_item("A".into()), text_message_item("B".into())]
    );
}

#[test]
fn orphan_input_delta_for_unstarted_block_emits_nothing() {
    let chunks = run(vec![delta(1, r#"{"x":1}"#), message_delta("end_turn")]);
    assert!(completions(&chunks).is_empty());
    assert!(accumulate(&chunks).is_completed());
}
