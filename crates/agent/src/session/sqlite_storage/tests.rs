use std::collections::HashMap;

use querymt::LLMParams;
use querymt::chat::ChatRole;
use rusqlite::Connection;

use crate::acp::protocol::{ContentBlock, ImageContent, TextContent};
use crate::agent::core::AgentMode;
use crate::agent::session_control::{SessionControlState, SessionModelBinding};
use crate::events::{AgentEventKind, EventOrigin};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::ForkOrigin;
use crate::session::projection::{
    EventJournal, NewDurableEvent, RecentModelEntry, SessionScope, ViewStore,
};
use crate::session::store::{RemoteSessionBookmark, SessionStore};

use super::SqliteStorage;
use super::migrations::{MIGRATIONS, apply_migrations};

#[tokio::test]
async fn prompt_blocks_round_trip_through_persistence_and_fork() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let session = storage
        .create_session(Some("source".to_string()), None, None, None)
        .await
        .unwrap();
    let message = AgentMessage {
        id: "message-1".to_string(),
        session_id: session.public_id.clone(),
        role: ChatRole::User,
        parts: vec![MessagePart::Prompt {
            blocks: vec![
                ContentBlock::Text(TextContent::new("before")),
                ContentBlock::Image(ImageContent::new("AQID", "image/png")),
                ContentBlock::Text(TextContent::new("after")),
            ],
        }],
        created_at: 1,
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };
    storage
        .add_message(&session.public_id, message.clone())
        .await
        .unwrap();

    let history = storage.get_history(&session.public_id).await.unwrap();
    assert_eq!(history[0].parts, message.parts);

    let fork_id = storage
        .fork_session(&session.public_id, "message-1", ForkOrigin::User)
        .await
        .unwrap();
    let fork_history = storage.get_history(&fork_id).await.unwrap();
    assert_eq!(fork_history.len(), 1);
    assert_eq!(fork_history[0].parts, message.parts);
}

#[tokio::test]
async fn legacy_reasoning_row_resaves_as_canonical_persistence() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let source = storage
        .create_session(Some("legacy-source".to_string()), None, None, None)
        .await
        .unwrap();
    let message = AgentMessage {
        id: "legacy-reasoning".to_string(),
        session_id: source.public_id.clone(),
        role: ChatRole::Assistant,
        parts: vec![MessagePart::Reasoning {
            item: querymt::chat::ChatReasoningItem {
                id: None,
                summary: Vec::new(),
                content: vec![querymt::chat::ChatReasoningPart::text("old thought")],
                encrypted_content: None,
                signature: Some("legacy-signature".into()),
                status: None,
                extensions: Default::default(),
            },
            time_ms: Some(7),
        }],
        created_at: 1,
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };
    storage
        .add_message(&source.public_id, message)
        .await
        .unwrap();

    let legacy_json = serde_json::json!({
        "type": "Reasoning",
        "data": {
            "content": "old thought",
            "signature": "legacy-signature",
            "time_ms": 7
        }
    })
    .to_string();
    storage
        .conn_for_test()
        .lock()
        .unwrap()
        .execute(
            "UPDATE message_parts SET content_json = ? WHERE message_id = (SELECT id FROM messages WHERE public_id = ?)",
            rusqlite::params![legacy_json, "legacy-reasoning"],
        )
        .unwrap();

    let mut reloaded = storage
        .get_history(&source.public_id)
        .await
        .unwrap()
        .remove(0);
    let MessagePart::Reasoning { item, .. } = &reloaded.parts[0] else {
        panic!("expected normalized reasoning part");
    };
    assert_eq!(item.visible_text(), "old thought");
    assert_eq!(item.signature.as_deref(), Some("legacy-signature"));

    let destination = storage
        .create_session(Some("canonical-destination".to_string()), None, None, None)
        .await
        .unwrap();
    reloaded.id = "canonical-reasoning".to_string();
    reloaded.session_id = destination.public_id.clone();
    storage
        .add_message(&destination.public_id, reloaded)
        .await
        .unwrap();

    let saved: String = storage
        .conn_for_test()
        .lock()
        .unwrap()
        .query_row(
            "SELECT content_json FROM message_parts WHERE message_id = (SELECT id FROM messages WHERE public_id = ?)",
            ["canonical-reasoning"],
            |row| row.get(0),
        )
        .unwrap();
    let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["type"], "Reasoning");
    assert!(saved["data"]["content"].is_array());
    assert_eq!(saved["data"]["content"][0]["text"], "old thought");
    assert_eq!(saved["data"]["signature"], "legacy-signature");
}

#[tokio::test]
async fn canonical_output_part_round_trips_through_sqlite_reload() {
    use querymt::chat::{
        ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOpaqueItem, ChatOutput,
        ChatOutputItem, ChatOutputProvenance, ChatOutputStatus, ChatReasoningItem,
    };

    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let session = storage
        .create_session(Some("source".to_string()), None, None, None)
        .await
        .unwrap();

    let output = ChatOutput {
        response_id: Some("resp_1".into()),
        status: Some(ChatOutputStatus::Completed),
        provenance: Some(ChatOutputProvenance {
            provider: "openai".into(),
            protocol: "responses".into(),
            model: "gpt-5".into(),
            endpoint: "https://api.openai.com/v1/responses".into(),
        }),
        items: vec![
            // Encrypted-only reasoning: no visible summary, opaque continuation.
            ChatOutputItem::Reasoning(ChatReasoningItem {
                id: Some("reasoning_1".into()),
                summary: Vec::new(),
                content: Vec::new(),
                encrypted_content: Some("encrypted-continuation".into()),
                signature: Some("sig-1".into()),
                status: None,
                extensions: Default::default(),
            }),
            ChatOutputItem::Message(ChatMessageItem {
                id: Some("message_1".into()),
                role: ChatRole::Assistant,
                phase: None,
                status: None,
                parts: vec![ChatMessagePart::Text {
                    text: "answer".into(),
                    annotations: Vec::new(),
                    extensions: Default::default(),
                }],
                extensions: Default::default(),
            }),
            ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: Some("item_1".into()),
                call_id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{\"query\":\"rust\",\"raw\": 1 }".into(),
                status: None,
                extensions: Default::default(),
            }),
            ChatOutputItem::Opaque(ChatOpaqueItem {
                original_type: "future_action".into(),
                payload: serde_json::json!({"vendor": true}),
            }),
        ],
        ..ChatOutput::default()
    };
    let message = AgentMessage {
        id: "assistant-1".to_string(),
        session_id: session.public_id.clone(),
        role: ChatRole::Assistant,
        parts: vec![MessagePart::Output { output }],
        created_at: 1,
        parent_message_id: None,
        source_provider: Some("openai".to_string()),
        source_model: Some("gpt-5".to_string()),
    };
    storage
        .add_message(&session.public_id, message)
        .await
        .unwrap();

    let history = storage.get_history(&session.public_id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].parts.len(),
        1,
        "one canonical part, no duplicates"
    );

    let MessagePart::Output { output: reloaded } = &history[0].parts[0] else {
        panic!("expected canonical Output part after reload");
    };
    assert_eq!(reloaded.items.len(), 4);

    let ChatOutputItem::Reasoning(reasoning) = &reloaded.items[0] else {
        panic!("expected reasoning item");
    };
    assert!(reasoning.summary.is_empty());
    assert_eq!(
        reasoning.encrypted_content.as_deref(),
        Some("encrypted-continuation"),
        "encrypted-only reasoning survives reload"
    );
    assert_eq!(reasoning.signature.as_deref(), Some("sig-1"));

    let ChatOutputItem::FunctionCall(call) = &reloaded.items[2] else {
        panic!("expected function call item");
    };
    assert_eq!(call.call_id, "call_1");
    assert_eq!(call.item_id.as_deref(), Some("item_1"));
    assert_eq!(
        call.arguments, "{\"query\":\"rust\",\"raw\": 1 }",
        "raw arguments reload byte-exact"
    );

    assert!(matches!(reloaded.items[3], ChatOutputItem::Opaque(_)));

    // Reloaded history projects once into provider request content without
    // duplicated parts. Without a native target the turn becomes the canonical
    // portable output: message text and call arguments, each exactly once, with
    // the call ID retained for correlation and native/opaque state removed.
    let chat = history[0].to_chat_message().unwrap();
    let portable = chat
        .output()
        .expect("portable projection retains canonical call correlation");
    assert!(
        !portable.requires_item_aware_fidelity(),
        "portable projection carries no native structured continuation"
    );
    assert!(
        portable
            .function_calls()
            .any(|call| call.call_id == "call_1"),
        "portable projection retains the call ID for call/result correlation"
    );
    assert_eq!(
        portable.items.len(),
        2,
        "encrypted-only reasoning and opaque items are dropped"
    );
    let call = portable
        .function_calls()
        .next()
        .expect("portable projection keeps the canonical function call");
    assert_eq!(call.call_id, "call_1");
    assert!(
        call.item_id.is_none(),
        "native item identity is stripped from the portable projection"
    );
    assert_eq!(
        call.arguments, "{\"query\":\"rust\",\"raw\": 1 }",
        "call arguments stay byte-exact"
    );
    let projected = chat.portable_input_parts();
    let text_parts = projected
        .iter()
        .filter(|part| part.as_text().is_some())
        .count();
    assert_eq!(
        text_parts, 1,
        "message text projects to input; calls stay canonical"
    );
}

fn structured_tool_exchange(session_id: &str) -> (AgentMessage, AgentMessage, AgentMessage) {
    use querymt::chat::{
        ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem,
    };

    let assistant = AgentMessage {
        id: "assistant-turn".to_string(),
        session_id: session_id.to_string(),
        role: ChatRole::Assistant,
        parts: vec![MessagePart::Output {
            output: ChatOutput {
                items: vec![
                    ChatOutputItem::Message(ChatMessageItem {
                        id: None,
                        role: ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![ChatMessagePart::Text {
                            text: "running lookup".into(),
                            annotations: Vec::new(),
                            extensions: Default::default(),
                        }],
                        extensions: Default::default(),
                    }),
                    ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                        item_id: Some("item_1".into()),
                        call_id: "call_1".into(),
                        name: "lookup".into(),
                        arguments: "{\"query\":\"rust\"}".into(),
                        status: None,
                        extensions: Default::default(),
                    }),
                ],
                ..ChatOutput::default()
            },
        }],
        created_at: 1,
        parent_message_id: None,
        source_provider: Some("openai".to_string()),
        source_model: Some("gpt-5".to_string()),
    };
    let result = AgentMessage {
        id: "result-turn".to_string(),
        session_id: session_id.to_string(),
        role: ChatRole::User,
        parts: vec![MessagePart::ToolResult {
            call_id: "call_1".to_string(),
            content: vec![querymt::chat::ToolResultPart::text("result payload")],
            is_error: false,
            tool_name: Some("lookup".to_string()),
            tool_arguments: Some("{\"query\":\"rust\"}".to_string()),
            compacted_at: None,
        }],
        created_at: 2,
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };
    let follow_up = AgentMessage {
        id: "user-follow-up".to_string(),
        session_id: session_id.to_string(),
        role: ChatRole::User,
        parts: vec![MessagePart::Text {
            content: "and now?".to_string(),
        }],
        created_at: 3,
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };
    (assistant, result, follow_up)
}

/// History edits delete structured output together with its projections: after
/// an edit frontier, neither the canonical payload nor any projection survives
/// in storage, so nothing stale can be resurrected on reload.
#[tokio::test]
async fn edit_removes_structured_authority_without_resurrection() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let session = storage
        .create_session(Some("source".to_string()), None, None, None)
        .await
        .unwrap();
    let (assistant, result, follow_up) = structured_tool_exchange(&session.public_id);
    for message in [&assistant, &result, &follow_up] {
        storage
            .add_message(&session.public_id, message.clone())
            .await
            .unwrap();
    }

    // Edit frontier at the assistant turn removes the whole dependency group.
    let deleted = storage
        .delete_messages_after(&session.public_id, "assistant-turn")
        .await
        .unwrap();
    assert_eq!(deleted, 3);

    let history = storage.get_history(&session.public_id).await.unwrap();
    assert!(history.is_empty(), "edit frontier drops the exchange");

    // No sidecar: a full storage scan finds no structured residue.
    let conn = storage.conn_for_test();
    let conn = conn.lock().unwrap();
    let residue: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM message_parts WHERE part_type = 'output'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(residue, 0, "no hidden structured sidecar remains");
}

/// Pruning replaces result content with placeholders but keeps the
/// call/result mapping, so structured replay never dangles.
#[tokio::test]
async fn pruning_keeps_call_result_mapping_for_structured_turns() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let session = storage
        .create_session(Some("source".to_string()), None, None, None)
        .await
        .unwrap();
    let (assistant, result, follow_up) = structured_tool_exchange(&session.public_id);
    for message in [&assistant, &result, &follow_up] {
        storage
            .add_message(&session.public_id, message.clone())
            .await
            .unwrap();
    }

    let updated = storage
        .mark_tool_results_compacted(&session.public_id, &["call_1".to_string()])
        .await
        .unwrap();
    assert_eq!(updated, 1);

    let history = storage.get_history(&session.public_id).await.unwrap();
    assert_eq!(history.len(), 3);

    // The canonical structured output part is untouched by pruning.
    let MessagePart::Output { output } = &history[0].parts[0] else {
        panic!("structured authority must be maintained across pruning");
    };
    assert_eq!(output.items.len(), 2);
    assert_eq!(
        history[0].parts, assistant.parts,
        "canonical output is byte-identical after pruning"
    );

    // The result still references the original call ID with placeholder content.
    let chat = history[1].to_chat_message().unwrap();
    let result = chat
        .portable_input_parts()
        .into_iter()
        .find_map(|part| match part {
            querymt::chat::ChatInputPart::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result still present");
    assert_eq!(result.call_id, "call_1");
    assert_eq!(
        result.parts[0].as_text(),
        Some("[Old tool result content cleared]")
    );
}

/// Rich media in structured output must survive storage reload with MIME
/// parameters, filename/detail, source form, and provider-reference scope
/// intact — and an unresolved reference must not imply renderable bytes.
#[tokio::test]
async fn structured_output_media_round_trips_through_sqlite_reload() {
    use querymt::chat::{
        ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem, ChatOutputProvenance,
        MediaKind, MediaPart, MediaSource,
    };

    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let session = storage
        .create_session(Some("source".to_string()), None, None, None)
        .await
        .unwrap();

    let origin = ChatOutputProvenance {
        provider: "provider-a".into(),
        protocol: "responses".into(),
        model: "model-a".into(),
        endpoint: "https://a.invalid/v1/responses".into(),
    };

    let mut inline = MediaPart::new(
        MediaKind::Document,
        Some("application/pdf; charset=binary".parse().unwrap()),
        MediaSource::Inline {
            data: vec![0x25, 0x50, 0x44, 0x46],
        },
    )
    .unwrap();
    inline.filename = Some("report.pdf".into());
    inline.detail = Some("high".into());

    let mut reference = MediaPart::new(
        MediaKind::Document,
        None,
        MediaSource::ProviderFile {
            file_id: "file_abc".into(),
            origin: origin.clone(),
        },
    )
    .unwrap();
    reference.filename = Some("remote.pdf".into());

    let message = AgentMessage {
        id: "media-turn".to_string(),
        session_id: session.public_id.clone(),
        role: ChatRole::Assistant,
        parts: vec![MessagePart::Output {
            output: ChatOutput {
                provenance: Some(origin.clone()),
                items: vec![ChatOutputItem::Message(ChatMessageItem {
                    id: None,
                    role: ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![
                        ChatMessagePart::Media(Box::new(inline.clone())),
                        ChatMessagePart::Media(Box::new(reference.clone())),
                    ],
                    extensions: Default::default(),
                })],
                ..ChatOutput::default()
            },
        }],
        created_at: 1,
        parent_message_id: None,
        source_provider: Some("provider-a".to_string()),
        source_model: Some("model-a".to_string()),
    };
    storage
        .add_message(&session.public_id, message.clone())
        .await
        .unwrap();

    let history = storage.get_history(&session.public_id).await.unwrap();
    assert_eq!(
        history[0].parts, message.parts,
        "media survives reload exactly"
    );

    let MessagePart::Output { output } = &history[0].parts[0] else {
        panic!("expected canonical output part");
    };
    let ChatOutputItem::Message(item) = &output.items[0] else {
        panic!("expected message item");
    };
    let ChatMessagePart::Media(reloaded_inline) = &item.parts[0] else {
        panic!("expected inline media");
    };
    assert_eq!(**reloaded_inline, inline);
    let media_type = reloaded_inline.media_type().unwrap();
    assert_eq!(media_type.type_(), "application");
    assert_eq!(media_type.subtype(), "pdf");
    assert_eq!(media_type.parameter("charset"), Some("binary"));

    let ChatMessagePart::Media(reloaded_reference) = &item.parts[1] else {
        panic!("expected provider reference media");
    };
    assert_eq!(**reloaded_reference, reference);
    assert!(matches!(
        &reloaded_reference.source(),
        MediaSource::ProviderFile { file_id, origin: o } if file_id == "file_abc" && o == &origin
    ));
    assert!(matches!(
        reloaded_reference.source(),
        MediaSource::ProviderFile { .. }
    ));
}

/// End-to-end roundtrip: response -> persist -> reload -> tool result ->
/// second request. The reloaded assistant turn must replay through the provider
/// conversion with exact raw arguments, preserved order, distinct item/call
/// IDs, encrypted reasoning continuation, and no duplicate projected items.
#[tokio::test]
async fn structured_response_persist_reload_then_second_request_round_trips() {
    use querymt::chat::{
        ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem,
        ChatOutputProvenance, ChatOutputStatus, ChatReasoningItem, ChatReasoningPart,
        ToolResultPart,
    };

    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let session = storage
        .create_session(Some("source".to_string()), None, None, None)
        .await
        .unwrap();

    // Raw (non-canonical) argument text must survive byte-exact.
    let raw_arguments = r#"{"query":"rust","limit": 2 }"#;

    let output = ChatOutput {
        response_id: Some("resp_e2e".into()),
        status: Some(ChatOutputStatus::Completed),
        provenance: Some(ChatOutputProvenance {
            provider: "openai".into(),
            protocol: "responses".into(),
            model: "gpt-5".into(),
            endpoint: "https://api.openai.com/v1/responses".into(),
        }),
        items: vec![
            ChatOutputItem::Reasoning(ChatReasoningItem {
                id: Some("reasoning_e2e".into()),
                summary: vec![ChatReasoningPart::text("thinking")],
                content: Vec::new(),
                encrypted_content: Some("encrypted-continuation-e2e".into()),
                signature: Some("sig-e2e".into()),
                status: None,
                extensions: Default::default(),
            }),
            ChatOutputItem::Message(ChatMessageItem {
                id: Some("message_e2e".into()),
                role: ChatRole::Assistant,
                phase: None,
                status: None,
                parts: vec![ChatMessagePart::Text {
                    text: "running lookup".into(),
                    annotations: Vec::new(),
                    extensions: Default::default(),
                }],
                extensions: Default::default(),
            }),
            ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: Some("fc_item_e2e".into()),
                call_id: "call_e2e".into(),
                name: "lookup".into(),
                arguments: raw_arguments.into(),
                status: None,
                extensions: Default::default(),
            }),
        ],
        ..ChatOutput::default()
    };

    let assistant = AgentMessage {
        id: "assistant-e2e".to_string(),
        session_id: session.public_id.clone(),
        role: ChatRole::Assistant,
        parts: vec![MessagePart::Output { output }],
        created_at: 1,
        parent_message_id: None,
        source_provider: Some("openai".to_string()),
        source_model: Some("gpt-5".to_string()),
    };
    let prompt = AgentMessage {
        id: "user-e2e".to_string(),
        session_id: session.public_id.clone(),
        role: ChatRole::User,
        parts: vec![MessagePart::Text {
            content: "look it up".to_string(),
        }],
        created_at: 0,
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };
    let result = AgentMessage {
        id: "result-e2e".to_string(),
        session_id: session.public_id.clone(),
        role: ChatRole::User,
        parts: vec![MessagePart::ToolResult {
            call_id: "call_e2e".to_string(),
            content: vec![ToolResultPart::text("result payload")],
            is_error: false,
            tool_name: Some("lookup".to_string()),
            tool_arguments: Some(raw_arguments.to_string()),
            compacted_at: None,
        }],
        created_at: 2,
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };

    for message in [&prompt, &assistant, &result] {
        storage
            .add_message(&session.public_id, message.clone())
            .await
            .unwrap();
    }

    // Reload from storage and convert to the provider request view.
    let history = storage.get_history(&session.public_id).await.unwrap();
    assert_eq!(history.len(), 3);

    let reloaded_assistant = &history[1];
    let MessagePart::Output { output: reloaded } = &reloaded_assistant.parts[0] else {
        panic!("expected canonical Output part after reload");
    };
    assert_eq!(
        reloaded.items.len(),
        3,
        "no duplicate projected items after reload"
    );

    let ChatOutputItem::Reasoning(reasoning) = &reloaded.items[0] else {
        panic!("expected reasoning item");
    };
    assert_eq!(
        reasoning.encrypted_content.as_deref(),
        Some("encrypted-continuation-e2e"),
        "reasoning continuation survives reload"
    );

    let ChatOutputItem::FunctionCall(call) = &reloaded.items[2] else {
        panic!("expected function call item");
    };
    assert_eq!(
        call.arguments, raw_arguments,
        "raw arguments reload byte-exact"
    );
    assert_eq!(call.call_id, "call_e2e");
    assert_eq!(call.item_id.as_deref(), Some("fc_item_e2e"));

    // Second request: same-origin conversion preserves the canonical structured
    // output alongside its deterministic projection so the request codec can
    // replay encrypted reasoning, item IDs, and byte-exact arguments.
    let chat_messages: Vec<querymt::chat::ChatMessage> = history
        .iter()
        .map(|m| {
            m.to_chat_message_with_output_target(
                Some(
                    &crate::model::OutputTarget::new("openai".to_string(), "gpt-5".to_string())
                        .with_protocol("responses")
                        .with_endpoint("https://api.openai.com/v1/responses"),
                ),
                None,
            )
            .unwrap()
        })
        .collect();

    // The canonical output survives to the second request: encrypted reasoning,
    // opaque/item IDs, and the original non-canonical arguments are all intact.
    let assistant_output = chat_messages[1]
        .output()
        .expect("exactly compatible target preserves canonical output");
    assert_eq!(assistant_output.items.len(), 3);
    assert_eq!(
        assistant_output
            .provenance
            .as_ref()
            .map(|provenance| provenance.endpoint.as_str()),
        Some("https://api.openai.com/v1/responses"),
        "native replay preserves provenance so provider codecs can gate on it"
    );
    let ChatOutputItem::Reasoning(reasoning) = &assistant_output.items[0] else {
        panic!("expected reasoning item");
    };
    assert_eq!(
        reasoning.encrypted_content.as_deref(),
        Some("encrypted-continuation-e2e"),
        "encrypted reasoning survives persist -> reload -> second request"
    );
    let ChatOutputItem::FunctionCall(call) = &assistant_output.items[2] else {
        panic!("expected function call item");
    };
    assert_eq!(
        call.arguments, raw_arguments,
        "raw arguments replay byte-exact on the second request"
    );
    assert_eq!(call.item_id.as_deref(), Some("fc_item_e2e"));
    // The portable projection is still present and consistent, so request
    // validation passes.
    assert_eq!(
        chat_messages[1].validate_output_consistency(),
        Ok(()),
        "projection stays consistent with the preserved output"
    );

    // The assistant turn projects its structured authority once (call/result
    // correlation preserved), without duplicating items.
    let calls = assistant_output.tool_calls().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "call projected exactly once into the assistant turn"
    );
    assert_eq!(calls[0].id, "call_e2e");
    assert_eq!(calls[0].function.name, "lookup");

    // Each structured item projects exactly once: visible reasoning and message
    // text become portable input text, while the call stays canonical output.
    let projected = chat_messages[1].portable_input_parts();
    let text_parts: Vec<&str> = projected.iter().filter_map(|part| part.as_text()).collect();
    assert_eq!(
        text_parts
            .iter()
            .filter(|text| **text == "running lookup")
            .count(),
        1,
        "message text projected exactly once"
    );
    assert_eq!(
        assistant_output
            .function_calls()
            .filter(|call| call.arguments == raw_arguments)
            .count(),
        1,
        "call arguments stay canonical exactly once"
    );

    let tool_result = chat_messages[2]
        .portable_input_parts()
        .into_iter()
        .find_map(|part| match part {
            querymt::chat::ChatInputPart::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result present");
    assert_eq!(tool_result.call_id, "call_e2e");

    // Same-origin replay retains the reasoning continuation, as one item.
    let reasoning_items = assistant_output
        .items
        .iter()
        .filter(|item| matches!(item, ChatOutputItem::Reasoning(_)))
        .count();
    assert_eq!(reasoning_items, 1, "reasoning projected once");

    // A different endpoint with the same provider/model must NOT receive native
    // state: only the portable projection is used.
    let cross_endpoint = history[1]
        .to_chat_message_with_output_target(
            Some(
                &crate::model::OutputTarget::new("openai".to_string(), "gpt-5".to_string())
                    .with_protocol("responses")
                    .with_endpoint("https://evil.example/v1/responses"),
            ),
            None,
        )
        .unwrap();
    let cross_endpoint_output = cross_endpoint
        .output()
        .expect("cross-endpoint target keeps the portable projection");
    assert!(
        !cross_endpoint_output.requires_item_aware_fidelity(),
        "cross-endpoint target must not replay native continuation state"
    );

    // A different protocol with the same provider/model/endpoint must also be
    // treated as a portable projection.
    let cross_protocol = history[1]
        .to_chat_message_with_output_target(
            Some(
                &crate::model::OutputTarget::new("openai".to_string(), "gpt-5".to_string())
                    .with_protocol("chat_completions")
                    .with_endpoint("https://api.openai.com/v1/responses"),
            ),
            None,
        )
        .unwrap();
    let cross_protocol_output = cross_protocol
        .output()
        .expect("cross-protocol target keeps the portable projection");
    assert!(
        !cross_protocol_output.requires_item_aware_fidelity(),
        "cross-protocol target must not replay native continuation state"
    );
}

/// Build the exact projection target identity matching [`origin_turn`]'s
/// recorded provenance (provider, protocol, model, endpoint).
fn origin_target(provider: &str, model: &str) -> crate::model::OutputTarget {
    crate::model::OutputTarget::new(provider.to_string(), model.to_string())
        .with_protocol("responses")
        .with_endpoint(format!("https://{provider}.invalid/v1/responses"))
}

/// Build a structured assistant turn for a specific origin (provider/model).
fn origin_turn(
    id: &str,
    session_id: &str,
    provider: &str,
    model: &str,
    call_id: &str,
) -> AgentMessage {
    use querymt::chat::{
        ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem,
        ChatOutputProvenance,
    };

    AgentMessage {
        id: id.to_string(),
        session_id: session_id.to_string(),
        role: ChatRole::Assistant,
        parts: vec![MessagePart::Output {
            output: ChatOutput {
                provenance: Some(ChatOutputProvenance {
                    provider: provider.to_string(),
                    protocol: "responses".to_string(),
                    model: model.to_string(),
                    endpoint: format!("https://{provider}.invalid/v1/responses"),
                }),
                items: vec![
                    ChatOutputItem::Reasoning(querymt::chat::ChatReasoningItem {
                        id: Some(format!("reasoning_{id}")),
                        summary: vec![querymt::chat::ChatReasoningPart::text(format!(
                            "{provider} visible summary"
                        ))],
                        content: Vec::new(),
                        encrypted_content: Some(format!("{provider}-encrypted-continuation")),
                        signature: Some(format!("{provider}-signature")),
                        status: None,
                        extensions: Default::default(),
                    }),
                    ChatOutputItem::Message(ChatMessageItem {
                        id: None,
                        role: ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![ChatMessagePart::Text {
                            text: format!("{provider} answer"),
                            annotations: Vec::new(),
                            extensions: Default::default(),
                        }],
                        extensions: Default::default(),
                    }),
                    ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                        item_id: Some(format!("item_{call_id}")),
                        call_id: call_id.to_string(),
                        name: "lookup".into(),
                        arguments: "{\"query\":\"rust\"}".into(),
                        status: None,
                        extensions: Default::default(),
                    }),
                ],
                ..ChatOutput::default()
            },
        }],
        created_at: 0,
        parent_message_id: None,
        source_provider: Some(provider.to_string()),
        source_model: Some(model.to_string()),
    }
}

/// A conversation that moves A -> B -> A keeps native A state in storage, never
/// forwards it to B, and projects B turn portably on return. Projections never
/// mutate the stored originals.
#[tokio::test]
async fn a_b_a_replay_scopes_opaque_state_to_its_origin() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let session = storage
        .create_session(Some("source".to_string()), None, None, None)
        .await
        .unwrap();

    let a1 = origin_turn("a1", &session.public_id, "provider-a", "model-a", "call_a1");
    let b1 = origin_turn("b1", &session.public_id, "provider-b", "model-b", "call_b1");
    let a2 = origin_turn("a2", &session.public_id, "provider-a", "model-a", "call_a2");
    for message in [&a1, &b1, &a2] {
        storage
            .add_message(&session.public_id, message.clone())
            .await
            .unwrap();
    }

    let history = storage.get_history(&session.public_id).await.unwrap();
    assert_eq!(history.len(), 3);

    // Snapshot the stored originals to prove projection does not mutate them.
    let stored_before: Vec<String> = history
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();

    // ── Send to B: A's opaque state must be excluded, B's own reasoning
    // signature (a same-origin provider-only signal) is retained. Full target
    // identity is required so endpoint/protocol are part of the decision.
    let to_b: Vec<_> = history
        .iter()
        .map(|m| {
            m.to_chat_message_with_output_target(
                Some(&origin_target("provider-b", "model-b")),
                None,
            )
            .unwrap()
        })
        .collect();
    let to_b_json = serde_json::to_string(&to_b).unwrap();
    assert!(
        !to_b_json.contains("provider-a-encrypted-continuation"),
        "A's encrypted continuation must never reach B"
    );
    assert!(
        !to_b_json.contains("provider-a-signature"),
        "A's reasoning signature must never reach B"
    );
    // A's visible/portable content still reaches B.
    assert!(to_b_json.contains("provider-a answer"));
    assert!(to_b_json.contains("provider-a visible summary"));
    // The portable projection keeps the call ID as the portable correlation key
    // between a call and its result, while provider-native item identity is
    // origin-scoped and never crosses to B.
    assert!(to_b_json.contains("rust"));
    assert!(
        to_b_json.contains("call_a1"),
        "portable call identity is retained for call/result correlation"
    );
    assert!(
        !to_b_json.contains("item_a1"),
        "native item identity is not portable to B"
    );
    // B's own turn keeps its native provider-only signature when the target is B.
    assert!(to_b_json.contains("provider-b-signature"));
    assert!(
        !to_b_json.contains("provider-a-encrypted-continuation"),
        "encrypted continuation is never portable content"
    );

    // ── Return to A: use validated native representations of A turns and
    // portable projections of B turns, in chronological order.
    let back_to_a: Vec<_> = history
        .iter()
        .map(|m| {
            m.to_chat_message_with_output_target(
                Some(&origin_target("provider-a", "model-a")),
                None,
            )
            .unwrap()
        })
        .collect();
    let back_json = serde_json::to_string(&back_to_a).unwrap();
    // Native A state is retained.
    assert!(back_json.contains("provider-a-signature"));
    // B is projected portably: visible content only, no B continuation state.
    assert!(back_json.contains("provider-b answer"));
    assert!(back_json.contains("provider-b visible summary"));
    assert!(
        !back_json.contains("provider-b-signature"),
        "B's provider-only signature must not be forwarded back to A"
    );
    // B's portable call identity survives for call/result correlation, but its
    // native item identity is not forwarded back to A.
    assert!(
        back_json.contains("call_b1"),
        "B's portable call identity survives projection back to A"
    );
    assert!(
        !back_json.contains("item_b1"),
        "B's native item identity is not forwarded back to A"
    );
    assert!(
        back_json.contains("call_a1"),
        "A's native call identity is retained for its own replay"
    );

    // Chronological order and call/result dependencies are preserved. Ordering
    // is checked via visible content so it does not depend on any provider's
    // native identifiers.
    let a1_pos = back_json.find("provider-a answer").unwrap();
    let b1_pos = back_json.find("provider-b answer").unwrap();
    let a2_pos = back_json.rfind("provider-a answer").unwrap();
    assert!(
        a1_pos < b1_pos && b1_pos < a2_pos,
        "chronological order holds"
    );

    // Stored originals are byte-identical and still hold the excluded
    // continuation state after projecting to other targets.
    let stored_after: Vec<String> = storage
        .get_history(&session.public_id)
        .await
        .unwrap()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(
        stored_before, stored_after,
        "projection never mutates storage"
    );
    assert!(
        stored_after
            .join("\n")
            .contains("provider-a-encrypted-continuation"),
        "authorized storage retains A continuation for native replay"
    );
}

/// A structured A dependency group that is compacted away must never be
/// resurrected from storage or a hidden sidecar when the conversation later
/// returns to A.
#[tokio::test]
async fn a_b_a_replay_does_not_resurrect_compacted_group() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let session = storage
        .create_session(Some("source".to_string()), None, None, None)
        .await
        .unwrap();

    let a1 = origin_turn("a1", &session.public_id, "provider-a", "model-a", "call_a1");
    let b1 = origin_turn("b1", &session.public_id, "provider-b", "model-b", "call_b1");
    storage
        .add_message(&session.public_id, a1.clone())
        .await
        .unwrap();
    storage
        .add_message(&session.public_id, b1.clone())
        .await
        .unwrap();

    // Edit frontier removes the A group entirely (as compaction/replacement does).
    storage
        .delete_messages_after(&session.public_id, "a1")
        .await
        .unwrap();

    let history = storage.get_history(&session.public_id).await.unwrap();
    assert!(history.is_empty(), "compacted A group is gone from history");

    // No hidden sidecar: the structured payload cannot be found anywhere.
    let conn = storage.conn_for_test();
    let conn = conn.lock().unwrap();
    let residue: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM message_parts WHERE content_json LIKE '%provider-a-encrypted-continuation%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(residue, 0, "no sidecar restores excluded A state");
    drop(conn);

    // A return to A must not surface the removed group.
    let back_to_a: Vec<String> = history
        .iter()
        .map(|m| {
            m.to_chat_message_with_target(Some("provider-a"), Some("model-a"), None)
                .unwrap()
        })
        .map(|m| serde_json::to_string(&m).unwrap())
        .collect();
    let back_json = back_to_a.join("\n");
    assert!(!back_json.contains("provider-a-encrypted-continuation"));
    assert!(!back_json.contains("call_a1"));
}

#[test]
fn migration_0001_is_recorded() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let version: String = conn
        .query_row(
            "SELECT version FROM schema_migrations ORDER BY version LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("query migration version");
    assert_eq!(version, "0001_initial_reset");
}

#[test]
fn migration_0002_drops_legacy_events_table() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let events_table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='events'",
            [],
            |row| row.get(0),
        )
        .expect("check events table");
    assert_eq!(
        events_table_count, 0,
        "legacy events table should be dropped"
    );

    // event_journal table should still exist
    let journal_table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='event_journal'",
            [],
            |row| row.get(0),
        )
        .expect("check event_journal table");
    assert_eq!(journal_table_count, 1, "event_journal table should exist");
}

#[test]
fn migration_0003_adds_message_source_columns() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let source_provider_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'source_provider'",
            [],
            |row| row.get(0),
        )
        .expect("check source_provider column");
    assert_eq!(
        source_provider_count, 1,
        "source_provider column should exist"
    );

    let source_model_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'source_model'",
            [],
            |row| row.get(0),
        )
        .expect("check source_model column");
    assert_eq!(source_model_count, 1, "source_model column should exist");
}

#[test]
fn migration_0004_adds_session_kind_column() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let mut stmt = conn
        .prepare("PRAGMA table_info(sessions)")
        .expect("table info query");
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("load column names")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect columns");

    assert!(columns.into_iter().any(|name| name == "session_kind"));
}

#[test]
fn migration_0012_adds_task_and_intent_revision_columns() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = '0012_task_and_intent_revisions'",
            [],
            |row| row.get(0),
        )
        .expect("query migration");
    assert_eq!(recorded, 1);

    let task_columns: Vec<String> = conn
        .prepare("PRAGMA table_info(tasks)")
        .expect("task columns")
        .query_map([], |row| row.get(1))
        .expect("query task columns")
        .collect::<Result<_, _>>()
        .expect("collect task columns");
    for expected in [
        "revision",
        "creation_key",
        "completion_evidence",
        "completed_at",
    ] {
        assert!(task_columns.iter().any(|column| column == expected));
    }

    let intent_columns: Vec<String> = conn
        .prepare("PRAGMA table_info(intent_snapshots)")
        .expect("intent columns")
        .query_map([], |row| row.get(1))
        .expect("query intent columns")
        .collect::<Result<_, _>>()
        .expect("collect intent columns");
    for expected in ["revision", "source", "source_ref"] {
        assert!(intent_columns.iter().any(|column| column == expected));
    }
}

#[test]
fn migration_0018_separates_protocol_task_ownership_and_approvals() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = '0018_dotagents_task_state'",
            [],
            |row| row.get(0),
        )
        .expect("query migration");
    assert_eq!(recorded, 1);

    conn.execute(
        "INSERT INTO dotagents_task_ownership (
            source_key, task_creation_key, schedule_creation_key, layer,
            canonical_workspace, task_id, fingerprint, created_at, updated_at
         ) VALUES (?1, ?2, ?3, 'workspace', '/workspace', 'nightly', 'fp-1', ?4, ?4)",
        rusqlite::params![
            "source-1",
            "task-key-1",
            "schedule-key-1",
            "2026-01-01T00:00:00Z"
        ],
    )
    .expect("insert protocol ownership");
    conn.execute(
        "INSERT INTO dotagents_task_approvals (
            canonical_workspace, task_id, fingerprint, decision, decided_at
         ) VALUES ('/workspace', 'nightly', 'fp-1', 'approve', '2026-01-01T00:00:00Z')",
        [],
    )
    .expect("insert fingerprint-bound approval");

    let owned: i64 = conn
        .query_row("SELECT COUNT(*) FROM dotagents_task_ownership", [], |row| {
            row.get(0)
        })
        .expect("count ownership rows");
    let approvals: i64 = conn
        .query_row("SELECT COUNT(*) FROM dotagents_task_approvals", [], |row| {
            row.get(0)
        })
        .expect("count approval rows");
    assert_eq!(owned, 1);
    assert_eq!(approvals, 1);

    conn.execute(
        "INSERT INTO sessions (public_id, created_at, updated_at)
         VALUES ('user-session', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        [],
    )
    .expect("insert user session");
    let session_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO tasks (
            public_id, session_id, kind, status, created_at, updated_at
         ) VALUES ('user-task', ?1, 'recurring', 'active', ?2, ?2)",
        rusqlite::params![session_id, "2026-01-01T00:00:00Z"],
    )
    .expect("insert user task");
    let task_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO schedules (
            public_id, task_id, task_public_id, session_id, session_public_id,
            trigger_json, state, config_json, created_at, updated_at
         ) VALUES ('user-schedule', ?1, 'user-task', ?2, 'user-session',
                   '{\"type\":\"interval\",\"seconds\":60}', 'armed', '{}', ?3, ?3)",
        rusqlite::params![task_id, session_id, "2026-01-01T00:00:00Z"],
    )
    .expect("insert user schedule");

    // Existing schedules have no protocol ownership unless reconciliation links them.
    let unowned_schedules: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schedules s
             LEFT JOIN dotagents_task_ownership o ON o.schedule_public_id = s.public_id
             WHERE o.source_key IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count user-created schedules");
    assert_eq!(unowned_schedules, 1);
}

#[test]
fn migration_0014_adds_session_control_tables() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    for table in ["session_control_states", "session_mode_model_bindings"] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("query control table");
        assert_eq!(count, 1, "missing {table}");
    }
}

#[tokio::test]
async fn delegate_assignments_are_read_only_revisioned_and_session_scoped() {
    use crate::delegation::DelegateModelOverride;
    use crate::session::error::SessionError;
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let first = storage
        .create_session(None, None, None, None)
        .await
        .unwrap();
    let second = storage
        .create_session(None, None, None, None)
        .await
        .unwrap();
    let original = storage
        .get_delegate_assignments(&first.public_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.revision, 0);
    assert!(original.overrides.is_empty());
    assert!(original.reasoning_overrides.is_empty());
    let rows = storage
        .conn_for_test()
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM session_delegate_assignments",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "read must not create assignment state");
    let model = DelegateModelOverride {
        model_id: "provider/model".into(),
        node_id: Some("mesh-node".into()),
    };
    let one = storage
        .set_delegate_assignment(&first.public_id, "coder", Some(model.clone()), Some(0))
        .await
        .unwrap();
    assert!(one.changed);
    assert_eq!(one.assignments.revision, 1);
    assert_eq!(one.assignments.overrides["coder"], model);
    let reasoning = storage
        .set_delegate_assignment_with_reasoning(
            &first.public_id,
            "coder",
            Some(model.clone()),
            Some(Some(crate::delegation::DelegateReasoningEffort::High)),
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(reasoning.assignments.revision, 2);
    assert_eq!(
        reasoning.assignments.reasoning_overrides["coder"],
        crate::delegation::DelegateReasoningEffort::High
    );
    let noop = storage
        .set_delegate_assignment(&first.public_id, "coder", Some(model.clone()), Some(2))
        .await
        .unwrap();
    assert!(!noop.changed);
    assert_eq!(noop.assignments, reasoning.assignments);
    let stale = storage
        .set_delegate_assignment(&first.public_id, "coder", None, Some(0))
        .await
        .unwrap_err();
    assert!(matches!(
        stale,
        SessionError::DelegateAssignmentRevisionConflict {
            expected: 0,
            found: 2
        }
    ));
    let two = storage
        .set_delegate_assignment(&first.public_id, "reviewer", Some(model.clone()), None)
        .await
        .unwrap();
    assert_eq!(two.assignments.revision, 3);
    let cleared = storage
        .set_delegate_assignment_with_reasoning(
            &first.public_id,
            "coder",
            None,
            Some(None),
            Some(3),
        )
        .await
        .unwrap();
    assert_eq!(cleared.assignments.revision, 4);
    assert!(!cleared.assignments.overrides.contains_key("coder"));
    assert!(
        !cleared
            .assignments
            .reasoning_overrides
            .contains_key("coder")
    );
    let cleared_rows = storage
        .conn_for_test()
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM session_delegate_assignment_overrides o
             JOIN sessions s ON s.id = o.session_id
             WHERE s.public_id = ?1 AND o.agent_id = 'coder'",
            [&first.public_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(cleared_rows, 0, "empty role overrides must delete the row");
    assert_eq!(cleared.assignments.overrides["reviewer"], model);
    assert!(
        storage
            .get_delegate_assignments(&second.public_id)
            .await
            .unwrap()
            .unwrap()
            .overrides
            .is_empty()
    );
    assert!(matches!(
        storage.get_delegate_assignments("missing").await,
        Err(SessionError::SessionNotFound(_))
    ));
    assert!(
        storage
            .set_delegate_assignment("missing", "coder", None, None)
            .await
            .is_err()
    );
    assert!(
        storage
            .set_delegate_assignment(&first.public_id, "", None, None)
            .await
            .is_err()
    );
    assert!(
        storage
            .set_delegate_assignment(&first.public_id, " coder ", Some(model.clone()), None)
            .await
            .is_err()
    );
    assert!(
        storage
            .set_delegate_assignment(
                &first.public_id,
                "coder",
                Some(DelegateModelOverride {
                    model_id: " provider/model ".into(),
                    node_id: None
                }),
                None
            )
            .await
            .is_err()
    );
    assert!(
        storage
            .set_delegate_assignment(
                &first.public_id,
                "coder",
                Some(DelegateModelOverride {
                    model_id: "provider/model".into(),
                    node_id: Some(" ".into())
                }),
                None
            )
            .await
            .is_err()
    );
    storage.delete_session(&first.public_id).await.unwrap();
    let count = storage
        .conn_for_test()
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM session_delegate_assignments",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(count, 0, "deleting a parent cascades to assignments");
}

#[tokio::test]
async fn delegate_assignments_survive_reopen_and_detect_cross_connection_conflicts() {
    use crate::delegation::DelegateModelOverride;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.db");
    let storage = SqliteStorage::connect(path.clone()).await.unwrap();
    let parent = storage
        .create_session(None, None, None, None)
        .await
        .unwrap();
    let model = DelegateModelOverride {
        model_id: "provider/retired-model".into(),
        node_id: Some("offline-node".into()),
    };
    storage
        .set_delegate_assignment(&parent.public_id, "coder", Some(model.clone()), Some(0))
        .await
        .unwrap();
    drop(storage);
    let first = SqliteStorage::connect(path.clone()).await.unwrap();
    let second = SqliteStorage::connect(path).await.unwrap();
    assert_eq!(
        first
            .get_delegate_assignments(&parent.public_id)
            .await
            .unwrap()
            .unwrap()
            .overrides["coder"],
        model
    );
    let noop = second
        .set_delegate_assignment(&parent.public_id, "coder", Some(model.clone()), None)
        .await
        .unwrap();
    assert!(!noop.changed, "cross-connection no-op must be explicit");
    assert_eq!(noop.assignments.revision, 1);
    let (a, b) = tokio::join!(
        first.set_delegate_assignment(&parent.public_id, "a", Some(model.clone()), Some(1)),
        second.set_delegate_assignment(&parent.public_id, "b", Some(model), Some(1)),
    );
    assert_ne!(
        a.is_ok(),
        b.is_ok(),
        "exactly one writer can consume revision 1"
    );
    let error = a.err().or_else(|| b.err()).unwrap();
    assert!(matches!(
        error,
        crate::session::error::SessionError::DelegateAssignmentRevisionConflict {
            expected: 1,
            found: 2
        }
    ));
    assert_eq!(
        first
            .get_delegate_assignments(&parent.public_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        2
    );
}

#[tokio::test]
async fn delegate_assignments_rollback_failed_writes_and_do_not_hide_corruption() {
    use crate::delegation::DelegateModelOverride;
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let parent = storage
        .create_session(None, None, None, None)
        .await
        .unwrap();
    let model = DelegateModelOverride {
        model_id: "provider/model".into(),
        node_id: None,
    };
    let before = storage
        .set_delegate_assignment(&parent.public_id, "coder", Some(model.clone()), None)
        .await
        .unwrap();
    storage
        .run_blocking(|conn| {
            conn.execute_batch(
        "CREATE TRIGGER reject_delegate_update BEFORE UPDATE ON session_delegate_assignments
         BEGIN SELECT RAISE(ABORT, 'rejected update'); END;"
    )
        })
        .await
        .unwrap();
    assert!(
        storage
            .set_delegate_assignment(&parent.public_id, "reviewer", Some(model), Some(1))
            .await
            .is_err()
    );
    assert_eq!(
        storage
            .get_delegate_assignments(&parent.public_id)
            .await
            .unwrap()
            .unwrap(),
        before.assignments
    );
    storage
        .run_blocking(|conn| {
            conn.execute_batch(
                "DROP TRIGGER reject_delegate_update;
         PRAGMA ignore_check_constraints = ON;
         INSERT INTO session_delegate_assignment_overrides
           (session_id, agent_id, model_id, provider_node_id, reasoning_effort)
         SELECT id, 'broken', NULL, NULL, NULL
         FROM sessions WHERE public_id = (SELECT public_id FROM sessions LIMIT 1);
         PRAGMA ignore_check_constraints = OFF;",
            )
        })
        .await
        .unwrap();
    assert!(
        storage
            .get_delegate_assignments(&parent.public_id)
            .await
            .is_err(),
        "empty corrupt rows must not be displayed as inheritance"
    );
    storage
        .run_blocking(|conn| {
            conn.execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE session_delegate_assignment_overrides
                 SET reasoning_effort = 'invalid' WHERE agent_id = 'broken';
                 PRAGMA ignore_check_constraints = OFF;",
            )
        })
        .await
        .unwrap();
    assert!(
        storage
            .get_delegate_assignments(&parent.public_id)
            .await
            .is_err(),
        "invalid reasoning values must not be displayed as inheritance"
    );
}

#[tokio::test]
async fn delegate_assignments_user_forks_inherit_but_delegate_children_do_not() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let parent = storage
        .create_session(None, None, None, None)
        .await
        .unwrap();
    let model = crate::delegation::DelegateModelOverride {
        model_id: "provider/model".into(),
        node_id: None,
    };
    storage
        .set_delegate_assignment_with_reasoning(
            &parent.public_id,
            "coder",
            Some(model.clone()),
            Some(Some(crate::delegation::DelegateReasoningEffort::Low)),
            None,
        )
        .await
        .unwrap();
    storage
        .add_message(
            &parent.public_id,
            AgentMessage {
                id: "fork-point".into(),
                session_id: parent.public_id.clone(),
                role: ChatRole::User,
                parts: vec![MessagePart::Prompt {
                    blocks: vec![ContentBlock::Text(TextContent::new("task"))],
                }],
                created_at: 1,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        )
        .await
        .unwrap();
    let fork = storage
        .fork_session(&parent.public_id, "fork-point", ForkOrigin::User)
        .await
        .unwrap();
    let child = storage
        .fork_session(&parent.public_id, "fork-point", ForkOrigin::Delegation)
        .await
        .unwrap();
    assert_eq!(
        storage
            .get_delegate_assignments(&fork)
            .await
            .unwrap()
            .unwrap()
            .overrides["coder"],
        model
    );
    assert_eq!(
        storage
            .get_delegate_assignments(&fork)
            .await
            .unwrap()
            .unwrap()
            .reasoning_overrides["coder"],
        crate::delegation::DelegateReasoningEffort::Low
    );
    assert!(
        storage
            .get_delegate_assignments(&child)
            .await
            .unwrap()
            .unwrap()
            .overrides
            .is_empty()
    );
    storage
        .set_delegate_assignment(&fork, "coder", None, Some(0))
        .await
        .unwrap();
    assert_eq!(
        storage
            .get_delegate_assignments(&parent.public_id)
            .await
            .unwrap()
            .unwrap()
            .overrides["coder"],
        model
    );
}

#[test]
fn delegate_assignment_migration_creates_normalized_schema_and_preserves_sessions() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    // Model an upgrade from main's prior schema rather than only testing fresh databases.
    for migration in MIGRATIONS
        .iter()
        .filter(|m| m.version != "0017_delegate_assignments")
    {
        (migration.apply)(&mut conn).unwrap();
    }
    conn.execute("INSERT INTO sessions (public_id, name, created_at, updated_at) VALUES ('existing', 'Keep me', '2026-09-07T00:00:00Z', '2026-09-07T00:00:00Z')", []).unwrap();
    let migration = MIGRATIONS
        .iter()
        .find(|m| m.version == "0017_delegate_assignments")
        .unwrap();
    (migration.apply)(&mut conn).unwrap();
    (migration.apply)(&mut conn).unwrap();

    for (table, expected_columns) in [
        (
            "session_delegate_assignments",
            vec!["session_id", "revision"],
        ),
        (
            "session_delegate_assignment_overrides",
            vec![
                "session_id",
                "agent_id",
                "model_id",
                "provider_node_id",
                "reasoning_effort",
            ],
        ),
    ] {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(columns, expected_columns, "unexpected schema for {table}");
    }

    let name: String = conn
        .query_row(
            "SELECT name FROM sessions WHERE public_id = 'existing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(name, "Keep me");
    conn.execute(
        "INSERT INTO session_delegate_assignments (session_id, revision)
         SELECT id, 0 FROM sessions WHERE public_id = 'existing'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_delegate_assignment_overrides
         (session_id, agent_id, model_id, provider_node_id, reasoning_effort)
         SELECT id, 'reasoner', NULL, NULL, 'high' FROM sessions WHERE public_id = 'existing'",
        [],
    )
    .unwrap();
    let reasoning_only: (Option<String>, String) = conn
        .query_row(
            "SELECT model_id, reasoning_effort FROM session_delegate_assignment_overrides
             WHERE agent_id = 'reasoner'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(reasoning_only, (None, "high".into()));
    assert!(
        conn.execute(
            "INSERT INTO session_delegate_assignment_overrides
             (session_id, agent_id, model_id, provider_node_id, reasoning_effort)
             SELECT id, 'invalid', NULL, NULL, 'extreme' FROM sessions WHERE public_id = 'existing'",
            [],
        )
        .is_err(),
        "invalid reasoning values must fail the schema constraint"
    );
    assert!(
        conn.execute(
            "INSERT INTO session_delegate_assignment_overrides
             (session_id, agent_id, model_id, provider_node_id, reasoning_effort)
             SELECT id, 'empty', NULL, NULL, NULL FROM sessions WHERE public_id = 'existing'",
            [],
        )
        .is_err(),
        "empty role rows must fail the schema constraint"
    );

    conn.execute("DELETE FROM sessions WHERE public_id = 'existing'", [])
        .unwrap();
    for table in [
        "session_delegate_assignments",
        "session_delegate_assignment_overrides",
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 0,
            "deleting the session must cascade through {table}"
        );
    }
}

#[tokio::test]
async fn session_control_commit_is_revisioned_and_session_scoped() {
    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let first = storage
        .create_session(None, None, None, None)
        .await
        .expect("first session");
    let second = storage
        .create_session(None, None, None, None)
        .await
        .expect("second session");
    let config = storage
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("model-a"))
        .await
        .expect("LLM config");
    for session in [&first, &second] {
        storage
            .set_session_llm_config(&session.public_id, config.id)
            .await
            .expect("bind LLM config");
    }
    let binding = SessionModelBinding {
        model_id: "mock/model-a".to_string(),
        provider: "mock".to_string(),
        model: "model-a".to_string(),
        llm_config_id: config.id,
        provider_node_id: None,
    };
    let state = SessionControlState {
        revision: 1,
        active_mode: AgentMode::Build,
        reasoning_effort: None,
        effective_model: binding.clone(),
        mode_models: HashMap::from([
            ("build".to_string(), binding.clone()),
            ("plan".to_string(), binding),
        ]),
    };
    storage
        .commit_session_control(&first.public_id, 0, &state)
        .await
        .expect("commit first state");
    assert!(
        storage
            .get_session_control(&second.public_id)
            .await
            .unwrap()
            .is_none()
    );

    let stale = storage
        .commit_session_control(&first.public_id, 0, &state)
        .await
        .expect_err("stale revision must fail");
    assert!(matches!(
        stale,
        crate::session::error::SessionError::SessionControlRevisionConflict {
            expected: 0,
            found: 1
        }
    ));
}

#[tokio::test]
async fn set_session_llm_config_rejects_changes_after_control_initialization() {
    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let session = storage
        .create_session(None, None, None, None)
        .await
        .expect("session");
    let config_a = storage
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("model-a"))
        .await
        .expect("config A");
    let config_b = storage
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("model-b"))
        .await
        .expect("config B");
    storage
        .set_session_llm_config(&session.public_id, config_a.id)
        .await
        .expect("initialize config");
    let binding = SessionModelBinding {
        model_id: "mock/model-a".to_string(),
        provider: "mock".to_string(),
        model: "model-a".to_string(),
        llm_config_id: config_a.id,
        provider_node_id: None,
    };
    storage
        .commit_session_control(
            &session.public_id,
            0,
            &SessionControlState {
                revision: 1,
                active_mode: AgentMode::Build,
                reasoning_effort: None,
                effective_model: binding.clone(),
                mode_models: HashMap::from([("build".to_string(), binding)]),
            },
        )
        .await
        .expect("initialize control state");

    let error = storage
        .set_session_llm_config(&session.public_id, config_b.id)
        .await
        .expect_err("direct config changes must be rejected after control initialization");
    assert!(error.to_string().contains("session control"));
    assert_eq!(
        storage
            .get_session(&session.public_id)
            .await
            .unwrap()
            .unwrap()
            .llm_config_id,
        Some(config_a.id)
    );
}

#[tokio::test]
async fn fork_copies_session_control_bindings_independently() {
    use crate::model::{AgentMessage, MessagePart};
    use querymt::chat::ChatRole;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let source = storage
        .create_session(None, None, None, None)
        .await
        .expect("source session");
    let config = storage
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("source-model"))
        .await
        .expect("source config");
    storage
        .set_session_llm_config(&source.public_id, config.id)
        .await
        .expect("bind source config");
    let mut message = AgentMessage::new(source.public_id.clone(), ChatRole::User);
    message.parts.push(MessagePart::Text {
        content: "fork here".to_string(),
    });
    let message_id = message.id.clone();
    storage
        .add_message(&source.public_id, message)
        .await
        .expect("source message");

    let binding = SessionModelBinding {
        model_id: "mock/source-model".to_string(),
        provider: "mock".to_string(),
        model: "source-model".to_string(),
        llm_config_id: config.id,
        provider_node_id: None,
    };
    let source_state = SessionControlState {
        revision: 1,
        active_mode: AgentMode::Plan,
        reasoning_effort: None,
        effective_model: binding.clone(),
        mode_models: HashMap::from([("plan".to_string(), binding)]),
    };
    storage
        .commit_session_control(&source.public_id, 0, &source_state)
        .await
        .expect("source control state");

    let fork_id = storage
        .fork_session(&source.public_id, &message_id, ForkOrigin::User)
        .await
        .expect("fork session");
    let fork_state = storage
        .get_session_control(&fork_id)
        .await
        .expect("load fork control")
        .expect("fork control exists");
    assert_eq!(fork_state, source_state);

    let mut changed_fork = fork_state;
    changed_fork.revision += 1;
    changed_fork.active_mode = AgentMode::Build;
    storage
        .commit_session_control(&fork_id, 1, &changed_fork)
        .await
        .expect("change fork control");
    assert_eq!(
        storage
            .get_session_control(&source.public_id)
            .await
            .unwrap()
            .unwrap()
            .active_mode,
        AgentMode::Plan
    );
}

#[test]
fn migrations_are_idempotent() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("first migration run");
    let count_after_first: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("count migration rows");

    apply_migrations(&mut conn).expect("second migration run");
    let count_after_second: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("count migration rows");

    assert_eq!(count_after_first, MIGRATIONS.len() as i64);
    assert_eq!(count_after_first, count_after_second);
}

#[tokio::test]
async fn task_lifecycle_is_atomic_revisioned_and_explicit() {
    use crate::session::domain::{TaskKind, TaskStatus};
    use crate::session::store::TaskPatch;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let session = storage
        .create_session(None, None, None, None)
        .await
        .expect("create session");

    let created = storage
        .create_and_bind_current_task(
            &session.public_id,
            TaskKind::Finite,
            "deliver the fix".to_string(),
            Some("tests pass".to_string()),
            "call-1",
        )
        .await
        .expect("create task");
    assert_eq!(created.revision, 1);
    assert_eq!(
        storage
            .get_current_task(&session.public_id)
            .await
            .expect("read current")
            .expect("current task")
            .public_id,
        created.public_id
    );

    let duplicate = storage
        .create_and_bind_current_task(
            &session.public_id,
            TaskKind::Finite,
            "ignored duplicate".to_string(),
            None,
            "call-1",
        )
        .await
        .expect("idempotent create");
    assert_eq!(duplicate.public_id, created.public_id);

    let updated = storage
        .patch_task_for_session(
            &session.public_id,
            &created.public_id,
            1,
            TaskPatch {
                acceptance_criteria: Some("all relevant tests pass".to_string()),
                ..TaskPatch::default()
            },
            "clarify criteria",
        )
        .await
        .expect("update task");
    assert_eq!(updated.revision, 2);

    let unchanged = storage
        .patch_task_for_session(
            &session.public_id,
            &created.public_id,
            2,
            TaskPatch {
                expected_deliverable: updated.expected_deliverable.clone(),
                acceptance_criteria: updated.acceptance_criteria.clone(),
                status: Some(updated.status),
                ..TaskPatch::default()
            },
            "repeat current state",
        )
        .await
        .expect("no-op update");
    assert_eq!(unchanged.revision, 2);
    assert_eq!(unchanged.updated_at, updated.updated_at);

    assert!(
        storage
            .patch_task_for_session(
                &session.public_id,
                &created.public_id,
                1,
                TaskPatch::default(),
                "stale update",
            )
            .await
            .is_err()
    );

    let completed = storage
        .complete_task_for_session(
            &session.public_id,
            &created.public_id,
            2,
            "cargo test passed",
        )
        .await
        .expect("complete task");
    assert_eq!(completed.status, TaskStatus::Done);
    assert_eq!(
        completed.completion_evidence.as_deref(),
        Some("cargo test passed")
    );
    assert!(completed.completed_at.is_some());
    assert!(
        storage
            .get_current_task(&session.public_id)
            .await
            .expect("read cleared current")
            .is_none()
    );
}

#[tokio::test]
async fn intent_projection_insert_and_binding_are_atomic() {
    use crate::session::domain::IntentSnapshot;
    use time::OffsetDateTime;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let session = storage
        .create_session(None, None, None, None)
        .await
        .expect("create session");
    let snapshot = IntentSnapshot {
        id: 0,
        session_id: session.id,
        task_id: None,
        summary: "implement the fix".to_string(),
        constraints: None,
        next_step_hint: None,
        revision: 1,
        source: "user_prompt".to_string(),
        source_ref: Some("message-1".to_string()),
        created_at: OffsetDateTime::now_utc(),
    };

    let stored = storage
        .create_and_set_current_intent_snapshot(&session.public_id, snapshot)
        .await
        .expect("persist intent projection");
    assert!(stored.id > 0);
    assert_eq!(
        storage
            .get_session(&session.public_id)
            .await
            .expect("read session")
            .expect("session")
            .current_intent_snapshot_id,
        Some(stored.id)
    );

    let missing = IntentSnapshot {
        id: 0,
        session_id: session.id,
        task_id: None,
        summary: "must roll back".to_string(),
        constraints: None,
        next_step_hint: None,
        revision: 2,
        source: "user_prompt".to_string(),
        source_ref: Some("message-2".to_string()),
        created_at: OffsetDateTime::now_utc(),
    };
    assert!(
        storage
            .create_and_set_current_intent_snapshot("missing-session", missing)
            .await
            .is_err()
    );
    assert_eq!(
        storage
            .list_intent_snapshots(&session.public_id)
            .await
            .expect("list snapshots")
            .len(),
        1
    );
}

#[tokio::test]
async fn intent_projection_rolls_back_when_fts_refresh_fails() {
    use crate::session::domain::IntentSnapshot;
    use time::OffsetDateTime;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let session = storage
        .create_session(None, None, None, None)
        .await
        .expect("create session");
    storage
        .run_blocking(|conn| {
            conn.execute_batch("DROP TABLE sessions_fts;")?;
            Ok(())
        })
        .await
        .expect("remove FTS table");
    let snapshot = IntentSnapshot {
        id: 0,
        session_id: session.id,
        task_id: None,
        summary: "must roll back".to_string(),
        constraints: None,
        next_step_hint: None,
        revision: 1,
        source: "user_prompt".to_string(),
        source_ref: Some("message-1".to_string()),
        created_at: OffsetDateTime::now_utc(),
    };

    assert!(
        storage
            .create_and_set_current_intent_snapshot(&session.public_id, snapshot)
            .await
            .is_err()
    );
    assert_eq!(
        storage
            .list_intent_snapshots(&session.public_id)
            .await
            .expect("list snapshots")
            .len(),
        0
    );
    assert_eq!(
        storage
            .get_session(&session.public_id)
            .await
            .expect("read session")
            .expect("session")
            .current_intent_snapshot_id,
        None
    );
}

#[tokio::test]
async fn connect_with_options_without_migration_keeps_db_unmodified() {
    let tmp = tempfile::NamedTempFile::new().expect("temp db file");
    let path = tmp.path().to_path_buf();

    let _storage = SqliteStorage::connect_with_options(path.clone(), false)
        .await
        .expect("connect without migrations");

    let conn = Connection::open(path).expect("reopen db");
    let has_migration_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |row| row.get(0),
        )
        .expect("check migration table existence");
    assert_eq!(has_migration_table, 0);
}

#[tokio::test]
async fn session_runtime_binding_round_trips_profile_and_delegate() {
    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");

    storage
        .set_session_runtime_binding(
            "delegate-session",
            "quorum",
            Some("coder"),
            Some("profile-sha"),
            Some("lock-sha"),
            Some("{}"),
        )
        .await
        .expect("persist runtime binding");

    let binding = storage
        .get_session_runtime_binding("delegate-session")
        .await
        .expect("load runtime binding")
        .expect("runtime binding");
    assert_eq!(binding.profile_id, "quorum");
    assert_eq!(binding.agent_id.as_deref(), Some("coder"));
    assert_eq!(
        storage
            .get_profile_binding("delegate-session")
            .await
            .expect("load compatibility profile binding")
            .as_deref(),
        Some("quorum")
    );

    assert_eq!(binding.profile_fingerprint.as_deref(), Some("profile-sha"));
    assert_eq!(binding.provider_lock_digest.as_deref(), Some("lock-sha"));
    assert_eq!(binding.provider_locks_json.as_deref(), Some("{}"));
}

#[tokio::test]
async fn session_runtime_binding_transaction_rolls_back_on_failure() {
    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    storage
        .set_session_runtime_binding(
            "session-1",
            "old-profile",
            None,
            Some("old-fingerprint"),
            Some("old-lock"),
            Some("{}"),
        )
        .await
        .expect("seed binding");
    storage
        .run_blocking(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER reject_provider_lock_update
                 BEFORE UPDATE ON profile_bindings
                 WHEN NEW.provider_locks_json = 'reject'
                 BEGIN
                    SELECT RAISE(ABORT, 'rejected provider lock');
                 END;",
            )
        })
        .await
        .expect("install failure trigger");

    storage
        .set_session_runtime_binding(
            "session-1",
            "new-profile",
            Some("coder"),
            Some("new-fingerprint"),
            Some("new-lock"),
            Some("reject"),
        )
        .await
        .expect_err("transaction must fail");

    let binding = storage
        .get_session_runtime_binding("session-1")
        .await
        .expect("load binding")
        .expect("original binding remains");
    assert_eq!(binding.profile_id, "old-profile");
    assert_eq!(binding.agent_id, None);
    assert_eq!(
        binding.profile_fingerprint.as_deref(),
        Some("old-fingerprint")
    );
    assert_eq!(binding.provider_lock_digest.as_deref(), Some("old-lock"));
    assert_eq!(binding.provider_locks_json.as_deref(), Some("{}"));
}

#[tokio::test]
async fn custom_model_crud_round_trip() {
    use crate::session::store::CustomModel;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");

    let base = CustomModel {
        provider: "llama_cpp".to_string(),
        model_id: "hf:foo/bar:model.gguf".to_string(),
        display_name: "Model A".to_string(),
        config_json: serde_json::json!({"model": "hf:foo/bar:model.gguf"}),
        source_type: "hf".to_string(),
        source_ref: Some("foo/bar:model.gguf".to_string()),
        family: Some("Foo-Model".to_string()),
        quant: Some("Q8_0".to_string()),
        created_at: None,
        updated_at: None,
    };

    storage
        .upsert_custom_model(&base)
        .await
        .expect("insert custom model");

    let fetched = storage
        .get_custom_model("llama_cpp", "hf:foo/bar:model.gguf")
        .await
        .expect("get custom model")
        .expect("custom model exists");
    assert_eq!(fetched.display_name, "Model A");
    assert_eq!(fetched.source_type, "hf");

    let mut updated = fetched.clone();
    updated.display_name = "Model A Updated".to_string();
    storage
        .upsert_custom_model(&updated)
        .await
        .expect("update custom model");

    let listed = storage
        .list_custom_models("llama_cpp")
        .await
        .expect("list custom models");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].display_name, "Model A Updated");

    storage
        .delete_custom_model("llama_cpp", "hf:foo/bar:model.gguf")
        .await
        .expect("delete custom model");

    let after_delete = storage
        .get_custom_model("llama_cpp", "hf:foo/bar:model.gguf")
        .await
        .expect("get custom model after delete");
    assert!(after_delete.is_none());
}

// ══════════════════════════════════════════════════════════════════════
// EventJournal tests
// ══════════════════════════════════════════════════════════════════════

fn new_durable(session_id: &str, kind: AgentEventKind) -> NewDurableEvent {
    NewDurableEvent {
        session_id: session_id.to_string(),
        origin: EventOrigin::Local,
        source_node: None,
        source_node_id: None,
        source_seq: None,
        kind,
    }
}

// ── Remote source identity (plan §15/§16) ─────────────────────────────────

#[tokio::test]
async fn remote_sync_checkpoint_survives_restart_and_live_events_cannot_advance_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sync.db");
    {
        let storage = SqliteStorage::connect(path.clone()).await.unwrap();
        storage
            .advance_remote_sync_cursor("s1", "node-a", 10, false)
            .await
            .unwrap();
        let mut live = new_durable("s1", AgentEventKind::Cancelled);
        live.origin = EventOrigin::Remote;
        live.source_node_id = Some("node-a".into());
        live.source_seq = Some(21);
        storage.append_durable_from_source(&live).await.unwrap();
        assert_eq!(
            storage.latest_source_seq("s1", "node-a").await.unwrap(),
            Some(21)
        );
    }
    let storage = SqliteStorage::connect(path).await.unwrap();
    assert_eq!(
        storage.remote_sync_cursor("s1", "node-a").await.unwrap(),
        Some(10)
    );
    assert_eq!(
        storage.remote_sync_cursor("s1", "node-b").await.unwrap(),
        None
    );
    storage
        .advance_remote_sync_cursor("s1", "node-a", 5, false)
        .await
        .unwrap();
    assert_eq!(
        storage.remote_sync_cursor("s1", "node-a").await.unwrap(),
        Some(10)
    );
}

#[tokio::test]
async fn remote_snapshot_orders_host_events_and_preserves_legacy_storage() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    storage
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    for (seq, kind) in [
        (3, AgentEventKind::Cancelled),
        (1, AgentEventKind::SessionCreated),
    ] {
        let mut event = new_durable("s1", kind);
        event.origin = EventOrigin::Remote;
        event.source_node_id = Some("node-a".into());
        event.source_seq = Some(seq);
        storage.append_durable_from_source(&event).await.unwrap();
    }
    assert_eq!(
        storage
            .load_remote_session_stream("s1", "node-a")
            .await
            .unwrap()
            .len(),
        3
    );
    storage
        .advance_remote_sync_cursor("s1", "node-a", 3, true)
        .await
        .unwrap();
    let snapshot = storage
        .load_remote_session_stream("s1", "node-a")
        .await
        .unwrap();
    assert_eq!(snapshot.len(), 2);
    assert!(matches!(snapshot[0].kind, AgentEventKind::SessionCreated));
    assert!(matches!(snapshot[1].kind, AgentEventKind::Cancelled));
    assert!(
        snapshot[0].stream_seq > snapshot[1].stream_seq,
        "retain local sequence identity, not source sequence"
    );
    assert_eq!(
        storage
            .load_session_stream("s1", None, None)
            .await
            .unwrap()
            .len(),
        3,
        "legacy rows are not deleted"
    );
}

#[tokio::test]
async fn journal_remote_source_insert_is_idempotent() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let mut event = new_durable("s1", AgentEventKind::SessionCreated);
    event.origin = EventOrigin::Remote;
    event.source_node = Some("peer-a".to_string());
    event.source_node_id = Some("node-a".to_string());
    event.source_seq = Some(7);

    let first = journal
        .append_durable_from_source(&event)
        .await
        .unwrap()
        .expect("first insert persists and returns the event");
    assert_eq!(first.stream_seq, 1);
    assert!(matches!(first.origin, EventOrigin::Remote));

    // Duplicate replay (same session/node/source_seq) is a no-op: no second
    // row and no stream_seq allocation (plan §15 "duplicate replay becomes a
    // no-op").
    let duplicate = journal.append_durable_from_source(&event).await.unwrap();
    assert!(
        duplicate.is_none(),
        "duplicate source identity must be ignored"
    );
    assert_eq!(journal.max_stream_seq("s1").await.unwrap(), 1);

    let stream = journal.load_session_stream("s1", None, None).await.unwrap();
    assert_eq!(stream.len(), 1);
    assert_eq!(stream[0].source_node.as_deref(), Some("peer-a"));

    // A different source sequence is a different event.
    let mut next = event.clone();
    next.kind = AgentEventKind::Cancelled;
    next.source_seq = Some(8);
    assert!(
        journal
            .append_durable_from_source(&next)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(journal.max_stream_seq("s1").await.unwrap(), 2);
}

#[tokio::test]
async fn journal_source_cursor_is_per_session_and_node() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    for (session, node, seq) in [
        ("s1", "node-a", 5),
        ("s1", "node-b", 3),
        ("s2", "node-a", 9),
    ] {
        let mut event = new_durable(session, AgentEventKind::SessionCreated);
        event.source_node_id = Some(node.to_string());
        event.source_seq = Some(seq);
        journal
            .append_durable_from_source(&event)
            .await
            .unwrap()
            .expect("identity insert persists");
    }

    // Cursors are scoped to (session_id, source_node_id).
    assert_eq!(
        journal.latest_source_seq("s1", "node-a").await.unwrap(),
        Some(5)
    );
    assert_eq!(
        journal.latest_source_seq("s1", "node-b").await.unwrap(),
        Some(3)
    );
    assert_eq!(
        journal.latest_source_seq("s2", "node-a").await.unwrap(),
        Some(9)
    );
    assert_eq!(
        journal.latest_source_seq("s2", "node-b").await.unwrap(),
        None
    );
    assert_eq!(
        journal.latest_source_seq("s3", "node-a").await.unwrap(),
        None
    );

    // The maximum source_seq wins over insertion order.
    let mut event = new_durable("s1", AgentEventKind::Cancelled);
    event.source_node_id = Some("node-a".to_string());
    event.source_seq = Some(11);
    journal
        .append_durable_from_source(&event)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        journal.latest_source_seq("s1", "node-a").await.unwrap(),
        Some(11)
    );

    // Stream tips reflect global stream_seq allocation (s1 owns rows 1, 2
    // and 4; s2 owns row 3).
    assert_eq!(journal.max_stream_seq("s1").await.unwrap(), 4);
    assert_eq!(journal.max_stream_seq("s2").await.unwrap(), 3);
    assert_eq!(journal.max_stream_seq("s4").await.unwrap(), 0);
}

#[tokio::test]
async fn journal_legacy_rows_without_source_cursor_remain_readable() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    // Legacy/pre-migration shape: no source identity at all.
    journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();

    // The identity-bearing API refuses identity-less events rather than
    // silently persisting undeduplicatable rows.
    assert!(
        journal
            .append_durable_from_source(&new_durable("s1", AgentEventKind::Cancelled))
            .await
            .is_err(),
        "append_durable_from_source requires source identity"
    );

    // No cursor can be derived from legacy rows: callers must treat the next
    // attachment as a new synchronization boundary (plan §16), never guess.
    assert_eq!(
        journal.latest_source_seq("s1", "node-a").await.unwrap(),
        None
    );

    // Legacy rows remain fully readable in the session stream.
    let stream = journal.load_session_stream("s1", None, None).await.unwrap();
    assert_eq!(stream.len(), 1);
    assert!(matches!(stream[0].origin, EventOrigin::Local));
    assert_eq!(stream[0].source_node, None);
}

#[tokio::test]
async fn journal_append_durable_assigns_monotonic_seq() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let e1 = journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    let e2 = journal
        .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
        .await
        .unwrap();

    assert!(
        e2.stream_seq > e1.stream_seq,
        "seq must be monotonically increasing"
    );
    assert_ne!(e1.event_id, e2.event_id, "event_ids must be unique");
}

#[tokio::test]
async fn journal_append_durable_returns_correct_fields() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let evt = journal
        .append_durable(&NewDurableEvent {
            session_id: "sess-x".to_string(),
            origin: EventOrigin::Remote,
            source_node: Some("node-a".to_string()),
            source_node_id: None,
            source_seq: None,
            kind: AgentEventKind::Cancelled,
        })
        .await
        .unwrap();

    assert_eq!(evt.session_id, "sess-x");
    assert!(matches!(evt.origin, EventOrigin::Remote));
    assert_eq!(evt.source_node.as_deref(), Some("node-a"));
    assert!(matches!(evt.kind, AgentEventKind::Cancelled));
    assert!(evt.stream_seq >= 1);
    assert!(!evt.event_id.is_empty());
    assert!(evt.timestamp > 0);
}

#[tokio::test]
async fn journal_load_session_stream_returns_only_matching_session() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    journal
        .append_durable(&new_durable("s2", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    journal
        .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
        .await
        .unwrap();

    let s1_events = journal.load_session_stream("s1", None, None).await.unwrap();
    assert_eq!(s1_events.len(), 2);
    assert!(s1_events.iter().all(|e| e.session_id == "s1"));

    let s2_events = journal.load_session_stream("s2", None, None).await.unwrap();
    assert_eq!(s2_events.len(), 1);
}

#[tokio::test]
async fn journal_load_session_stream_respects_after_seq_cursor() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let e1 = journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    let _e2 = journal
        .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
        .await
        .unwrap();
    let _e3 = journal
        .append_durable(&new_durable(
            "s1",
            AgentEventKind::Error {
                message: "x".into(),
            },
        ))
        .await
        .unwrap();

    let after_first = journal
        .load_session_stream("s1", Some(e1.stream_seq), None)
        .await
        .unwrap();
    assert_eq!(after_first.len(), 2);
    assert!(after_first[0].stream_seq > e1.stream_seq);
}

#[tokio::test]
async fn journal_load_session_stream_respects_limit() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    for _ in 0..5 {
        journal
            .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
            .await
            .unwrap();
    }

    let limited = journal
        .load_session_stream("s1", None, Some(2))
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
}

#[tokio::test]
async fn journal_load_global_stream_returns_all_sessions() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    journal
        .append_durable(&new_durable("s2", AgentEventKind::SessionCreated))
        .await
        .unwrap();

    let global = journal.load_global_stream(None, None).await.unwrap();
    assert_eq!(global.len(), 2);
}

#[tokio::test]
async fn journal_load_global_stream_respects_cursor() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let e1 = journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    journal
        .append_durable(&new_durable("s2", AgentEventKind::SessionCreated))
        .await
        .unwrap();

    let after = journal
        .load_global_stream(Some(e1.stream_seq), None)
        .await
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].session_id, "s2");
}

#[tokio::test]
async fn journal_durable_event_never_replayed_for_ephemeral_kind() {
    // Verify that classify_durability correctly identifies ephemeral events;
    // the EventSink will use this to route. The journal itself doesn't filter.
    assert_eq!(
        crate::events::classify_durability(&AgentEventKind::AssistantContentDelta {
            content: "x".into(),
            message_id: "m".into(),
        }),
        crate::events::Durability::Ephemeral
    );
}

#[tokio::test]
async fn journal_empty_session_returns_empty_vec() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let events = journal
        .load_session_stream("nonexistent", None, None)
        .await
        .unwrap();
    assert!(events.is_empty());
}

#[tokio::test]
async fn journal_ordering_is_monotonic_per_stream() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    for _ in 0..10 {
        journal
            .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
            .await
            .unwrap();
    }

    let events = journal.load_session_stream("s1", None, None).await.unwrap();
    for window in events.windows(2) {
        assert!(
            window[1].stream_seq > window[0].stream_seq,
            "stream_seq must be strictly increasing"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════
// ViewStore — scoped session browsing tests
// ══════════════════════════════════════════════════════════════════════

#[cfg(unix)]
#[tokio::test]
async fn migration_merges_legacy_noncanonical_workspace_groups() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("sessions.db");
    let storage = SqliteStorage::connect(db_path.clone()).await.unwrap();
    let plain = storage
        .create_session(None, Some("/workspace".into()), None, None)
        .await
        .unwrap();
    let legacy = storage
        .create_session(None, Some("/workspace".into()), None, None)
        .await
        .unwrap();
    let legacy_dot = storage
        .create_session(None, Some("/workspace".into()), None, None)
        .await
        .unwrap();
    let root = storage
        .create_session(None, Some("/".into()), None, None)
        .await
        .unwrap();
    let relative = storage
        .create_session(None, Some("/workspace".into()), None, None)
        .await
        .unwrap();
    {
        let conn = storage.conn_for_test();
        let conn = conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET cwd = '/workspace//' WHERE public_id = ?1",
            [&legacy.public_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions SET cwd = '/workspace/./' WHERE public_id = ?1",
            [&legacy_dot.public_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions SET cwd = '//' WHERE public_id = ?1",
            [&root.public_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions SET cwd = 'relative/workspace/' WHERE public_id = ?1",
            [&relative.public_id],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM schema_migrations WHERE version = '0021_normalize_session_cwd_slashes'",
            [],
        )
        .unwrap();
    }
    drop(storage);

    let storage = SqliteStorage::connect(db_path).await.unwrap();
    let view: &dyn ViewStore = &storage;
    let (groups, next, total) = view
        .browse_session_groups(None, 10, 10, SessionScope::Root)
        .await
        .unwrap();
    assert_eq!(total, 5);
    assert!(next.is_none());
    assert_eq!(groups.len(), 3);
    let merged = groups
        .iter()
        .find(|group| group.cwd.as_deref() == Some("/workspace"))
        .unwrap();
    assert_eq!(merged.total_count, Some(3));
    assert_eq!(merged.sessions.len(), 3);
    assert!(groups.iter().any(|group| group.cwd.as_deref() == Some("/")));
    assert!(
        groups
            .iter()
            .any(|group| group.cwd.as_deref() == Some("relative/workspace/"))
    );
    let (page, count) = view
        .list_group_sessions(Some("/workspace".into()), None, 2, SessionScope::Root)
        .await
        .unwrap();
    assert_eq!(count, 3);
    assert_eq!(page.sessions.len(), 2);
    assert!(page.next_cursor.is_some());
    let (last_page, _) = view
        .list_group_sessions(
            Some("/workspace".into()),
            page.next_cursor,
            2,
            SessionScope::Root,
        )
        .await
        .unwrap();
    assert_eq!(last_page.sessions.len(), 1);
    assert!(last_page.next_cursor.is_none());
    assert_eq!(
        storage
            .get_session(&legacy.public_id)
            .await
            .unwrap()
            .unwrap()
            .cwd,
        Some("/workspace".into())
    );
    assert_eq!(
        storage
            .get_session(&plain.public_id)
            .await
            .unwrap()
            .unwrap()
            .cwd,
        Some("/workspace".into())
    );
    assert_eq!(
        storage
            .get_session(&legacy_dot.public_id)
            .await
            .unwrap()
            .unwrap()
            .cwd,
        Some("/workspace".into())
    );
}

async fn seed_scoped_sessions(storage: &SqliteStorage) -> (String, String, String, String) {
    let root_a = storage
        .create_session(
            Some("root-alpha".to_string()),
            Some("/workspace".into()),
            None,
            None,
        )
        .await
        .unwrap();
    let root_b = storage
        .create_session(
            Some("root-beta".to_string()),
            Some("/workspace".into()),
            None,
            None,
        )
        .await
        .unwrap();
    let user_fork = storage
        .create_session(
            Some("user-fork".to_string()),
            Some("/workspace".into()),
            Some(root_a.public_id.clone()),
            Some(ForkOrigin::User),
        )
        .await
        .unwrap();
    let delegate = storage
        .create_session(
            Some("delegate-child".to_string()),
            Some("/workspace".into()),
            Some(root_a.public_id.clone()),
            Some(ForkOrigin::Delegation),
        )
        .await
        .unwrap();

    (
        root_a.public_id,
        root_b.public_id,
        user_fork.public_id,
        delegate.public_id,
    )
}

fn session_ids(groups: &[crate::session::projection::SessionGroup]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|group| {
            group
                .sessions
                .iter()
                .map(|session| session.session_id.clone())
        })
        .collect()
}

#[tokio::test]
async fn browse_session_groups_filters_by_scope_and_counts_after_filtering() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let (root_a, root_b, user_fork, delegate) = seed_scoped_sessions(&storage).await;
    let view: &dyn ViewStore = &storage;

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(total, 4);
    assert_eq!(session_ids(&groups).len(), 4);

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::Root)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(session_ids(&groups), vec![root_b.clone(), root_a.clone()]);
    assert_eq!(groups[0].total_count, Some(2));

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::Forks)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(session_ids(&groups), vec![user_fork.clone()]);

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::Delegates)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(session_ids(&groups), vec![delegate.clone()]);

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::Children)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(session_ids(&groups), vec![delegate, user_fork]);
}

#[tokio::test]
async fn browse_session_groups_marks_only_user_forks_as_children() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let root_with_user_fork = storage
        .create_session(
            Some("root-with-user-fork".to_string()),
            Some("/workspace".into()),
            None,
            None,
        )
        .await
        .unwrap();
    storage
        .create_session(
            Some("user-fork".to_string()),
            Some("/workspace".into()),
            Some(root_with_user_fork.public_id.clone()),
            Some(ForkOrigin::User),
        )
        .await
        .unwrap();
    let root_with_delegate_only = storage
        .create_session(
            Some("root-with-delegate-only".to_string()),
            Some("/workspace".into()),
            None,
            None,
        )
        .await
        .unwrap();
    storage
        .create_session(
            Some("delegate-child".to_string()),
            Some("/workspace".into()),
            Some(root_with_delegate_only.public_id.clone()),
            Some(ForkOrigin::Delegation),
        )
        .await
        .unwrap();
    let view: &dyn ViewStore = &storage;

    let (groups, _, _) = view
        .browse_session_groups(None, 20, 20, SessionScope::Root)
        .await
        .unwrap();
    let sessions: Vec<_> = groups
        .iter()
        .flat_map(|group| group.sessions.iter())
        .collect();

    let root_with_user_fork_item = sessions
        .iter()
        .find(|session| session.session_id == root_with_user_fork.public_id)
        .unwrap();
    assert!(root_with_user_fork_item.has_children);
    assert_eq!(root_with_user_fork_item.fork_count, 1);

    let root_with_delegate_only_item = sessions
        .iter()
        .find(|session| session.session_id == root_with_delegate_only.public_id)
        .unwrap();
    assert!(!root_with_delegate_only_item.has_children);
    assert_eq!(root_with_delegate_only_item.fork_count, 0);
}

#[tokio::test]
async fn list_session_children_returns_user_forks_and_excludes_delegates() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let (root_a, _, user_fork, delegate) = seed_scoped_sessions(&storage).await;
    let view: &dyn ViewStore = &storage;

    let (group, total) = view.list_session_children(root_a, None, 20).await.unwrap();

    assert_eq!(total, 1);
    assert_eq!(group.total_count, Some(1));
    assert_eq!(group.sessions[0].fork_count, 0);
    assert_eq!(session_ids(&[group]), vec![user_fork.clone()]);
    assert_ne!(delegate, user_fork);
}

#[tokio::test]
async fn list_session_children_returns_empty_group_for_missing_parent() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let view: &dyn ViewStore = &storage;

    let (group, total) = view
        .list_session_children("missing-parent".to_string(), None, 20)
        .await
        .unwrap();

    assert_eq!(total, 0);
    assert_eq!(group.total_count, Some(0));
    assert!(group.sessions.is_empty());
    assert!(group.next_cursor.is_none());
}

#[tokio::test]
async fn group_sessions_scope_filtering_respects_cursors_and_counts() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let (root_a, root_b, _, _) = seed_scoped_sessions(&storage).await;
    let view: &dyn ViewStore = &storage;

    let (group, total) = view
        .list_group_sessions(Some("/workspace".to_string()), None, 1, SessionScope::Root)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(group.total_count, Some(2));
    assert_eq!(group.sessions.len(), 1);
    assert_eq!(group.sessions[0].session_id, root_b);
    assert_eq!(group.next_cursor.as_deref(), Some("1"));

    let (group, total) = view
        .list_group_sessions(
            Some("/workspace".to_string()),
            group.next_cursor,
            1,
            SessionScope::Root,
        )
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(group.sessions[0].session_id, root_a);
    assert!(group.next_cursor.is_none());
}

#[tokio::test]
async fn search_sessions_filters_by_scope() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    seed_scoped_sessions(&storage).await;
    let view: &dyn ViewStore = &storage;

    let (groups, _, total) = view
        .search_sessions("root".to_string(), None, 20, SessionScope::Root)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert!(
        groups
            .iter()
            .flat_map(|group| group.sessions.iter())
            .all(|session| session.parent_session_id.is_none())
    );

    let (groups, _, total) = view
        .search_sessions("delegate".to_string(), None, 20, SessionScope::Delegates)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(
        groups[0].sessions[0].fork_origin.as_deref(),
        Some("delegation")
    );
}

// ══════════════════════════════════════════════════════════════════════
// ViewStore — get_recent_models_view tests
// ══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn recent_models_view_reads_from_event_journal() {
    // This test verifies that get_recent_models_view reads from
    // event_journal (not the dropped legacy `events` table).
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();

    // Create a session so we can join on sessions.public_id
    let session = storage
        .create_session(
            None,
            Some(std::path::PathBuf::from("/home/user/project")),
            None,
            None,
        )
        .await
        .unwrap();
    let session_id = session.public_id;

    // Insert a ProviderChanged event into event_journal
    let journal: &dyn EventJournal = &storage;
    journal
        .append_durable(&NewDurableEvent {
            session_id: session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            source_node_id: None,
            source_seq: None,
            kind: AgentEventKind::ProviderChanged {
                provider: "anthropic".to_string(),
                model: "claude-3-opus".to_string(),
                config_id: 1,
                context_limit: Some(200_000),
                provider_node_id: None,
            },
        })
        .await
        .unwrap();

    // Query recent models — should find the one we just inserted
    let view: &dyn ViewStore = &storage;
    let result = view.get_recent_models_view(10).await.unwrap();

    // Flatten all workspace entries
    let all_entries: Vec<&RecentModelEntry> = result.by_workspace.values().flatten().collect();
    assert_eq!(
        all_entries.len(),
        1,
        "expected 1 recent model entry, got {}",
        all_entries.len()
    );
    assert_eq!(all_entries[0].provider, "anthropic");
    assert_eq!(all_entries[0].model, "claude-3-opus");
    assert_eq!(all_entries[0].use_count, 1);
}

#[tokio::test]
async fn recent_models_view_returns_empty_when_no_provider_changed_events() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();

    let view: &dyn ViewStore = &storage;
    let result = view.get_recent_models_view(10).await.unwrap();
    assert!(
        result.by_workspace.is_empty(),
        "expected empty recent models on fresh db"
    );
}

#[tokio::test]
async fn recent_models_view_respects_limit_per_workspace() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();

    let session = storage
        .create_session(
            None,
            Some(std::path::PathBuf::from("/workspace")),
            None,
            None,
        )
        .await
        .unwrap();
    let session_id = session.public_id;

    let journal: &dyn EventJournal = &storage;
    for (provider, model) in &[
        ("anthropic", "model-a"),
        ("openai", "model-b"),
        ("cohere", "model-c"),
    ] {
        journal
            .append_durable(&NewDurableEvent {
                session_id: session_id.clone(),
                origin: EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                kind: AgentEventKind::ProviderChanged {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    config_id: 1,
                    context_limit: None,
                    provider_node_id: None,
                },
            })
            .await
            .unwrap();
    }

    let view: &dyn ViewStore = &storage;
    let result = view.get_recent_models_view(2).await.unwrap();

    // Each workspace should have at most 2 entries
    for entries in result.by_workspace.values() {
        assert!(
            entries.len() <= 2,
            "expected at most 2 entries per workspace, got {}",
            entries.len()
        );
    }
}

#[tokio::test]
async fn journal_preserves_remote_origin_and_source_node() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    journal
        .append_durable(&NewDurableEvent {
            session_id: "s1".to_string(),
            origin: EventOrigin::Remote,
            source_node: Some("peer-42".to_string()),
            source_node_id: None,
            source_seq: None,
            kind: AgentEventKind::SessionCreated,
        })
        .await
        .unwrap();

    let events = journal.load_session_stream("s1", None, None).await.unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].origin, EventOrigin::Remote));
    assert_eq!(events[0].source_node.as_deref(), Some("peer-42"));
}

#[tokio::test]
async fn remote_bookmark_point_lookup_round_trip() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let bookmark = RemoteSessionBookmark {
        session_id: "s-rem".to_string(),
        node_id: "node-1".to_string(),
        peer_label: "remote-host".to_string(),
        cwd: Some("/remote/dir".to_string()),
        created_at: 42,
        title: Some("My remote session".to_string()),
    };
    storage
        .save_remote_session_bookmark(&bookmark)
        .await
        .unwrap();

    let got = storage
        .get_remote_session_bookmark("s-rem")
        .await
        .unwrap()
        .expect("bookmark must round-trip through the store");
    assert_eq!(got, bookmark);
}

#[tokio::test]
async fn remote_bookmark_point_lookup_missing_returns_none() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let got = storage.get_remote_session_bookmark("absent").await.unwrap();
    assert_eq!(got, None);
}
