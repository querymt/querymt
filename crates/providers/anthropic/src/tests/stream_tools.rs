//! Unit tests: stream_tools.

use crate::tests::support::{accumulate, collect_chunks, completions, test_anthropic};
use querymt::chat::{ChatOutputItem, FinishReason, StructuredStreamEvent};

/// The structured-event fixtures consumed by agent and remote-relay tests
/// are exactly what this parser emits for the checked-in SSE transcripts.
/// Regenerate with `UPDATE_ANTHROPIC_FIXTURES=1`.
#[test]
fn structured_event_fixtures_match_parser_output() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in ["structured_tool_turn", "structured_max_tokens"] {
        let sse = std::fs::read_to_string(dir.join(format!("{name}.sse"))).unwrap();
        let lines: Vec<&str> = sse.lines().collect();
        let chunks = collect_chunks(&test_anthropic("sk-ant-api03-test"), &lines);
        let actual = serde_json::to_string_pretty(&chunks).unwrap() + "\n";
        let path = dir.join(format!("{name}.json"));
        if std::env::var_os("UPDATE_ANTHROPIC_FIXTURES").is_some() {
            std::fs::write(&path, &actual).unwrap();
        }
        let expected = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            actual, expected,
            "{name}: parser output differs from fixture"
        );
    }
}

#[test]
fn test_streaming_tool_call_assembled() {
    // content_block_start -> ItemStarted(FunctionCall), input_json_delta ->
    // FunctionArgumentsDelta, content_block_stop -> ItemCompleted, and
    // message_delta -> ResponseTerminal.
    let anthropic = test_anthropic("sk-ant-api03-test");

    let lines = [
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"read_file"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"foo.txt\"}"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null}}"#,
    ];

    let chunks = collect_chunks(&anthropic, &lines);

    assert!(matches!(
        &chunks[0],
        querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata { .. })
    ));
    let deltas: Vec<&str> = chunks
        .iter()
        .filter_map(|c| match c {
            querymt::chat::StreamChunk::Structured(
                StructuredStreamEvent::FunctionArgumentsDelta {
                    output_index: 0,
                    delta,
                },
            ) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, [r#"{"path":"#, r#""foo.txt"}"#]);
    assert!(
        chunks
            .iter()
            .all(|c| matches!(c, querymt::chat::StreamChunk::Structured(_)))
    );

    match accumulate(&chunks) {
        querymt::chat::ChatStreamFinish::Completed(output) => {
            assert_eq!(output.items.len(), 1);
            let ChatOutputItem::FunctionCall(call) = &output.items[0] else {
                panic!("expected function call: {:?}", output.items);
            };
            assert_eq!(call.call_id, "toolu_01");
            assert_eq!(call.name, "read_file");
            assert_eq!(call.arguments, r#"{"path":"foo.txt"}"#);
            assert_eq!(output.finish_reason, Some(FinishReason::ToolCalls));
        }
        other => panic!("expected completed outcome, got {other:?}"),
    }
}

#[test]
fn test_streaming_multiple_tool_calls() {
    // Two tool calls at indices 0 and 1 (parallel tool calls)
    let anthropic = test_anthropic("sk-ant-api03-test");

    let lines = [
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"read_file"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.txt\"}"}}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_02","name":"write_file"}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"b.txt\"}"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null}}"#,
    ];

    let chunks = collect_chunks(&anthropic, &lines);
    assert_eq!(completions(&chunks).len(), 2, "expected 2 completed calls");

    let output = match accumulate(&chunks) {
        querymt::chat::ChatStreamFinish::Completed(output) => output,
        other => panic!("expected completed outcome, got {other:?}"),
    };
    let names: Vec<&str> = output
        .items
        .iter()
        .filter_map(|item| match item {
            ChatOutputItem::FunctionCall(call) => Some(call.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["read_file", "write_file"]);
}

#[test]
fn test_streaming_text_only_no_tool_complete() {
    // Pure text response: no function call completes, and no flat chunk is emitted.
    let anthropic = test_anthropic("sk-ant-api03-test");

    let lines = [
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello!"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#,
    ];

    let chunks = collect_chunks(&anthropic, &lines);

    assert!(completions(&chunks).is_empty());
    assert!(
        chunks
            .iter()
            .all(|c| matches!(c, querymt::chat::StreamChunk::Structured(_)))
    );
    match accumulate(&chunks) {
        querymt::chat::ChatStreamFinish::Completed(output) => {
            assert_eq!(output.text().as_deref(), Some("Hello!"));
            assert_eq!(output.finish_reason, Some(FinishReason::Stop));
        }
        other => panic!("expected completed outcome, got {other:?}"),
    }
}
