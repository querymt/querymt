//! Unit tests: system.

use crate::config::Anthropic;
use crate::wire::system::{
    AnthropicSystemPrompt, CacheControlEphemeral, CacheTTL, TextBlockParam, TextCitationParam,
};

#[test]
fn portable_instruction_hints_map_to_anthropic_cache_control() {
    let prompt: AnthropicSystemPrompt = serde_json::from_value(serde_json::json!([
        "legacy",
        {"text": "stable", "cache": {"ephemeral": {"ttl_seconds": 1800}}},
        {"type": "text", "text": "native", "cache_control": {"type": "ephemeral", "ttl": "5m"}, "cache": {"ephemeral": {"ttl_seconds": 1800}}},
        {"type": "text", "text": "native with portable hint", "cache": {"ephemeral": {"ttl_seconds": 300}}}
    ]))
    .unwrap();
    let AnthropicSystemPrompt::Blocks(blocks) = prompt else {
        panic!("blocks");
    };
    assert!(blocks[0].cache_control.is_none());
    assert_eq!(
        blocks[1].cache_control.as_ref().unwrap().ttl,
        Some(CacheTTL::OneHour)
    );
    assert_eq!(
        blocks[2].cache_control.as_ref().unwrap().ttl,
        Some(CacheTTL::FiveMinutes)
    );
    assert_eq!(blocks[1].text, "stable");
    assert_eq!(
        blocks[3].cache_control.as_ref().unwrap().ttl,
        Some(CacheTTL::FiveMinutes)
    );
}

#[test]
fn portable_defaults_match_legacy_wire_controls() {
    let portable: AnthropicSystemPrompt = serde_json::from_value(serde_json::json!([
        {"text": "stable", "cache": {"ephemeral": {}}}
    ]))
    .unwrap();
    let legacy: AnthropicSystemPrompt = serde_json::from_value(serde_json::json!([
        {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral"}}
    ]))
    .unwrap();
    assert_eq!(
        serde_json::to_value(portable).unwrap(),
        serde_json::to_value(legacy).unwrap()
    );
}

#[test]
fn test_system_prompt_deserialize_string() {
    let json = serde_json::json!({
        "api_key": "sk-ant-api03-test",
        "model": "claude-3-7-sonnet-20250219",
        "max_tokens": 100,
        "system": "You are a helpful assistant."
    });
    let anthropic: Anthropic = serde_json::from_value(json).unwrap();
    assert_eq!(
        anthropic.system,
        Some(AnthropicSystemPrompt::Text(
            "You are a helpful assistant.".to_string()
        ))
    );
}

#[test]
fn test_system_prompt_deserialize_blocks() {
    let json = serde_json::json!({
        "api_key": "sk-ant-api03-test",
        "model": "claude-3-7-sonnet-20250219",
        "max_tokens": 100,
        "system": [
            {
                "type": "text",
                "text": "You are a helpful assistant.",
                "cache_control": {
                    "type": "ephemeral"
                }
            }
        ]
    });
    let anthropic: Anthropic = serde_json::from_value(json).unwrap();
    match &anthropic.system {
        Some(AnthropicSystemPrompt::Blocks(blocks)) => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].text, "You are a helpful assistant.");
            assert_eq!(blocks[0].block_type, "text");
            assert!(blocks[0].cache_control.is_some());
            let cc = blocks[0].cache_control.as_ref().unwrap();
            assert_eq!(cc.control_type, "ephemeral");
            assert_eq!(cc.ttl, None);
        }
        other => panic!("Expected Blocks variant, got {:?}", other),
    }
}

#[test]
fn test_system_prompt_deserialize_blocks_with_ttl() {
    let json = serde_json::json!({
        "api_key": "sk-ant-api03-test",
        "model": "claude-3-7-sonnet-20250219",
        "max_tokens": 100,
        "system": [
            {
                "type": "text",
                "text": "You are a helpful assistant.",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "1h"
                }
            }
        ]
    });
    let anthropic: Anthropic = serde_json::from_value(json).unwrap();
    match &anthropic.system {
        Some(AnthropicSystemPrompt::Blocks(blocks)) => {
            let cc = blocks[0].cache_control.as_ref().unwrap();
            assert_eq!(cc.ttl, Some(CacheTTL::OneHour));
        }
        other => panic!("Expected Blocks variant, got {:?}", other),
    }
}

#[test]
fn test_system_prompt_serialize_string() {
    let prompt = AnthropicSystemPrompt::Text("Hello".to_string());
    let json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(json, serde_json::json!("Hello"));
}

#[test]
fn test_system_prompt_serialize_blocks() {
    let prompt = AnthropicSystemPrompt::Blocks(vec![TextBlockParam {
        block_type: "text".to_string(),
        text: "Hello".to_string(),
        cache_control: Some(CacheControlEphemeral {
            control_type: "ephemeral".to_string(),
            ttl: Some(CacheTTL::FiveMinutes),
        }),
        citations: None,
    }]);
    let json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(
        json,
        serde_json::json!([
            {
                "type": "text",
                "text": "Hello",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "5m"
                }
            }
        ])
    );
}

#[test]
fn test_system_prompt_deserialize_blocks_with_citations() {
    let json = serde_json::json!({
        "api_key": "sk-ant-api03-test",
        "model": "claude-3-7-sonnet-20250219",
        "max_tokens": 100,
        "system": [
            {
                "type": "text",
                "text": "Context from document.",
                "citations": [
                    {
                        "type": "char_location",
                        "cited_text": "some text",
                        "document_index": 0,
                        "document_title": "doc.pdf",
                        "start_char_index": 0,
                        "end_char_index": 9
                    },
                    {
                        "type": "page_location",
                        "cited_text": "page text",
                        "document_index": 1,
                        "document_title": "doc2.pdf",
                        "start_page_number": 1,
                        "end_page_number": 3
                    }
                ]
            }
        ]
    });
    let anthropic: Anthropic = serde_json::from_value(json).unwrap();
    match &anthropic.system {
        Some(AnthropicSystemPrompt::Blocks(blocks)) => {
            assert_eq!(blocks.len(), 1);
            let citations = blocks[0].citations.as_ref().unwrap();
            assert_eq!(citations.len(), 2);
            match &citations[0] {
                TextCitationParam::CharLocation(c) => {
                    assert_eq!(c.cited_text, "some text");
                    assert_eq!(c.start_char_index, 0);
                    assert_eq!(c.end_char_index, 9);
                }
                other => panic!("Expected CharLocation, got {:?}", other),
            }
            match &citations[1] {
                TextCitationParam::PageLocation(p) => {
                    assert_eq!(p.cited_text, "page text");
                    assert_eq!(p.start_page_number, 1);
                    assert_eq!(p.end_page_number, 3);
                }
                other => panic!("Expected PageLocation, got {:?}", other),
            }
        }
        other => panic!("Expected Blocks variant, got {:?}", other),
    }
}
