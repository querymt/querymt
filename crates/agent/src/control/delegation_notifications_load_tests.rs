//! Runtime-side test for delegation update delivery on session load.
//!
//! It needs `TestAgent` and the ACP transport, so it stays with the runtime
//! while `delegation_notifications.rs` itself belongs to the services layer.

use crate::events::{AgentEvent, AgentEventKind, EventEnvelope, EventOrigin};
use crate::session::domain::{Delegation, DelegationStatus, ForkOrigin, ForkPointType};
use time::OffsetDateTime;

fn delegation() -> Delegation {
    Delegation {
        isolation: Default::default(),
        id: 0,
        public_id: "delegation-1".into(),
        session_id: 0,
        task_id: None,
        target_agent_id: "coder".into(),
        objective: "Implement it".into(),
        objective_hash: crate::hash::RapidHash::default(),
        context: None,
        constraints: None,
        expected_output: None,
        verification_spec: None,
        planning_summary: None,
        status: DelegationStatus::Requested,
        retry_count: 0,
        created_at: OffsetDateTime::UNIX_EPOCH,
        completed_at: None,
    }
}

fn event(timestamp: i64, kind: AgentEventKind) -> AgentEvent {
    AgentEvent {
        session_seq: 0,
        seq: timestamp,
        timestamp,
        session_id: "parent-1".into(),
        origin: EventOrigin::Local,
        source_node: None,
        kind,
    }
}

#[tokio::test]
async fn load_delivers_completion_buffered_before_history_once() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let session_id = fixture.create_session().await;
    let stream = fixture.config.event_sink.session_stream(&session_id);
    stream.emit(AgentEventKind::DelegationRequested {
        delegation: delegation(),
        tool_call_id: Some("call-1".into()),
        batch_id: None,
        cycle_id: None,
    });
    stream.emit(AgentEventKind::SessionForked {
        parent_session_id: session_id.clone(),
        child_session_id: "child-1".into(),
        target_agent_id: "coder".into(),
        origin: ForkOrigin::Delegation,
        fork_point_type: ForkPointType::ProgressEntry,
        fork_point_ref: "delegation-1".into(),
        instructions: None,
        selected_model_id: None,
        selected_provider_node_id: None,
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while stream.published_tip() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut completed = event(
        i64::MAX,
        AgentEventKind::DelegationCompleted {
            delegation_id: "delegation-1".into(),
            result: Some("done".into()),
        },
    );
    completed.session_id = session_id.clone();
    // Load uses metadata; extension attachment uses the event-stream fallback.
    for method in ["session/load", "querymt/session/runtimeState"] {
        let translator = std::sync::Arc::new(std::sync::Mutex::new(
            crate::acp::shared::AcpLiveEventTranslator::new(),
        ));
        assert!(
            translator
                .lock()
                .unwrap()
                .translate_notification(&EventEnvelope::from(completed.clone()))
                .is_none()
        );
        let owners = crate::acp::shared::SessionOwnerMap::default();
        let pending = crate::acp::shared::PendingElicitationMap::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let context = crate::acp::shared::RpcDispatchContext {
            translator: Some(translator),
            session_bridge: Some(crate::acp::client_bridge::ClientBridgeSender::new(tx)),
            session_load: Some(std::sync::Arc::new(std::sync::Mutex::new(
                crate::acp::shared::AcpSessionLoadPreferences {
                    snapshot_history: true,
                },
            ))),
            ..Default::default()
        };
        for expected_count in [1, 0] {
            let output = crate::acp::shared::handle_rpc_message_with_context(
                fixture.handle.as_ref(), &owners, &pending, "connection-1",
                crate::acp::shared::RpcMessage {
                    jsonrpc: "2.0".into(), id: Some(serde_json::json!(1)), method: method.into(),
                    params: if method == "session/load" {
                        serde_json::json!({"sessionId": session_id, "cwd": "/tmp", "mcpServers": []})
                    } else { serde_json::json!({"session_id": session_id}) },
                }, context.clone(),
            ).await;
            assert!(output.response.unwrap().error.is_none());
            let mut updates: Vec<_> = output
                .notifications
                .into_iter()
                .filter(|notification| notification["method"] == "querymt/session/delegationUpdate")
                .map(|notification| notification["params"].clone())
                .collect();
            while let Ok(message) = rx.try_recv() {
                if let crate::acp::client_bridge::ClientBridgeMessage::ExtNotification(
                    notification,
                ) = message
                {
                    updates.push(
                        serde_json::from_str::<serde_json::Value>(notification.params.get())
                            .unwrap(),
                    );
                }
            }
            assert_eq!(updates.len(), expected_count, "{method}");
            if let Some(update) = updates.first() {
                assert_eq!(update["state"], "completed");
                assert_eq!(update["toolCallId"], "call-1");
                assert_eq!(update["childSessionId"], "child-1");
            }
        }
    }
}
