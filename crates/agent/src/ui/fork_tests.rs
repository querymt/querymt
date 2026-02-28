//! Integration tests for UI session fork handler.

use crate::events::{AgentEventKind, EventOrigin};
use crate::model::{AgentMessage, MessagePart};
use crate::session::backend::StorageBackend;
use crate::session::projection::NewDurableEvent;
use crate::ui::handlers::handle_ui_message;
use crate::ui::messages::UiClientMessage;
use anyhow::Result;
use querymt::chat::ChatRole;
use serde_json::Value;
use std::path::PathBuf;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

async fn next_json(rx: &mut tokio::sync::mpsc::Receiver<String>) -> Option<Value> {
    let msg = timeout(Duration::from_millis(400), rx.recv())
        .await
        .ok()??;
    serde_json::from_str(&msg).ok()
}

async fn wait_for_fork_result(rx: &mut tokio::sync::mpsc::Receiver<String>) -> Value {
    for _ in 0..8 {
        if let Some(msg) = next_json(rx).await
            && msg["type"] == "fork_result"
        {
            return msg;
        }
    }
    panic!("expected fork_result message");
}

#[tokio::test]
async fn fork_session_from_selected_message_succeeds() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let source_cwd = PathBuf::from("/tmp/fork-workspace-test");
    let source_session_id = f
        .agent
        .storage
        .session_store()
        .create_session(
            Some("Fork source".to_string()),
            Some(source_cwd.clone()),
            None,
            None,
        )
        .await?
        .public_id;

    let same_second = time::OffsetDateTime::now_utc().unix_timestamp();
    let user_message_id = Uuid::new_v4().to_string();
    f.agent
        .storage
        .session_store()
        .add_message(
            &source_session_id,
            AgentMessage {
                id: user_message_id.clone(),
                session_id: source_session_id.clone(),
                role: ChatRole::User,
                parts: vec![MessagePart::Text {
                    content: "Fork from here".to_string(),
                }],
                created_at: same_second,
                parent_message_id: None,
            },
        )
        .await?;

    let assistant_message_id = Uuid::new_v4().to_string();
    f.agent
        .storage
        .session_store()
        .add_message(
            &source_session_id,
            AgentMessage {
                id: assistant_message_id.clone(),
                session_id: source_session_id.clone(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Text {
                    content: "Assistant kept in fork".to_string(),
                }],
                created_at: same_second,
                parent_message_id: Some(user_message_id.clone()),
            },
        )
        .await?;

    let journal = f.agent.storage.event_journal();
    journal
        .append_durable(&NewDurableEvent {
            session_id: source_session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::PromptReceived {
                content: "Fork from here".to_string(),
                message_id: Some(user_message_id.clone()),
            },
        })
        .await?;
    journal
        .append_durable(&NewDurableEvent {
            session_id: source_session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::ToolCallStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: "read_tool".to_string(),
                arguments: "{}".to_string(),
            },
        })
        .await?;
    journal
        .append_durable(&NewDurableEvent {
            session_id: source_session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantMessageStored {
                content: "Assistant kept in fork".to_string(),
                thinking: None,
                message_id: Some(assistant_message_id.clone()),
            },
        })
        .await?;

    let (tx, mut rx) = f.add_connection("conn-fork-ok").await;
    {
        let mut connections = f.state.connections.lock().await;
        let conn = connections
            .get_mut("conn-fork-ok")
            .expect("connection should exist");
        conn.sessions
            .insert("primary".to_string(), source_session_id.clone());
    }

    handle_ui_message(
        &f.state,
        "conn-fork-ok",
        &tx,
        UiClientMessage::ForkSession {
            message_id: assistant_message_id.clone(),
        },
    )
    .await;

    let fork_result = wait_for_fork_result(&mut rx).await;
    assert_eq!(fork_result["success"], true);
    assert_eq!(fork_result["source_session_id"], source_session_id);

    let forked_session_id = fork_result["forked_session_id"]
        .as_str()
        .expect("forked session id should exist")
        .to_string();
    assert_ne!(forked_session_id, source_session_id);

    let forked_session = f
        .agent
        .storage
        .session_store()
        .get_session(&forked_session_id)
        .await?
        .expect("forked session should exist");
    assert_eq!(forked_session.cwd, Some(source_cwd.clone()));

    let forked_history = f
        .agent
        .storage
        .session_store()
        .get_history(&forked_session_id)
        .await?;
    assert_eq!(forked_history.len(), 2);
    assert_eq!(
        forked_history[0].parts,
        vec![MessagePart::Text {
            content: "Fork from here".to_string(),
        }]
    );
    assert_eq!(
        forked_history[1].parts,
        vec![MessagePart::Text {
            content: "Assistant kept in fork".to_string(),
        }]
    );

    handle_ui_message(
        &f.state,
        "conn-fork-ok",
        &tx,
        UiClientMessage::LoadSession {
            session_id: forked_session_id.clone(),
        },
    )
    .await;

    let mut loaded_events: Option<Vec<Value>> = None;
    for _ in 0..10 {
        if let Some(msg) = next_json(&mut rx).await
            && msg["type"] == "session_loaded"
            && msg["session_id"] == forked_session_id
        {
            loaded_events = msg["audit"]["events"].as_array().cloned();
            break;
        }
    }

    let loaded_events = loaded_events.expect("expected session_loaded for forked session");
    assert!(
        !loaded_events.is_empty(),
        "forked session should include copied audit events"
    );

    let has_prompt = loaded_events
        .iter()
        .any(|event| event["kind"]["type"] == "prompt_received");
    let has_assistant = loaded_events
        .iter()
        .any(|event| event["kind"]["type"] == "assistant_message_stored");
    let has_tool_call = loaded_events
        .iter()
        .any(|event| event["kind"]["type"] == "tool_call_start");

    assert!(
        has_prompt,
        "forked session should include prompt_received history"
    );
    assert!(
        has_assistant,
        "forked session should include assistant_message_stored history"
    );
    assert!(
        !has_tool_call,
        "forked session should not include non-conversational tool call events"
    );

    Ok(())
}

#[tokio::test]
async fn fork_session_without_active_session_returns_error() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let (tx, mut rx) = f.add_connection("conn-fork-no-session").await;

    handle_ui_message(
        &f.state,
        "conn-fork-no-session",
        &tx,
        UiClientMessage::ForkSession {
            message_id: "msg-does-not-matter".to_string(),
        },
    )
    .await;

    let fork_result = wait_for_fork_result(&mut rx).await;
    assert_eq!(fork_result["success"], false);
    assert!(fork_result["forked_session_id"].is_null());
    assert_eq!(fork_result["message"], "No active session");

    Ok(())
}

#[tokio::test]
async fn fork_session_invalid_message_id_returns_error() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let source_session_id = f.agent.create_session().await;
    let (tx, mut rx) = f.add_connection("conn-fork-bad-msg").await;

    {
        let mut connections = f.state.connections.lock().await;
        let conn = connections
            .get_mut("conn-fork-bad-msg")
            .expect("connection should exist");
        conn.sessions
            .insert("primary".to_string(), source_session_id.clone());
    }

    handle_ui_message(
        &f.state,
        "conn-fork-bad-msg",
        &tx,
        UiClientMessage::ForkSession {
            message_id: "missing-message-id".to_string(),
        },
    )
    .await;

    let fork_result = wait_for_fork_result(&mut rx).await;
    assert_eq!(fork_result["success"], false);
    assert_eq!(fork_result["source_session_id"], source_session_id);
    assert!(fork_result["forked_session_id"].is_null());
    let message = fork_result["message"]
        .as_str()
        .expect("error message should be present");
    assert!(message.contains("Failed to fork session"));

    Ok(())
}
