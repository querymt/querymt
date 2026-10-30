//! Unit tests: strict_tools.

use crate::config::Anthropic;
use crate::create_http_factory;
use crate::tests::support::{CAPTURED_WARNINGS, CaptureLogger, anthropic_for, user_msgs};
use crate::tools::AnthropicTool;
use querymt::chat::Tool;
use serde_json::Value;

fn simple_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "occurrence": {"type": "integer", "minimum": 0, "description": "0-based."}
        },
        "required": ["path"]
    })
}

fn schema_tool(name: &str, schema: Value, strict: Option<bool>) -> Tool {
    Tool {
        tool_type: "function".to_string(),
        function: querymt::chat::FunctionTool {
            name: name.to_string(),
            description: "d".to_string(),
            parameters: schema,
            strict,
        },
    }
}

/// Provider configured with a name-based strict list, as an embedding
/// application would supply it.
fn anthropic_strict(model: &str) -> Anthropic {
    let mut a = anthropic_for(model, None, 4096);
    a.strict_tools = Some(vec![
        "edit".into(),
        "multiedit".into(),
        "replace_symbol".into(),
    ]);
    a
}

fn request_tools(a: &Anthropic, tools: &[Tool]) -> Vec<Value> {
    let req = a
        .build_chat_request(&user_msgs(), Some(tools))
        .expect("request builds");
    let body: Value = serde_json::from_slice(req.body()).unwrap();
    body["tools"].as_array().cloned().unwrap_or_default()
}

fn tool_named<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("tool {name} missing in {tools:?}"))
}

fn strict_of(tool: &Value) -> Option<bool> {
    tool.get("strict").and_then(Value::as_bool)
}

fn assert_all_original(tools: &[Value], originals: &[Tool]) {
    for (sent, original) in tools.iter().zip(originals) {
        assert!(sent.get("strict").is_none(), "{sent}");
        assert_eq!(sent["input_schema"], original.function.parameters);
    }
}

#[test]
fn anthropic_tool_serializes_strict_only_when_set() {
    let schema = simple_schema();
    let plain = AnthropicTool {
        name: "a".into(),
        description: "d",
        schema: std::borrow::Cow::Borrowed(&schema),
        strict: None,
    };
    let strict = AnthropicTool {
        name: "b".into(),
        description: "d",
        schema: std::borrow::Cow::Owned(serde_json::json!({"type": "object"})),
        strict: Some(true),
    };
    let plain = serde_json::to_value(&plain).unwrap();
    let strict = serde_json::to_value(&strict).unwrap();
    assert!(plain.get("strict").is_none());
    assert_eq!(plain["input_schema"], schema);
    assert_eq!(strict["strict"], true);
    assert_eq!(
        strict["input_schema"],
        serde_json::json!({"type": "object"})
    );
}

#[test]
fn configured_list_is_strict_on_supported_model_with_converted_schema() {
    let a = anthropic_strict("claude-sonnet-5-5");
    let tools = vec![
        schema_tool("edit", simple_schema(), None),
        schema_tool("multiedit", simple_schema(), None),
        schema_tool("replace_symbol", simple_schema(), None),
        schema_tool("glob", simple_schema(), None),
    ];
    let before = tools.clone();
    let sent = request_tools(&a, &tools);

    for name in ["edit", "multiedit", "replace_symbol"] {
        let tool = tool_named(&sent, name);
        assert_eq!(strict_of(tool), Some(true), "{name}");
        assert_eq!(tool["input_schema"]["additionalProperties"], false);
        assert!(
            tool["input_schema"]["properties"]["occurrence"]
                .get("minimum")
                .is_none()
        );
    }
    let glob = tool_named(&sent, "glob");
    assert!(glob.get("strict").is_none());
    assert_eq!(glob["input_schema"], simple_schema());
    // Source definitions are untouched by conversion.
    for (tool, original) in tools.iter().zip(&before) {
        assert_eq!(tool.function.parameters, original.function.parameters);
    }
}

#[test]
fn definition_marked_strict_is_sent_strict_even_when_unlisted() {
    let a = anthropic_for("claude-opus-4-8", None, 4096);
    let sent = request_tools(
        &a,
        &[schema_tool("mcp_lookup", simple_schema(), Some(true))],
    );
    assert_eq!(strict_of(&sent[0]), Some(true));
}

#[test]
fn unset_strict_tools_selects_nothing_by_name() {
    let a = anthropic_for("claude-sonnet-5-5", None, 4096);
    assert_eq!(a.strict_tools, None);
    let tools = vec![
        schema_tool("edit", simple_schema(), None),
        schema_tool("explicit", simple_schema(), Some(true)),
    ];
    let sent = request_tools(&a, &tools);
    assert!(tool_named(&sent, "edit").get("strict").is_none());
    assert_eq!(strict_of(tool_named(&sent, "explicit")), Some(true));
}

#[test]
fn empty_strict_tools_disables_name_selection_but_not_explicit_strictness() {
    let mut a = anthropic_for("claude-sonnet-5-5", None, 4096);
    a.strict_tools = Some(vec![]);
    let tools = vec![
        schema_tool("edit", simple_schema(), None),
        schema_tool("explicit", simple_schema(), Some(true)),
    ];
    let sent = request_tools(&a, &tools);
    assert!(tool_named(&sent, "edit").get("strict").is_none());
    assert_eq!(strict_of(tool_named(&sent, "explicit")), Some(true));
}

#[test]
fn configured_list_selects_only_listed_tools() {
    let mut a = anthropic_for("claude-sonnet-5-5", None, 4096);
    a.strict_tools = Some(vec!["glob".into()]);
    let sent = request_tools(
        &a,
        &[
            schema_tool("edit", simple_schema(), None),
            schema_tool("glob", simple_schema(), None),
        ],
    );
    assert!(tool_named(&sent, "edit").get("strict").is_none());
    assert_eq!(strict_of(tool_named(&sent, "glob")), Some(true));
}

#[test]
fn unsupported_and_unrecognised_models_send_original_schemas_without_strict() {
    let tools = vec![
        schema_tool("edit", simple_schema(), None),
        schema_tool("explicit", simple_schema(), Some(true)),
    ];
    for model in [
        "claude-sonnet-4-0",
        "claude-3-7-sonnet-20250219",
        "my-proxy-model",
    ] {
        let a = anthropic_for(model, None, 4096);
        assert_all_original(&request_tools(&a, &tools), &tools);
    }
}

#[test]
fn unsupported_schema_falls_back_per_tool_and_keeps_compatible_tools_strict() {
    let _ = log::set_logger(&CaptureLogger);
    log::set_max_level(log::LevelFilter::Warn);
    let a = anthropic_strict("claude-sonnet-5-5");
    // Relaxes `count`'s minimum before hitting the dictionary.
    let partial = serde_json::json!({
        "type": "object",
        "properties": {
            "count": {"type": "integer", "minimum": 0},
            "meta": {"type": "object", "additionalProperties": {"type": "string"}}
        }
    });
    let external = serde_json::json!({
        "type": "object",
        "properties": {"a": {"$ref": "https://example.com/x.json"}}
    });
    let tools = vec![
        schema_tool("dict_tool_zz", partial.clone(), Some(true)),
        schema_tool("ext_tool_zz", external.clone(), Some(true)),
        schema_tool("edit", simple_schema(), None),
    ];
    let sent = request_tools(&a, &tools);

    let dict = tool_named(&sent, "dict_tool_zz");
    assert!(dict.get("strict").is_none());
    assert_eq!(dict["input_schema"], partial, "no partial conversion leaks");
    assert_eq!(tool_named(&sent, "ext_tool_zz")["input_schema"], external);
    assert_eq!(strict_of(tool_named(&sent, "edit")), Some(true));

    let warnings = CAPTURED_WARNINGS.lock().unwrap();
    assert!(
        warnings.iter().any(
            |w| w.contains("dict_tool_zz") && w.contains("arbitrary keys")
                || w.contains("dict_tool_zz") && w.contains("extensible")
        ),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("ext_tool_zz") && w.contains("external reference")),
        "{warnings:?}"
    );
}

#[test]
fn unsupported_tools_do_not_count_against_the_strict_budget() {
    let a = anthropic_strict("claude-sonnet-5-5");
    // 25 optional properties would exceed the budget, but the tool is
    // unconvertible so it is not counted.
    let mut props = serde_json::Map::new();
    for i in 0..25 {
        props.insert(format!("o{i}"), serde_json::json!({"type": "string"}));
    }
    props.insert(
        "m".into(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    );
    let big = serde_json::json!({"type": "object", "properties": props});
    let sent = request_tools(
        &a,
        &[
            schema_tool("big", big, Some(true)),
            schema_tool("edit", simple_schema(), None),
        ],
    );
    assert_eq!(strict_of(tool_named(&sent, "edit")), Some(true));
    assert!(tool_named(&sent, "big").get("strict").is_none());
}

#[test]
fn exceeding_a_budget_sends_every_tool_non_strict_with_original_schemas() {
    let _ = log::set_logger(&CaptureLogger);
    log::set_max_level(log::LevelFilter::Warn);
    let a = anthropic_strict("claude-sonnet-5-5");

    let optional = |n: usize| {
        let mut props = serde_json::Map::new();
        for i in 0..n {
            props.insert(format!("o{i}"), serde_json::json!({"type": "string"}));
        }
        serde_json::json!({"type": "object", "properties": props})
    };

    // 25 optional properties across two strict tools.
    let tools = vec![
        schema_tool("edit", optional(12), None),
        schema_tool("multiedit", optional(13), None),
        schema_tool("glob", simple_schema(), None),
    ];
    assert_all_original(&request_tools(&a, &tools), &tools);
    assert!(
        CAPTURED_WARNINGS
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.contains("optional-property limit"))
    );

    // Exactly 24 is within the limit.
    let tools = vec![
        schema_tool("edit", optional(12), None),
        schema_tool("multiedit", optional(12), None),
    ];
    let sent = request_tools(&a, &tools);
    assert!(sent.iter().all(|t| strict_of(t) == Some(true)));

    // 21 strict tools.
    let many: Vec<Tool> = (0..21)
        .map(|i| schema_tool(&format!("t{i}"), simple_schema(), Some(true)))
        .collect();
    assert_all_original(&request_tools(&a, &many), &many);
    let twenty: Vec<Tool> = (0..20)
        .map(|i| schema_tool(&format!("t{i}"), simple_schema(), Some(true)))
        .collect();
    assert!(
        request_tools(&a, &twenty)
            .iter()
            .all(|t| strict_of(t) == Some(true))
    );

    // 17 union properties.
    let union = |n: usize| {
        let mut props = serde_json::Map::new();
        for i in 0..n {
            props.insert(
                format!("u{i}"),
                serde_json::json!({"anyOf": [{"type": "string"}, {"type": "integer"}]}),
            );
        }
        let required: Vec<String> = (0..n).map(|i| format!("u{i}")).collect();
        serde_json::json!({"type": "object", "properties": props, "required": required})
    };
    let over = vec![schema_tool("edit", union(17), None)];
    assert_all_original(&request_tools(&a, &over), &over);
    let at = vec![schema_tool("edit", union(16), None)];
    assert_eq!(strict_of(&request_tools(&a, &at)[0]), Some(true));
}

#[test]
fn oauth_prefixes_names_but_matches_strict_tools_by_unprefixed_name() {
    let mut a = anthropic_strict("claude-sonnet-5-5");
    a.api_key = "sk-ant-oat01-test".into();
    let sent = request_tools(
        &a,
        &[
            schema_tool("edit", simple_schema(), None),
            schema_tool("glob", simple_schema(), None),
        ],
    );
    assert_eq!(strict_of(tool_named(&sent, "mcp_Edit")), Some(true));
    assert!(tool_named(&sent, "mcp_Glob").get("strict").is_none());
}

#[test]
fn oauth_honours_explicit_strictness_like_api_key() {
    let mut oauth = anthropic_for("claude-sonnet-5-5", None, 4096);
    oauth.api_key = "sk-ant-oat01-test".into();
    let sent = request_tools(
        &oauth,
        &[schema_tool("explicit", simple_schema(), Some(true))],
    );
    assert_eq!(strict_of(&sent[0]), Some(true));
}

#[test]
fn strict_settings_parse_from_documented_config() {
    let a: Anthropic = serde_json::from_value(serde_json::json!({
        "api_key": "sk-ant-api03-test",
        "model": "claude-sonnet-5-5",
        "max_tokens": 4096,
        "strict_tools": ["edit", "glob"]
    }))
    .unwrap();
    assert_eq!(a.strict_tools, Some(vec!["edit".into(), "glob".into()]));

    let empty: Anthropic = serde_json::from_value(serde_json::json!({
        "api_key": "k", "model": "m", "max_tokens": 1, "strict_tools": []
    }))
    .unwrap();
    assert_eq!(empty.strict_tools, Some(vec![]));

    let default: Anthropic = serde_json::from_value(serde_json::json!({
        "api_key": "k", "model": "m", "max_tokens": 1
    }))
    .unwrap();
    assert_eq!(default.strict_tools, None);

    let factory = create_http_factory();
    let schema: Value = serde_json::from_str(&factory.config_schema()).unwrap();
    assert!(schema["properties"]["strict_tools"].is_object());
    factory
        .from_config(
            r#"{"api_key":"k","model":"claude-sonnet-5-5","max_tokens":1,"strict_tools":["edit"]}"#,
        )
        .expect("factory accepts strict_tools");
}
