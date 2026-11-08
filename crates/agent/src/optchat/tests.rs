use super::source::sources;
use super::*;
use crate::model::{AgentMessage, MessagePart};
use querymt::chat::{ChatMessage, ChatOutputStatus, ChatRole};

#[test]
fn config_is_disabled_by_default_and_rejects_invalid_enabled_settings() {
    let settings: MemorySettings = toml::from_str("").unwrap();
    assert!(!settings.enabled);
    let invalid = MemorySettings {
        enabled: true,
        scope: "global".into(),
        ..Default::default()
    };
    assert!(invalid.validate().is_err());
    let valid = MemorySettings {
        enabled: true,
        ..Default::default()
    };
    valid.validate().unwrap();
    let config: crate::config::SingleAgentConfig=toml::from_str("[agent]\nprovider='test'\nmodel='test'\n[agent.memory]\nenabled=true\nstrategy='optchat'\nscope='session'\n").unwrap();
    assert!(config.agent.memory.enabled);
    assert!(toml::from_str::<MemorySettings>("enabled=true\nunknown_setting=3").is_err());
    let invalid = MemorySettings {
        enabled: true,
        directory: Some("".into()),
        ..Default::default()
    };
    assert!(invalid.validate().is_err());
    let compactor = CompactorSettings {
        provider: "provider".into(),
        model: "model".into(),
        parameters: serde_json::from_value(serde_json::json!({"api_key":"SECRET_CONFIG"})).unwrap(),
    };
    let settings = MemorySettings {
        compactor: Some(compactor),
        ..Default::default()
    };
    assert!(!format!("{settings:?}").contains("SECRET_CONFIG"));
}

#[test]
fn extraction_excludes_reasoning_and_preserves_user_text() {
    let mut message = AgentMessage::new("session".into(), ChatRole::Assistant);
    let output = ChatMessage::assistant()
        .thinking("private thinking")
        .text("public answer")
        .build()
        .output()
        .unwrap()
        .clone();
    message.parts.push(MessagePart::Output { output });
    message.parts.push(MessagePart::Text {
        content: "stale duplicate".into(),
    });
    let input = sources(&[message]).unwrap();
    assert_eq!(input[0].entries.len(), 1);
    assert_eq!(input[0].entries[0].text, "public answer");
    let mut long = AgentMessage::new("session".into(), ChatRole::User);
    let text = "界".repeat(25_000);
    long.parts.push(MessagePart::Text {
        content: text.clone(),
    });
    let input = sources(&[long]).unwrap();
    assert_eq!(
        input[0]
            .entries
            .iter()
            .map(|e| e.text.as_str())
            .collect::<String>(),
        text
    );
    assert!(input[0].entries.iter().all(|e| e.text.len() <= 30_000));
}

#[test]
fn extraction_keeps_refusals_media_and_prompt_block_boundaries() {
    use querymt::chat::{ChatMessagePart, ChatOutputItem};
    let mut message = AgentMessage::new("s".into(), ChatRole::Assistant);
    let mut output = ChatMessage::assistant()
        .text("visible text")
        .build()
        .output()
        .unwrap()
        .clone();
    let ChatOutputItem::Message(item) = &mut output.items[0] else {
        panic!("expected message");
    };
    item.parts.push(ChatMessagePart::Refusal {
        refusal: "cannot perform that action".into(),
        extensions: Default::default(),
    });
    let media = querymt::chat::MediaPart::new(
        querymt::chat::MediaKind::Image,
        Some("image/png".parse().unwrap()),
        querymt::chat::MediaSource::Url {
            url: "https://example.com/image.png".into(),
        },
    )
    .unwrap();
    item.parts.push(ChatMessagePart::Media(Box::new(media)));
    message.parts.push(MessagePart::Output { output });
    let input = sources(&[message]).unwrap();
    assert!(
        input[0].entries[0]
            .text
            .contains("cannot perform that action")
    );
    assert_eq!(input[0].entries[0].attachments.len(), 1);
    let mut prompt = AgentMessage::new("s".into(), ChatRole::User);
    prompt.parts.push(MessagePart::Prompt { blocks: serde_json::from_value(serde_json::json!([{"type":"text","text":"first block"},{"type":"text","text":"second block"}])).unwrap() });
    assert_eq!(
        sources(&[prompt]).unwrap()[0].entries[0].text,
        "first block\nsecond block"
    );
}

#[test]
fn incomplete_empty_and_tool_call_outputs_are_not_summaries() {
    let good = ChatMessage::assistant()
        .text("summary")
        .build()
        .output()
        .unwrap()
        .clone();
    assert_eq!(summary_text(&good).unwrap(), "summary");
    let mut failed = good.clone();
    failed.status = Some(ChatOutputStatus::Incomplete);
    assert!(summary_text(&failed).is_err());
    let mut failed = good;
    failed.finish_reason = Some(FinishReason::Length);
    assert!(summary_text(&failed).is_err());
    let tool = ChatMessage::assistant()
        .tool_use("id", "shell", serde_json::json!({}))
        .build()
        .output()
        .unwrap()
        .clone();
    assert!(summary_text(&tool).is_err());
    assert!(summary_text(&ChatOutput::default()).is_err());
}

#[test]
fn cache_blocks_keep_completed_prefix_payloads_stable() {
    let view: Vec<_> = (0..8)
        .map(|i| (store::Key { start: i, n: 1 }, format!("user: {i}")))
        .collect();
    let old = runtime::render_view(&view[..5]);
    let new = runtime::render_view(&view);
    // Cache marks move, but the already-completed block payload must not.
    for (previous, current) in old[..2].iter().zip(&new[..2]) {
        assert_eq!(previous.input(), current.input());
        assert_eq!(previous.role, current.role);
    }
    assert!(old[1].cache.is_some());
}

#[test]
fn original_paging_respects_utf8_and_bad_offsets_fail() {
    let entry = source::Entry {
        kind: "user".into(),
        text: "界".repeat(5000),
        date: 0,
        attachments: vec![],
    };
    assert!(runtime::original_page(entry.clone(), 1).is_err());
    let result = runtime::original_page(entry, 0).unwrap();
    assert!(result[0].as_text().unwrap().contains("next offset=12000"));
}

#[tokio::test]
async fn disabled_install_is_a_strict_noop_and_creates_no_files() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let directory = fixture._tempdir.path().join("must-not-exist");
    let mut config = (*fixture.config).clone();
    let registry = config.tool_registry.clone();
    let pruning = config.execution_policy.pruning.enabled;
    let compaction = config.execution_policy.compaction.auto;
    install(
        &mut config,
        MemorySettings {
            directory: Some(directory.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(std::sync::Arc::ptr_eq(&registry, &config.tool_registry));
    assert_eq!(pruning, config.execution_policy.pruning.enabled);
    assert_eq!(compaction, config.execution_policy.compaction.auto);
    assert!(!directory.exists());
    assert!(config.tool_registry.find("zoom").is_none());
}

#[tokio::test]
async fn full_agent_turns_use_memory_and_zoom_preserves_tool_pairing() {
    use crate::api::{AgentBuilder, AgentInfra};
    use crate::session::backend::StorageBackend;
    use crate::test_utils::{
        MockChatResponse, MockLlmProvider, SharedLlmProvider, TestProviderFactory,
        mock_plugin_registry,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    let directory = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let summaries = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let turn = count.clone();
    let compressed = summaries.clone();
    let mut mock = MockLlmProvider::new();
    mock.expect_chat_with_tools()
        .returning(move |messages, tools| {
            if tools.is_some_and(|t| t.is_empty()) {
                compressed.fetch_add(1, Ordering::SeqCst);
                return Ok(MockChatResponse::text_only("user: remembered first decision").into());
            }
            captured.lock().unwrap().push(messages.to_vec());
            let index = turn.fetch_add(1, Ordering::SeqCst);
            Ok(match index {
                0 => MockChatResponse::text_only("first answer"),
                1 => MockChatResponse::with_tools(
                    "Opening the original.",
                    vec![querymt::ToolCall {
                        id: "memory-zoom-call".into(),
                        call_type: "function".into(),
                        function: querymt::FunctionCall {
                            name: "zoom".into(),
                            arguments: "{\"id\":0,\"n\":1}".into(),
                        },
                    }],
                ),
                _ => MockChatResponse::text_only("retrieved the original decision"),
            }
            .into())
        });
    let provider = SharedLlmProvider::new(mock, vec![]);
    let (registry, _provider_dir) =
        mock_plugin_registry(Arc::new(TestProviderFactory::new(provider))).unwrap();
    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(
            directory.path().join("sessions.db"),
        )
        .await
        .unwrap(),
    );
    let agent = AgentBuilder::new()
        .provider("mock", "mock-model")
        .tools(["read_tool"])
        .skills(crate::config::SkillsConfig {
            enabled: false,
            ..Default::default()
        })
        .memory(MemorySettings {
            enabled: true,
            directory: Some(directory.path().join("memory")),
            ..Default::default()
        })
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage.clone()),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .unwrap();
    let session = agent.chat_session().await.unwrap();
    let first = format!("FIRST_DECISION: {}", "preserve original text ".repeat(40));
    // Inspect canonical stored outputs: the existing simple-API string helper
    // still only reads legacy Text parts, independently of this experiment.
    session.chat(&first).await.unwrap();
    let first_history = storage
        .session_store()
        .get_history(session.id())
        .await
        .unwrap();
    assert!(first_history.iter().any(|m|m.parts.iter().any(|p|matches!(p,MessagePart::Output { output } if output.text().as_deref()==Some("first answer")))));
    session.chat("Retrieve my earlier decision.").await.unwrap();
    let second_history = storage
        .session_store()
        .get_history(session.id())
        .await
        .unwrap();
    assert!(second_history.iter().any(|m|m.parts.iter().any(|p|matches!(p,MessagePart::Output { output } if output.text().as_deref()==Some("retrieved the original decision")))));
    assert!(summaries.load(Ordering::SeqCst) > 0);
    let captured = calls.lock().unwrap().clone();
    assert_eq!(captured.len(), 3);
    let second = &captured[1];
    assert_eq!(
        second
            .iter()
            .filter(|m| m.role == ChatRole::Assistant)
            .count(),
        0,
        "old assistant history must not be replayed verbatim"
    );
    assert!(second.iter().any(|m| m.input().is_some_and(|p| {
        p.iter()
            .any(|p| p.as_text().is_some_and(|t| t.contains("<chat>")))
    })));
    assert!(!second.iter().any(|m| {
        m.input()
            .is_some_and(|p| p.iter().any(|p| p.as_text() == Some(first.as_str())))
    }));
    let third = &captured[2];
    let result = third
        .iter()
        .flat_map(|m| m.input().into_iter().flatten())
        .find_map(|p| match p {
            querymt::chat::ChatInputPart::ToolResult(r) if r.call_id == "memory-zoom-call" => {
                Some(r)
            }
            _ => None,
        })
        .unwrap();
    assert!(result.text_content().contains(&first));
    let history = storage
        .session_store()
        .get_history(session.id())
        .await
        .unwrap();
    assert!(history.iter().all(|m| !m.parts.iter().any(|p| matches!(
        p,
        MessagePart::Compaction { .. } | MessagePart::CompactionRequest { .. }
    ))));
    assert!(directory.path().join("memory").is_dir());
    assert!(second.iter().any(|m| m.input().is_some_and(|parts| {
        parts
            .iter()
            .any(|p| p.as_text().is_some_and(|t| t.contains("date(id)")))
    })));
}
