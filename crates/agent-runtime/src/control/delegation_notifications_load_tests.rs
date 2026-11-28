//! Runtime-side test for delegation update delivery on session load.
//!
//! It needs `TestAgent` and the ACP transport, so it stays with the runtime
//! while `delegation_notifications.rs` itself belongs to the services layer.

use crate::control::delegation_notifications::*;
use crate::session::domain::{Delegation, DelegationStatus, ForkOrigin, ForkPointType};
use querymt_agent_core::events::{AgentEvent, AgentEventKind, EventEnvelope, EventOrigin};
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
        objective_hash: querymt_agent_core::hash::RapidHash::default(),
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
        handoff_from: None,
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

#[test]
fn live_translator_emits_camel_case_extension_notification() {
    let envelope = EventEnvelope::from(event(
        1,
        AgentEventKind::DelegationRequested {
            delegation: delegation(),
            tool_call_id: Some("call-1".into()),
            batch_id: None,
            cycle_id: None,
            handoff_from: None,
        },
    ));
    let mut translator = crate::acp::shared::AcpLiveEventTranslator::new();

    let notification = translator.translate_notification(&envelope).unwrap();

    assert_eq!(
        notification["method"],
        crate::acp::shared::QMT_NOTIFICATION_DELEGATION_UPDATE
    );
    assert_eq!(notification["params"]["delegationId"], "delegation-1");
    assert_eq!(notification["params"]["toolCallId"], "call-1");
    assert_eq!(notification["params"]["state"], "requested");
    assert!(notification["params"].get("childSessionId").is_none());
    assert!(notification["params"].get("resultSummary").is_none());
}

#[test]
fn seeding_history_enriches_lifecycle_behind_live_stage_watermark() {
    let mut requested = event(
        1,
        AgentEventKind::DelegationRequested {
            delegation: delegation(),
            tool_call_id: Some("call-1".into()),
            batch_id: None,
            cycle_id: None,
            handoff_from: None,
        },
    );
    requested.session_seq = 1;
    let mut forked = event(
        2,
        AgentEventKind::SessionForked {
            parent_session_id: "parent-1".into(),
            child_session_id: "child-1".into(),
            target_agent_id: "coder".into(),
            origin: ForkOrigin::Delegation,
            fork_point_type: ForkPointType::ProgressEntry,
            fork_point_ref: "delegation-1".into(),
            instructions: None,
            selected_model_id: Some("confirmed-model".into()),
            selected_provider_node_id: None,
        },
    );
    forked.session_seq = 2;
    let mut completed = event(
        3,
        AgentEventKind::DelegationCompleted {
            delegation_id: "delegation-1".into(),
            result: Some("durable outcome".into()),
        },
    );
    completed.session_seq = 3;
    let mut stage = event(
        50,
        AgentEventKind::DelegationStageChanged {
            delegation_id: "delegation-1".into(),
            stage: Some(DelegationStage::Verifying),
            source_epoch: "current-owner".into(),
        },
    );
    stage.session_seq = 50;
    for (terminal, expected) in [
        (None, DelegationUpdateState::Forked),
        (
            Some(completed.kind.clone()),
            DelegationUpdateState::Completed,
        ),
        (
            Some(AgentEventKind::DelegationFailed {
                delegation_id: "delegation-1".into(),
                error: "durable failure".into(),
            }),
            DelegationUpdateState::Failed,
        ),
        (
            Some(AgentEventKind::DelegationCancelled {
                delegation_id: "delegation-1".into(),
            }),
            DelegationUpdateState::Cancelled,
        ),
    ] {
        let mut translator = crate::acp::shared::AcpLiveEventTranslator::new();
        assert!(
            translator
                .translate_notification(&EventEnvelope::from(stage.clone()))
                .is_none()
        );
        let mut history = vec![requested.clone(), forked.clone()];
        if let Some(kind) = terminal {
            let mut terminal = completed.clone();
            terminal.kind = kind;
            history.push(terminal);
        }
        let meta = serde_json::Map::from_iter([(
            "querymt/sessionLoadSnapshot.v1".into(),
            serde_json::json!({"audit":{"events":history}}),
        )]);
        let recovered = translator
            .seed_history_from_meta("parent-1", Some(&meta))
            .unwrap();
        assert_eq!(
            recovered.len(),
            1,
            "actual seed_history must deliver enriched lifecycle behind a live watermark, including a live fork"
        );
        assert_eq!(recovered[0].state, expected);
        assert_eq!(
            recovered[0].result_summary.as_deref(),
            (expected == DelegationUpdateState::Completed).then_some("durable outcome")
        );
        assert_eq!(
            recovered[0].error.as_deref(),
            (expected == DelegationUpdateState::Failed).then_some("durable failure")
        );
        assert_eq!(recovered[0].child_session_id.as_deref(), Some("child-1"));
        assert_eq!(
            recovered[0].selected_model_id.as_deref(),
            Some("confirmed-model")
        );
        assert_eq!(recovered[0].session_seq, Some(50));
        assert_eq!(recovered[0].stage, Some(DelegationStage::Verifying));
        let mut next_stage = stage.clone();
        next_stage.session_seq = 51;
        let observed = translator
            .translate_notification(&EventEnvelope::from(next_stage.clone()))
            .unwrap();
        assert_eq!(
            observed["params"]["childSessionId"], "child-1",
            "queued stage must not prevent fork identity hydration"
        );
        assert_eq!(observed["params"]["selectedModelId"], "confirmed-model");
        assert_eq!(
            observed["params"]["state"],
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(observed["params"]["stage"], "verifying");
        assert_eq!(observed["params"]["sourceEpoch"], "current-owner");
        assert!(
            translator
                .translate_notification(&EventEnvelope::from(stage.clone()))
                .is_none(),
            "real stale live stage must still be rejected"
        );
        let mut wrong_epoch = next_stage.clone();
        wrong_epoch.session_seq = 100;
        if let AgentEventKind::DelegationStageChanged { source_epoch, .. } = &mut wrong_epoch.kind {
            *source_epoch = "other-owner".into();
        }
        assert!(
            translator
                .translate_notification(&EventEnvelope::from(wrong_epoch))
                .is_none(),
            "history must not weaken epoch rejection"
        );
        next_stage.session_seq = 52;
        if let AgentEventKind::DelegationStageChanged { stage, .. } = &mut next_stage.kind {
            *stage = None;
        }
        let cleaned = translator
            .translate_notification(&EventEnvelope::from(next_stage))
            .unwrap();
        assert_eq!(cleaned["params"]["activity"], "historical");
        assert!(
            translator
                .seed_history_from_meta("parent-1", Some(&meta))
                .unwrap()
                .is_empty(),
            "repeat history must not revive activity or reemit terminal"
        );
    }
}

#[test]
fn seeding_connection_history_preserves_live_delegation_completion() {
    let history = vec![
        event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: Some("call-1".into()),
                batch_id: None,
                cycle_id: None,
                handoff_from: None,
            },
        ),
        event(
            2,
            AgentEventKind::SessionForked {
                parent_session_id: "parent-1".into(),
                child_session_id: "child-1".into(),
                target_agent_id: "coder".into(),
                origin: ForkOrigin::Delegation,
                fork_point_type: ForkPointType::ProgressEntry,
                fork_point_ref: "delegation-1".into(),
                instructions: None,
                selected_model_id: None,
                selected_provider_node_id: None,
            },
        ),
    ];
    let meta = serde_json::Map::from_iter([(
        "querymt/sessionLoadSnapshot.v1".into(),
        serde_json::json!({ "audit": { "events": history } }),
    )]);
    let completed = EventEnvelope::from(event(
        3,
        AgentEventKind::DelegationCompleted {
            delegation_id: "delegation-1".into(),
            result: Some("done".into()),
        },
    ));
    // Completion can arrive while history is being read, before the request is seeded.
    let mut translator = crate::acp::shared::AcpLiveEventTranslator::new();
    assert!(translator.translate_notification(&completed).is_none());
    let recovered = translator
        .seed_history_from_meta("parent-1", Some(&meta))
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].state, DelegationUpdateState::Completed);
    assert_eq!(recovered[0].tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(recovered[0].child_session_id.as_deref(), Some("child-1"));

    // Repeated loads must not roll newer terminal state back to the historical fork.
    assert!(
        translator
            .seed_history_from_meta("parent-1", Some(&meta))
            .unwrap()
            .is_empty()
    );
    let repeated = translator
        .translate_notification(&EventEnvelope::from(history[0].clone()))
        .unwrap();
    assert_eq!(repeated["params"]["state"], "completed");

    let mut cleanup = event(
        50,
        AgentEventKind::DelegationStageChanged {
            delegation_id: "delegation-1".into(),
            stage: None,
            source_epoch: "current-owner".into(),
        },
    );
    cleanup.session_seq = 50;
    translator.translate_notification(&cleanup.into()).unwrap();
    let mut enriched_fork = history[1].clone();
    if let AgentEventKind::SessionForked {
        selected_model_id,
        selected_provider_node_id,
        ..
    } = &mut enriched_fork.kind
    {
        *selected_model_id = Some("confirmed-model".into());
        *selected_provider_node_id = Some("confirmed-node".into());
    }
    let mut conflicts = Vec::new();
    let mut foreign = enriched_fork.clone();
    foreign.session_id = "other-parent".into();
    conflicts.push(foreign);
    let mut wrong_parent = enriched_fork.clone();
    if let AgentEventKind::SessionForked {
        parent_session_id, ..
    } = &mut wrong_parent.kind
    {
        *parent_session_id = "other-parent".into();
    }
    conflicts.push(wrong_parent);
    let mut wrong_child = enriched_fork.clone();
    if let AgentEventKind::SessionForked {
        child_session_id, ..
    } = &mut wrong_child.kind
    {
        *child_session_id = "other-child".into();
    }
    conflicts.push(wrong_child);
    let mut wrong_request = history[0].clone();
    if let AgentEventKind::DelegationRequested {
        delegation,
        tool_call_id,
        ..
    } = &mut wrong_request.kind
    {
        delegation.target_agent_id = "other-agent".into();
        *tool_call_id = Some("other-tool".into());
    }
    conflicts.push(wrong_request);
    let bad_meta = serde_json::Map::from_iter([(
        "querymt/sessionLoadSnapshot.v1".into(),
        serde_json::json!({"audit":{"events":conflicts}}),
    )]);
    assert!(
        translator
            .seed_history_from_meta("parent-1", Some(&bad_meta))
            .unwrap()
            .is_empty(),
        "conflicting identity is not enrichment"
    );
    let enrichment = serde_json::Map::from_iter([(
        "querymt/sessionLoadSnapshot.v1".into(),
        serde_json::json!({"audit":{"events":[enriched_fork, event(4, AgentEventKind::DelegationFailed { delegation_id: "delegation-1".into(), error: "must not replace outcome".into() })]}}),
    )]);
    let recovered = translator
        .seed_history_from_meta("parent-1", Some(&enrichment))
        .unwrap();
    assert_eq!(
        recovered.len(),
        1,
        "late missing metadata must be delivered even after cleanup"
    );
    assert_eq!(recovered[0].state, DelegationUpdateState::Completed);
    assert_eq!(recovered[0].result_summary.as_deref(), Some("done"));
    assert_eq!(recovered[0].error, None);
    assert_eq!(recovered[0].child_session_id.as_deref(), Some("child-1"));
    assert_eq!(
        recovered[0].selected_model_id.as_deref(),
        Some("confirmed-model")
    );
    assert_eq!(
        recovered[0].selected_provider_node_id.as_deref(),
        Some("confirmed-node")
    );
    assert_eq!(recovered[0].session_seq, Some(50));
    assert_eq!(recovered[0].source_epoch.as_deref(), Some("current-owner"));
    assert_eq!(recovered[0].activity, DelegationActivity::Historical);
    assert_eq!(recovered[0].stage, None);
    assert!(
        translator
            .seed_history_from_meta("parent-1", Some(&meta))
            .unwrap()
            .is_empty(),
        "old missing model metadata must not erase current identity"
    );
    assert!(
        translator
            .seed_history_from_meta("parent-1", Some(&enrichment))
            .unwrap()
            .is_empty()
    );
}
