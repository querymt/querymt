//! Unit tests: tool changes inside the conversation.

use crate::config::Anthropic;
use crate::tests::support::{anthropic_for, test_anthropic, test_tool};
use crate::tool_changes::{INLINE_TOOLS_BETA, place_tool_changes};
use crate::wire::request::{AnthropicMessage, MessageContent};
use querymt::chat::http::HTTPChatProvider;
use querymt::chat::{
    CacheHint, ChatCall, ChatMessage, FunctionTool, Tool, ToolChange, ToolResultPart,
};
use serde_json::{Value, json};

const SUPPORTED: &str = "claude-sonnet-5-5";

fn supported() -> Anthropic {
    anthropic_for(SUPPORTED, None, 4096)
}

fn change(added: Vec<Tool>, removed: &[&str]) -> ChatMessage {
    ChatMessage::from_tool_change(ToolChange::new(
        added,
        removed.iter().map(|name| name.to_string()).collect(),
    ))
}

#[test]
fn a_baseline_is_never_sent_to_the_provider() {
    let messages = [
        user("go"),
        ChatMessage::from_tool_change(ToolChange::baseline(vec![test_tool("deploy")], "x")),
    ];
    let err = supported()
        .prepare_chat_request(&ChatCall::new(&messages), false)
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("baseline"), "{err}");
}

fn user(text: &str) -> ChatMessage {
    ChatMessage::user().text(text).build()
}

fn strict_tool(name: &str) -> Tool {
    Tool {
        tool_type: "function".into(),
        function: FunctionTool {
            name: name.into(),
            description: "strict tool".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            strict: Some(true),
        },
    }
}

fn prepare(
    a: &Anthropic,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> querymt::chat::http::PreparedChatRequest {
    a.prepare_chat_request(&ChatCall::new(messages).maybe_tools(tools), false)
        .expect("request builds")
}

fn body_of(prepared: &querymt::chat::http::PreparedChatRequest) -> Value {
    serde_json::from_slice(prepared.request.body()).expect("valid JSON")
}

fn roles(body: &Value) -> Vec<&str> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect()
}

fn beta_header(prepared: &querymt::chat::http::PreparedChatRequest) -> Option<String> {
    prepared
        .request
        .headers()
        .get("anthropic-beta")
        .map(|v| v.to_str().unwrap().to_string())
}

// ---- declaration per model ----

#[test]
fn supporting_model_declares_tool_updates() {
    for id in [
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
        "claude-mythos-5",
    ] {
        let a = anthropic_for(id, None, 4096);
        assert!(a.capabilities().tool_updates, "{id}");
    }
}

#[test]
fn other_model_declares_no_tool_updates() {
    for id in [
        "claude-opus-4-7",
        "claude-sonnet-4-6",
        "claude-3-7-sonnet-20250219",
        "my-proxy-model",
    ] {
        let a = anthropic_for(id, None, 4096);
        assert!(!a.capabilities().tool_updates, "{id}");
    }
}

#[test]
fn sibling_model_id_does_not_inherit_support() {
    for id in ["claude-sonnet-5", "claude-haiku-4-5-20251001"] {
        let a = anthropic_for(id, None, 4096);
        assert!(!a.capabilities().tool_updates, "{id}");
    }
}

#[test]
fn dated_id_of_a_supporting_model_declares_tool_updates() {
    for id in ["claude-sonnet-5-5-20260301", "claude-opus-4-8-20260301"] {
        let a = anthropic_for(id, None, 4096);
        assert!(a.capabilities().tool_updates, "{id}");
    }
}

// ---- wire shape ----

#[test]
fn added_tool_is_sent_as_a_system_entry_and_not_in_tools() {
    let base = [test_tool("read")];
    let messages = [user("go"), change(vec![test_tool("deploy")], &[])];
    let body = body_of(&prepare(&supported(), &messages, Some(&base)));

    let entry = &body["messages"][1];
    assert_eq!(entry["role"], "system");
    let blocks = entry["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "tool_addition");
    assert_eq!(blocks[0]["tool"]["type"], "tool_definition");
    assert_eq!(blocks[0]["tool"]["definition"]["name"], "deploy");
    assert_eq!(blocks[0]["tool"]["definition"]["description"], "test tool");
    let names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["read"], "top-level tools are not changed");
}

#[test]
fn removed_tool_is_sent_as_a_reference_and_stays_in_tools() {
    let base = [test_tool("read"), test_tool("deploy")];
    let messages = [user("go"), change(Vec::new(), &["deploy"])];
    let body = body_of(&prepare(&supported(), &messages, Some(&base)));

    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        blocks[0],
        json!({"type": "tool_removal", "tool": {"type": "tool_reference", "name": "deploy"}})
    );
    assert_eq!(body["tools"].as_array().unwrap().len(), 2);
}

#[test]
fn redefined_tool_has_one_addition_and_no_removal() {
    let base = [test_tool("skill")];
    let mut new_skill = test_tool("skill");
    new_skill.function.description = "new description".into();
    let messages = [user("go"), change(vec![new_skill], &[])];
    let body = body_of(&prepare(&supported(), &messages, Some(&base)));

    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "tool_addition");
    assert_eq!(
        blocks[0]["tool"]["definition"]["description"],
        "new description"
    );
}

#[test]
fn additions_come_before_removals_in_the_given_order() {
    let base = [test_tool("read")];
    let messages = [
        user("go"),
        change(
            vec![test_tool("alpha"), test_tool("beta")],
            &["gone", "also_gone"],
        ),
    ];
    let body = body_of(&prepare(&supported(), &messages, Some(&base)));

    let blocks = body["messages"][1]["content"].as_array().unwrap();
    let seen: Vec<(String, String)> = blocks
        .iter()
        .map(|b| {
            let name = b["tool"]["definition"]["name"]
                .as_str()
                .or_else(|| b["tool"]["name"].as_str())
                .unwrap()
                .to_string();
            (b["type"].as_str().unwrap().to_string(), name)
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("tool_addition".into(), "alpha".into()),
            ("tool_addition".into(), "beta".into()),
            ("tool_removal".into(), "gone".into()),
            ("tool_removal".into(), "also_gone".into()),
        ]
    );
}

#[test]
fn strict_tool_added_by_a_change_carries_the_flag_and_converted_schema() {
    let base = [test_tool("read")];
    let messages = [user("go"), change(vec![strict_tool("edit")], &[])];
    let body = body_of(&prepare(&supported(), &messages, Some(&base)));

    let definition = &body["messages"][1]["content"][0]["tool"]["definition"];
    assert_eq!(definition["strict"], true);
    assert_eq!(definition["input_schema"]["additionalProperties"], false);
}

// ---- strict limits across tool changes ----
//
// The API applies the strict limits to the tools in effect (verified live,
// 2026-11-20): a removal or a redefinition frees its slot, an added strict
// tool counts. The entry order (additions, then removals) does not matter.

/// The most strict tools one request may carry.
const MAX_STRICT: usize = 20;

fn full_strict_list() -> Vec<Tool> {
    (0..MAX_STRICT)
        .map(|i| strict_tool(&format!("t{i:02}")))
        .collect()
}

fn assistant(text: &str) -> ChatMessage {
    ChatMessage::assistant().text(text).build()
}

/// Request body whose conversation ends with the given tool changes, each
/// directly after a user message.
fn body_with_changes(changes: Vec<ChatMessage>) -> Value {
    let mut messages = vec![user("go")];
    for (i, change) in changes.into_iter().enumerate() {
        if i > 0 {
            messages.push(assistant("ok"));
            messages.push(user("next"));
        }
        messages.push(change);
    }
    let base = full_strict_list();
    body_of(&prepare(&supported(), &messages, Some(&base)))
}

/// The `strict` flag of the first definition in the system entry at `index`.
fn strict_of_entry(body: &Value, index: usize) -> Value {
    body["messages"][index]["content"][0]["tool"]["definition"]["strict"].clone()
}

#[test]
fn removal_frees_a_strict_slot_for_an_added_tool() {
    let body = body_with_changes(vec![change(vec![strict_tool("t20")], &["t19"])]);
    assert_eq!(strict_of_entry(&body, 1), true);
}

#[test]
fn redefinition_counts_once_against_the_strict_limit() {
    let mut redefined = strict_tool("t00");
    redefined.function.description = "redefined".into();
    let body = body_with_changes(vec![change(vec![redefined], &[])]);
    assert_eq!(strict_of_entry(&body, 1), true);
}

#[test]
fn strict_tool_over_the_limit_is_sent_without_strict() {
    let body = body_with_changes(vec![change(vec![strict_tool("t20")], &[])]);
    assert_ne!(strict_of_entry(&body, 1), true);
}

#[test]
fn redefining_a_tool_without_strict_frees_its_slot_for_a_later_change() {
    let body = body_with_changes(vec![
        change(vec![test_tool("t00")], &[]),
        change(vec![strict_tool("t20")], &[]),
    ]);
    assert_ne!(
        strict_of_entry(&body, 1),
        true,
        "the redefinition is not strict"
    );
    assert_eq!(strict_of_entry(&body, 4), true, "the freed slot is used");
}

#[test]
fn a_removed_tool_stays_removed_for_the_budget_of_later_changes() {
    let body = body_with_changes(vec![
        change(Vec::new(), &["t19"]),
        change(vec![strict_tool("t20")], &[]),
        change(vec![strict_tool("t21")], &[]),
    ]);
    assert_eq!(strict_of_entry(&body, 4), true, "takes the freed slot");
    assert_ne!(strict_of_entry(&body, 7), true, "the list is full again");
}

#[test]
fn appending_a_change_does_not_alter_an_earlier_entry() {
    let first = change(vec![strict_tool("t20")], &["t19"]);
    let earlier = body_with_changes(vec![first.clone()]);
    let later = body_with_changes(vec![first, change(vec![strict_tool("t21")], &[])]);
    assert_eq!(earlier["messages"][1], later["messages"][1]);
}

#[test]
fn removal_names_the_tool_only() {
    let messages = [user("go"), change(Vec::new(), &["deploy"])];
    let body = body_of(&prepare(&supported(), &messages, None));
    let tool = &body["messages"][1]["content"][0]["tool"];
    assert_eq!(tool, &json!({"type": "tool_reference", "name": "deploy"}));
}

// ---- placement ----

#[test]
fn trailing_user_message_goes_before_the_tool_change_entry() {
    let messages = [
        user("go"),
        change(vec![test_tool("deploy")], &[]),
        user("tail"),
    ];
    let body = body_of(&prepare(&supported(), &messages, None));
    assert_eq!(roles(&body), ["user", "user", "system"]);
    assert_eq!(body["messages"][1]["content"][0]["text"], "tail");
}

#[test]
fn new_prompt_after_an_interrupted_run_comes_before_the_entry() {
    let reply = ChatMessage::assistant().text("done").build();
    let messages = [
        user("first"),
        reply,
        user("second"),
        change(vec![test_tool("deploy")], &[]),
        user("third"),
        ChatMessage::assistant().text("ok").build(),
    ];
    let body = body_of(&prepare(&supported(), &messages, None));
    assert_eq!(
        roles(&body),
        ["user", "assistant", "user", "user", "system", "assistant"]
    );
}

#[test]
fn tool_change_after_tool_results_is_sent_in_place() {
    let messages = [
        user("go"),
        ChatMessage::assistant()
            .tool_use("toolu_1", "read", json!({}))
            .build(),
        ChatMessage::user()
            .tool_result(
                "toolu_1".into(),
                Some("read".into()),
                false,
                vec![ToolResultPart::Text { text: "ok".into() }],
            )
            .build(),
        change(vec![test_tool("deploy")], &[]),
    ];
    let body = body_of(&prepare(
        &supported(),
        &messages,
        Some(&[test_tool("read")]),
    ));
    assert_eq!(roles(&body), ["user", "assistant", "user", "system"]);
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
}

#[test]
fn several_tool_changes_keep_their_relative_order() {
    let messages = [
        user("go"),
        change(vec![test_tool("one")], &[]),
        change(vec![test_tool("two")], &[]),
        user("tail"),
    ];
    let body = body_of(&prepare(&supported(), &messages, None));
    assert_eq!(roles(&body), ["user", "user", "system", "system"]);
    assert_eq!(
        body["messages"][2]["content"][0]["tool"]["definition"]["name"],
        "one"
    );
    assert_eq!(
        body["messages"][3]["content"][0]["tool"]["definition"]["name"],
        "two"
    );
}

#[test]
fn tool_change_first_in_the_conversation_fails_before_sending() {
    let messages = [change(vec![test_tool("deploy")], &[]), user("go")];
    let err = supported()
        .prepare_chat_request(&ChatCall::new(&messages), false)
        .err()
        .expect("must fail");
    assert!(
        err.to_string().contains("first message"),
        "unexpected error: {err}"
    );
}

#[test]
fn tool_change_that_would_follow_an_assistant_turn_fails() {
    let entries = vec![
        AnthropicMessage {
            role: "user",
            content: Vec::new(),
        },
        AnthropicMessage {
            role: "assistant",
            content: Vec::new(),
        },
        AnthropicMessage {
            role: "system",
            content: vec![MessageContent::ToolChange {
                content_type: "tool_removal",
                tool: json!({"type": "tool_reference", "name": "x"}),
            }],
        },
    ];
    let err = place_tool_changes(entries).expect_err("must fail");
    assert!(err.to_string().contains("follow a user message"), "{err}");
}

#[test]
fn message_without_tool_changes_is_left_alone() {
    let messages = [user("go"), ChatMessage::assistant().text("hi").build()];
    let body = body_of(&prepare(&supported(), &messages, None));
    assert_eq!(roles(&body), ["user", "assistant"]);
}

// ---- cache marker ----

#[test]
fn hinted_tool_change_message_places_no_marker() {
    let mut hinted = change(vec![test_tool("deploy")], &[]);
    hinted.cache = Some(CacheHint::Ephemeral { ttl_seconds: None });
    let messages = [user("go"), hinted];
    let prepared = prepare(&supported(), &messages, None);
    let text = String::from_utf8(prepared.request.body().clone()).unwrap();
    assert!(
        !text.contains("cache_control"),
        "no marker may be placed: {text}"
    );
}

// ---- beta header ----

#[test]
fn header_is_added_with_a_tool_change_next_to_another_beta() {
    let mut a = test_anthropic("sk-ant-oat01-abc123");
    a.model = SUPPORTED.into();
    let messages = [user("go"), change(vec![test_tool("deploy")], &[])];
    let header = beta_header(&prepare(&a, &messages, None)).expect("header");
    assert!(header.contains("oauth-2025-04-20"), "{header}");
    assert!(header.contains(INLINE_TOOLS_BETA), "{header}");
}

#[test]
fn header_is_added_on_the_api_key_path() {
    let messages = [user("go"), change(vec![test_tool("deploy")], &[])];
    let header = beta_header(&prepare(&supported(), &messages, None)).expect("header");
    assert_eq!(header, INLINE_TOOLS_BETA);
}

#[test]
fn no_header_without_a_tool_change() {
    let messages = [user("go")];
    assert_eq!(beta_header(&prepare(&supported(), &messages, None)), None);

    let mut oauth = test_anthropic("sk-ant-oat01-abc123");
    oauth.model = SUPPORTED.into();
    let header = beta_header(&prepare(&oauth, &messages, None)).expect("oauth header");
    assert!(!header.contains(INLINE_TOOLS_BETA), "{header}");
}

// ---- unsupported model ----

#[test]
fn tool_change_for_an_unsupported_model_fails_naming_the_model() {
    let a = anthropic_for("claude-sonnet-5", None, 4096);
    let messages = [user("go"), change(vec![test_tool("deploy")], &[])];
    let err = a
        .prepare_chat_request(&ChatCall::new(&messages), false)
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("claude-sonnet-5"), "{err}");
}

// ---- OAuth tool names ----

#[test]
fn oauth_prefixes_added_and_removed_names_and_maps_them_back() {
    let mut a = test_anthropic("sk-ant-oat01-abc123");
    a.model = SUPPORTED.into();
    let messages = [user("go"), change(vec![test_tool("deploy")], &["old_tool"])];
    let prepared = prepare(&a, &messages, None);
    let body = body_of(&prepared);

    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["tool"]["definition"]["name"], "mcp_Deploy");
    assert_eq!(blocks[1]["tool"]["name"], "mcp_Old_tool");
    assert_eq!(prepared.context["mcp_Deploy"], "deploy");
}

#[test]
fn tool_added_by_a_change_maps_back_for_api_key_requests() {
    let messages = [user("go"), change(vec![test_tool("deploy")], &[])];
    let prepared = prepare(&supported(), &messages, None);
    assert_eq!(prepared.context["deploy"], "deploy");
}

#[test]
fn two_added_names_that_encode_alike_fail() {
    let mut a = test_anthropic("sk-ant-oat01-abc123");
    a.model = SUPPORTED.into();
    let messages = [
        user("go"),
        change(vec![test_tool("deploy"), test_tool("Deploy")], &[]),
    ];
    let err = a
        .prepare_chat_request(&ChatCall::new(&messages), false)
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("both encode"), "{err}");
}

#[test]
fn redefining_a_tool_in_a_later_change_is_not_a_name_conflict() {
    let messages = [
        user("go"),
        change(vec![test_tool("deploy")], &[]),
        user("again"),
        ChatMessage::assistant().text("ok").build(),
        change(vec![test_tool("deploy")], &[]),
    ];
    // Last entry follows an assistant turn only after "again", so add a user.
    let mut messages = Vec::from(messages);
    messages.push(user("more"));
    let prepared = prepare(&supported(), &messages, None);
    assert_eq!(prepared.context["deploy"], "deploy");
}

// ---- messages built wrongly ----

#[test]
fn empty_tool_change_is_rejected() {
    let messages = [user("go"), change(Vec::new(), &[])];
    let err = supported()
        .prepare_chat_request(&ChatCall::new(&messages), false)
        .err()
        .expect("must fail");
    assert!(
        err.to_string().contains("must add, redefine or remove"),
        "{err}"
    );
}
