//! Unit tests: tool_names.

use crate::config::Anthropic;
use crate::tests::support::{
    collect_chunks, completions, started_call_names, test_anthropic, test_tool,
};
use http::Response;
use querymt::chat::http::HTTPChatProvider;
use querymt::chat::{ChatCall, ChatOutput, ChatOutputItem};
use querymt::error::LLMError;
use serde_json::Value;
use std::collections::HashMap;

fn tool_response(name: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(200)
        .body(
            serde_json::to_vec(&serde_json::json!({
                "content": [{"type": "tool_use", "id": "toolu_1", "name": name, "input": {}}],
                "stop_reason": "tool_use"
            }))
            .unwrap(),
        )
        .unwrap()
}

fn output_tool_name(output: ChatOutput) -> String {
    output
        .items
        .into_iter()
        .find_map(|item| match item {
            ChatOutputItem::FunctionCall(call) => Some(call.name),
            _ => None,
        })
        .expect("tool call")
}

#[test]
fn prepared_context_restores_per_call_names_without_configured_tools() {
    let anthropic = test_anthropic("sk-ant-oat01-test");
    for name in [
        "GetWeather",
        "getWeather",
        "mcp_custom",
        "StructuredOutput",
        "structuredOutput",
    ] {
        let tools = [test_tool(name)];
        let prepared = anthropic
            .prepare_chat_request(&ChatCall::new(&[]).tools(&tools), false)
            .unwrap();
        let wire = anthropic.prefix_tool_name(name);
        let request: Value = serde_json::from_slice(prepared.request.body()).unwrap();
        assert_eq!(request["tools"][0]["name"], wire);
        let output = anthropic
            .parse_chat(tool_response(&wire), prepared.context)
            .unwrap();
        assert_eq!(output_tool_name(output), name);
    }
}

#[test]
fn prepared_context_uses_overrides_and_survives_out_of_order_responses() {
    let mut anthropic = test_anthropic("sk-ant-oat01-test");
    anthropic.tools = Some(vec![test_tool("getWeather")]);
    let upper = [test_tool("GetWeather")];
    let first = anthropic
        .prepare_chat_request(&ChatCall::new(&[]).tools(&upper), false)
        .unwrap();
    let second = anthropic
        .prepare_chat_request(&ChatCall::new(&[]), false)
        .unwrap();
    assert_eq!(
        output_tool_name(
            anthropic
                .parse_chat(tool_response("mcp_GetWeather"), second.context,)
                .unwrap()
        ),
        "getWeather"
    );
    assert_eq!(
        output_tool_name(
            anthropic
                .parse_chat(tool_response("mcp_GetWeather"), first.context,)
                .unwrap()
        ),
        "GetWeather"
    );

    let empty = anthropic
        .prepare_chat_request(&ChatCall::new(&[]).tools(&[]), false)
        .unwrap();
    assert_eq!(empty.context, serde_json::json!({}));
    let request: Value = serde_json::from_slice(empty.request.body()).unwrap();
    assert_eq!(request["tools"], serde_json::json!([]));
}

#[test]
fn prepared_stream_parsers_own_independent_tool_mappings() {
    let anthropic = test_anthropic("sk-ant-oat01-test");
    let upper = [test_tool("GetWeather")];
    let lower = [test_tool("getWeather")];
    let a = anthropic
        .prepare_chat_request(&ChatCall::new(&[]).tools(&upper), true)
        .unwrap();
    let b = anthropic
        .prepare_chat_request(&ChatCall::new(&[]).tools(&lower), true)
        .unwrap();
    let request: Value = serde_json::from_slice(a.request.body()).unwrap();
    assert_eq!(request["stream"], true);
    // Exercise the same JSON boundary used by the plugin transport.
    let context = serde_json::from_slice(&serde_json::to_vec(&a.context).unwrap()).unwrap();
    let mut a = anthropic.chat_stream_parser(context).unwrap();
    let mut b = anthropic.chat_stream_parser(b.context).unwrap();
    let frame = br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"mcp_GetWeather"}}
"#;
    for (parser, expected) in [(&mut b, "getWeather"), (&mut a, "GetWeather")] {
        let chunks = parser.parse_chunk(frame).unwrap();
        assert_eq!(started_call_names(&chunks), [expected]);
    }
}

#[test]
fn oauth_rejects_colliding_tool_names_before_sending() {
    let anthropic = test_anthropic("sk-ant-oat01-test");
    let tools = [test_tool("getWeather"), test_tool("GetWeather")];
    for stream in [false, true] {
        let error = anthropic
            .prepare_chat_request(&ChatCall::new(&[]).tools(&tools), stream)
            .err()
            .expect("collision");
        assert!(
            matches!(error, LLMError::InvalidRequest(message) if message.contains("mcp_GetWeather"))
        );
    }
    assert!(
        anthropic
            .prepare_chat_request(&ChatCall::new(&[]).maybe_tools(Some(&tools)), false)
            .map(|p| p.request)
            .is_err()
    );
    let api_key = test_anthropic("sk-ant-api03-test");
    assert!(
        api_key
            .prepare_chat_request(&ChatCall::new(&[]).tools(&tools), false)
            .is_ok()
    );
}

#[test]
fn strip_tool_prefix_preserves_unprefixed_names() {
    // Names without TOOL_PREFIX were never transformed: keep them exact.
    assert_eq!(
        Anthropic::strip_tool_prefix("GetWeather", None),
        "GetWeather"
    );
    assert_eq!(Anthropic::strip_tool_prefix("read_file", None), "read_file");
    assert_eq!(
        Anthropic::strip_tool_prefix("StructuredOutput", None),
        "StructuredOutput"
    );
}

#[test]
fn strip_tool_prefix_restores_declared_capitalization() {
    let declared = HashMap::from([("mcp_GetWeather".to_string(), "GetWeather".to_string())]);

    // Prefixed names of declared tools are restored exactly.
    assert_eq!(
        Anthropic::strip_tool_prefix("mcp_GetWeather", Some(&declared)),
        "GetWeather"
    );
    // Undeclared tools keep the legacy first-character lowercasing.
    assert_eq!(
        Anthropic::strip_tool_prefix("mcp_get_weather", Some(&declared)),
        "get_weather"
    );
    // Prefixed StructuredOutput keeps its legacy handling.
    assert_eq!(
        Anthropic::strip_tool_prefix("mcp_StructuredOutput", Some(&declared)),
        "StructuredOutput"
    );
}

#[test]
fn tool_name_prefixing_round_trips_declared_names() {
    let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
    anthropic.tools = Some(vec![test_tool("GetWeather")]);

    let wire = anthropic.prefix_tool_name("GetWeather");
    assert_eq!(wire, "mcp_GetWeather");
    assert_eq!(
        Anthropic::strip_tool_prefix(&wire, Some(&anthropic.declared_tool_names(None).unwrap())),
        "GetWeather"
    );
}

#[test]
fn parse_chat_restores_declared_tool_names() {
    let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
    anthropic.tools = Some(vec![test_tool("GetWeather")]);

    let body = serde_json::json!({
        "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "mcp_GetWeather", "input": {}},
            {"type": "text", "text": "Checking the weather."}
        ],
        "stop_reason": "tool_use"
    });
    let resp = Response::builder()
        .status(200)
        .body(serde_json::to_vec(&body).expect("json should serialize"))
        .expect("response should build");

    let output = anthropic
        .parse_chat(resp, serde_json::Value::Null)
        .expect("response should parse");
    let names: Vec<&str> = output
        .items
        .iter()
        .filter_map(|item| match item {
            ChatOutputItem::FunctionCall(call) => Some(call.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["GetWeather"]);
}

#[test]
fn streaming_oauth_tool_names_restore_declared_capitalization() {
    let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
    anthropic.tools = Some(vec![test_tool("GetWeather")]);

    let lines = [
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"mcp_GetWeather"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
    ];
    let chunks = collect_chunks(&anthropic, &lines);

    assert_eq!(started_call_names(&chunks), ["GetWeather"]);
    let calls = completions(&chunks);
    assert_eq!(calls.len(), 1);
}
