//! Unit tests: usage.

use crate::config::Anthropic;
use crate::tests::support::{
    accumulate, collect_chunks, parse_response, stream_output, terminal_index, test_anthropic,
};
use crate::wire::usage::AnthropicUsage;
use querymt::chat::{ChatOutputStatus, FinishReason, StructuredStreamEvent};
use querymt::{CacheWriteTiers, Usage};
use serde_json::Value;

#[test]
fn test_usage_deserialization_with_cache() {
    // Real fixture from Anthropic API response with cache creation and read tokens
    let json = r#"{
        "input_tokens": 12,
        "cache_creation_input_tokens": 1495,
        "cache_read_input_tokens": 0,
        "cache_creation": {
            "ephemeral_5m_input_tokens": 1495,
            "ephemeral_1h_input_tokens": 0
        },
        "output_tokens": 1024,
        "service_tier": "standard"
    }"#;

    let usage: Usage = serde_json::from_str::<AnthropicUsage>(json).unwrap().into();

    // Verify standard token counts
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 1024);

    // Verify cache-related fields (using Anthropic aliases)
    assert_eq!(usage.cache_write, 1495); // from cache_creation_input_tokens
    assert_eq!(usage.cache_read, 0); // from cache_read_input_tokens

    // Verify default values for fields not in the JSON
    assert_eq!(usage.reasoning_tokens, 0);

    // Note: Extra fields like "cache_creation" and "service_tier" are
    // silently ignored during deserialization as they're not part of Usage struct
}

#[test]
fn test_streaming_message_start_usage() {
    // Full "Hello" response from the example in the Anthropic docs.
    // message_start carries input_tokens; message_delta carries output_tokens.
    let anthropic = test_anthropic("sk-ant-api03-test");

    let lines = [
        r#"data: {"type":"message_start","message":{"id":"msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY","type":"message","role":"assistant","content":[],"model":"claude-opus-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"data: {"type":"ping"}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
        r#"data: {"type":"message_stop"}"#,
    ];

    let chunks = collect_chunks(&anthropic, &lines);

    // The first chunk is structured metadata carrying the start usage.
    match &chunks[0] {
        querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
            response_id,
            status,
            usage: Some(u),
            ..
        }) => {
            assert_eq!(*status, Some(ChatOutputStatus::InProgress));
            assert_eq!(
                response_id.as_deref(),
                Some("msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY")
            );
            assert_eq!(u.input_tokens, 25);
            // The start event's output count is a provisional placeholder;
            // it is withheld so it cannot survive into the final usage.
            assert_eq!(u.output_tokens, 0);
        }
        other => panic!("expected metadata from message_start, got {other:?}"),
    }

    // No flat chunks are interleaved.
    assert!(
        chunks
            .iter()
            .all(|c| matches!(c, querymt::chat::StreamChunk::Structured(_)))
    );

    // Exactly one terminal, carrying the final overlaid usage.
    let terminal = terminal_index(&chunks);
    match &chunks[terminal] {
        querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
            status,
            usage: Some(u),
            finish_reason,
            detail,
        }) => {
            assert_eq!(*status, ChatOutputStatus::Completed);
            assert_eq!(*finish_reason, Some(FinishReason::Stop));
            assert!(detail.is_none());
            assert_eq!((u.input_tokens, u.output_tokens), (25, 15));
        }
        other => panic!("expected terminal with usage, got {other:?}"),
    }

    // The finished text equals the concatenated deltas.
    match accumulate(&chunks) {
        querymt::chat::ChatStreamFinish::Completed(output) => {
            assert_eq!(output.items.len(), 1);
            assert_eq!(output.text().as_deref(), Some("Hello!"));
        }
        other => panic!("expected completed outcome, got {other:?}"),
    }
}

#[test]
fn test_streaming_usage_merge_max_gives_correct_totals() {
    // The terminal carries the start usage overlaid with the final output
    // count, so the accumulated usage is already correct.
    let (output, _) = stream_output(&[
        r#"{"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","content":[],"model":"claude-opus-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
    ]);

    let merged = output.usage.expect("usage expected");
    assert_eq!(
        merged.input_tokens, 25,
        "input_tokens must come from message_start"
    );
    assert_eq!(
        merged.output_tokens, 15,
        "output_tokens must be the cumulative value from message_delta"
    );
    assert_eq!(merged.cache_read, 0);
    assert_eq!(merged.cache_write, 0);
}

#[test]
fn usage_maps_thinking_tokens_streaming_and_non_streaming() {
    let usage: AnthropicUsage = serde_json::from_str(
        r#"{"input_tokens":12,"cache_creation_input_tokens":1495,"cache_read_input_tokens":3,
            "output_tokens":1024,"service_tier":"standard"}"#,
    )
    .unwrap();
    let usage: Usage = usage.into();
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 1024);
    assert_eq!(usage.cache_write, 1495);
    assert_eq!(usage.cache_read, 3);
    assert_eq!(usage.reasoning_tokens, 0);

    let (output, _) = stream_output(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":32000,"output_tokens_details":{"thinking_tokens":31950}}}"#,
    ]);
    let usage = output.usage.unwrap();
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.output_tokens, 50);
    assert_eq!(usage.reasoning_tokens, 31_950);
    assert_eq!(output.finish_reason, Some(FinishReason::Length));
}

// Single line: `sse()` emits one `data:` line per entry.
const MIXED_TTL_CACHE: &str = r#""cache_creation_input_tokens":300,"cache_read_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":200,"ephemeral_1h_input_tokens":100}"#;

fn mixed_ttl_expected() -> Usage {
    Usage {
        input_tokens: 12,
        output_tokens: 40,
        reasoning_tokens: 0,
        cache_read: 30,
        cache_write: 300,
        cache_write_tiers: Some(CacheWriteTiers {
            five_minute: Some(200),
            one_hour: Some(100),
        }),
    }
}

#[test]
fn mixed_ttl_cache_writes_match_between_streaming_and_non_streaming() {
    let non_streaming = parse_response(serde_json::json!({
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": serde_json::from_str::<Value>(&format!(
            r#"{{"input_tokens":12,"output_tokens":40,{MIXED_TTL_CACHE}}}"#
        )).unwrap()
    }));
    assert_eq!(non_streaming.usage, Some(mixed_ttl_expected()));

    // Start carries input/cache (with a provisional output of 1); the
    // delta carries only the final output count.
    let (streamed, _) = stream_output(&[
        &format!(
            r#"{{"type":"message_start","message":{{"usage":{{"input_tokens":12,"output_tokens":1,{MIXED_TTL_CACHE}}}}}}}"#
        ),
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":40}}"#,
    ]);
    assert_eq!(streamed.usage, Some(mixed_ttl_expected()));
    // Total input-side tokens count the writes once: 12 + 30 + 300.
    let u = streamed.usage.unwrap();
    assert_eq!(u.input_tokens + u.cache_read + u.cache_write, 342);
}

#[test]
fn aggregate_writes_without_breakdown_keep_tiers_absent() {
    let usage: Usage = serde_json::from_str::<AnthropicUsage>(
        r#"{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":300}"#,
    )
    .unwrap()
    .into();
    assert_eq!(usage.cache_write, 300);
    assert_eq!(usage.cache_write_tiers, None, "no invented 5m writes");

    // An empty breakdown object is not a breakdown either.
    let usage: Usage = serde_json::from_str::<AnthropicUsage>(
        r#"{"cache_creation_input_tokens":300,"cache_creation":{}}"#,
    )
    .unwrap()
    .into();
    assert_eq!(usage.cache_write_tiers, None);

    // An explicit zero tier stays distinct from an absent one.
    let usage: Usage = serde_json::from_str::<AnthropicUsage>(
        r#"{"cache_creation_input_tokens":300,
            "cache_creation":{"ephemeral_5m_input_tokens":300,"ephemeral_1h_input_tokens":0}}"#,
    )
    .unwrap()
    .into();
    let tiers = usage.cache_write_tiers.unwrap();
    assert_eq!((tiers.five_minute, tiers.one_hour), (Some(300), Some(0)));
}

#[test]
fn inconsistent_breakdown_is_retained_not_corrected() {
    let usage: Usage = serde_json::from_str::<AnthropicUsage>(
        r#"{"cache_creation_input_tokens":300,
            "cache_creation":{"ephemeral_5m_input_tokens":200,"ephemeral_1h_input_tokens":200}}"#,
    )
    .unwrap()
    .into();
    // The aggregate stays authoritative; valuation reports the mismatch.
    assert_eq!(usage.cache_write, 300);
    let tiers = usage.cache_write_tiers.unwrap();
    assert_eq!((tiers.five_minute, tiers.one_hour), (Some(200), Some(200)));
}

#[test]
fn final_ordinary_output_drops_below_the_provisional_count() {
    // message_start reports a provisional output of 1; the final inclusive
    // 100 is all thinking, so ordinary output must end at 0, not 1.
    let (output, chunks) = stream_output(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":100,"output_tokens_details":{"thinking_tokens":100}}}"#,
    ]);
    let usage = output.usage.unwrap();
    assert_eq!(usage.output_tokens, 0);
    assert_eq!(usage.reasoning_tokens, 100);
    assert_eq!(usage.input_tokens, 7);
    // The provisional value is never emitted as ordinary output: neither
    // the start metadata nor the terminal carries a non-zero ordinary count.
    let mut seen = 0;
    for chunk in &chunks {
        if let querymt::chat::StreamChunk::Structured(
            StructuredStreamEvent::ResponseMetadata { usage: Some(u), .. }
            | StructuredStreamEvent::ResponseTerminal { usage: Some(u), .. },
        ) = chunk
        {
            assert_eq!(u.output_tokens, 0);
            seen += 1;
        }
    }
    assert_eq!(seen, 2, "start metadata and terminal both carry usage");
}

#[test]
fn no_thinking_detail_keeps_reported_output_and_zero_reasoning() {
    let (output, _) = stream_output(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
    ]);
    let usage = output.usage.unwrap();
    assert_eq!((usage.output_tokens, usage.reasoning_tokens), (15, 0));
}

#[test]
fn later_events_retain_omitted_fields_and_replace_reported_ones() {
    // An omitted input field keeps the start value; an explicit zero replaces it.
    let (output, _) = stream_output(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":9}}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
    ]);
    let usage = output.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.cache_read), (25, 9));

    // The terminal carries the overlaid value, and the accumulator takes the
    // terminal usage as authoritative, so the replacement is visible.
    let (output, _) = stream_output(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":9}}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":25,"cache_read_input_tokens":0,"output_tokens":5}}"#,
    ]);
    let last = output.usage.unwrap();
    assert_eq!((last.input_tokens, last.cache_read), (25, 0));
}

#[test]
fn repeated_delta_events_do_not_duplicate_tokens() {
    // A redelivered usage-only delta, then the terminal delta.
    let usage_only = r#"{"type":"message_delta","delta":{},"usage":{"output_tokens":40}}"#;
    let terminal = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":40}}"#;
    let (output, _) = stream_output(&[
        &format!(
            r#"{{"type":"message_start","message":{{"usage":{{"input_tokens":12,"output_tokens":1,{MIXED_TTL_CACHE}}}}}}}"#
        ),
        usage_only,
        usage_only,
        terminal,
    ]);
    assert_eq!(output.usage, Some(mixed_ttl_expected()));
}

#[test]
fn stop_reasons_map_identically_on_both_paths() {
    let cases = [
        ("end_turn", FinishReason::Stop),
        ("stop_sequence", FinishReason::Stop),
        ("tool_use", FinishReason::ToolCalls),
        ("max_tokens", FinishReason::Length),
        ("model_context_window_exceeded", FinishReason::Length),
        ("refusal", FinishReason::ContentFilter),
        ("pause_turn", FinishReason::Other),
        ("something_new", FinishReason::Unknown),
    ];
    for (raw, expected) in cases {
        assert_eq!(Anthropic::map_stop_reason(raw), expected, "{raw}");

        let output = parse_response(serde_json::json!({
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": raw,
            "stop_details": {"category": "cyber"}
        }));
        assert_eq!(output.finish_reason, Some(expected), "non-streaming {raw}");

        let delta = format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{raw}","stop_details":{{"category":"cyber"}}}},"usage":{{"output_tokens":1}}}}"#
        );
        let (out, _) = stream_output(&[delta.as_str()]);
        assert_eq!(out.finish_reason, Some(expected), "streaming {raw}");
    }
}
