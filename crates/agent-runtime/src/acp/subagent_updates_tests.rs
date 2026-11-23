//! Delegated child sessions as ACP subagents: the `subagent_update` mapping,
//! its ordering against child traffic, and the delegate tool call's session
//! reference, for clients that advertise `subagents`.

use crate::acp::shared::{AcpLiveEventTranslator, AcpSessionUpdateCapabilities};
use crate::session::domain::{Delegation, DelegationStatus, ForkOrigin, ForkPointType};
use querymt_agent_core::delegation_update::DelegationStage;
use querymt_agent_core::events::{
    AgentEventKind, DurableEvent, EphemeralEvent, EventEnvelope, EventOrigin,
};
use serde_json::{Value, json};
use time::OffsetDateTime;

const PARENT: &str = "parent-1";
const CHILD: &str = "c1";

fn subagents() -> AcpSessionUpdateCapabilities {
    AcpSessionUpdateCapabilities {
        subagents: true,
        ..Default::default()
    }
}

fn translator(capabilities: AcpSessionUpdateCapabilities) -> AcpLiveEventTranslator {
    let mut translator = AcpLiveEventTranslator::new();
    translator.set_capabilities(capabilities);
    translator
}

fn durable(session_id: &str, seq: i64, kind: AgentEventKind) -> EventEnvelope {
    EventEnvelope::Durable(DurableEvent {
        session_seq: 0,
        event_id: format!("evt-{seq}"),
        stream_seq: seq,
        timestamp: seq,
        session_id: session_id.to_owned(),
        origin: EventOrigin::Local,
        source_node: None,
        kind,
    })
}

fn delegation() -> Delegation {
    Delegation {
        isolation: Default::default(),
        id: 0,
        public_id: "delegation-1".into(),
        session_id: 0,
        task_id: None,
        target_agent_id: "coder".into(),
        objective: "fix the build".into(),
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

fn requested() -> EventEnvelope {
    durable(
        PARENT,
        1,
        AgentEventKind::DelegationRequested {
            delegation: delegation(),
            tool_call_id: Some("call-1".into()),
            batch_id: None,
            cycle_id: None,
        },
    )
}

fn forked(model: Option<&str>, node: Option<&str>) -> EventEnvelope {
    durable(
        PARENT,
        2,
        AgentEventKind::SessionForked {
            parent_session_id: PARENT.into(),
            child_session_id: CHILD.into(),
            target_agent_id: "coder".into(),
            origin: ForkOrigin::Delegation,
            fork_point_type: ForkPointType::ProgressEntry,
            fork_point_ref: "delegation-1".into(),
            instructions: None,
            selected_model_id: model.map(str::to_owned),
            selected_provider_node_id: node.map(str::to_owned),
        },
    )
}

fn stage(seq: i64, stage: Option<DelegationStage>) -> EventEnvelope {
    EventEnvelope::Ephemeral(EphemeralEvent {
        session_id: PARENT.into(),
        session_seq: seq,
        timestamp: 3,
        origin: EventOrigin::Local,
        source_node: None,
        kind: AgentEventKind::DelegationStageChanged {
            delegation_id: "delegation-1".into(),
            stage,
            source_epoch: "workflow-1".into(),
        },
    })
}

fn completed() -> EventEnvelope {
    durable(
        PARENT,
        5,
        AgentEventKind::DelegationCompleted {
            delegation_id: "delegation-1".into(),
            result: Some("done".into()),
        },
    )
}

fn failed() -> EventEnvelope {
    durable(
        PARENT,
        5,
        AgentEventKind::DelegationFailed {
            delegation_id: "delegation-1".into(),
            error: "boom".into(),
        },
    )
}

fn delegate_tool_end(is_error: bool) -> EventEnvelope {
    durable(
        PARENT,
        6,
        AgentEventKind::ToolCallEnd {
            tool_call_id: "call-1".into(),
            tool_name: "delegate".into(),
            is_error,
            result: "{}".into(),
        },
    )
}

fn child_message() -> EventEnvelope {
    durable(
        CHILD,
        7,
        AgentEventKind::AssistantContentDelta {
            content: "hello from the child".into(),
            message_id: "child-msg-1".into(),
        },
    )
}

/// The addressed updates `event` produces, as `(session id, update JSON)`.
fn updates(translator: &mut AcpLiveEventTranslator, event: &EventEnvelope) -> Vec<(String, Value)> {
    translator
        .translate_event(event)
        .updates
        .into_iter()
        .map(|(session_id, update)| {
            (
                session_id,
                serde_json::to_value(update).expect("update serializes"),
            )
        })
        .collect()
}

fn kind(update: &Value) -> &str {
    update["sessionUpdate"].as_str().unwrap_or_default()
}

/// Runs the delegation up to its fork and returns the fork's updates.
fn fork_updates(translator: &mut AcpLiveEventTranslator) -> Vec<(String, Value)> {
    assert!(updates(translator, &requested()).is_empty());
    updates(translator, &forked(None, None))
}

fn subagent_update(updates: &[(String, Value)]) -> &Value {
    updates
        .iter()
        .map(|(_, update)| update)
        .find(|update| kind(update) == "subagent_update")
        .unwrap_or_else(|| panic!("no subagent_update in {updates:?}"))
}

#[test]
fn child_is_announced_before_its_events() {
    let mut translator = translator(subagents());
    let fork = fork_updates(&mut translator);

    let (session, announced) = &fork[0];
    assert_eq!(session, PARENT, "the announcement belongs to the parent");
    assert_eq!(kind(announced), "subagent_update");
    assert_eq!(announced["sessionId"], CHILD);
    assert_eq!(announced["title"], "coder");
    assert_eq!(announced["description"], "fix the build");

    let child = updates(&mut translator, &child_message());
    assert_eq!(child.len(), 1, "an announced child is not announced again");
    assert_eq!(child[0].0, CHILD);
    assert_eq!(kind(&child[0].1), "agent_message_chunk");
}

#[test]
fn child_is_announced_before_its_events_when_its_first_event_comes_first() {
    // A connection that learned of the delegation from a snapshot or a
    // recovery, not from the live fork event, never saw the fork.
    let mut live = translator(subagents());
    live.translate_event(&requested());
    let snapshot = live
        .translate_event(&forked(Some("gpt-5"), None))
        .delegation
        .expect("the fork projects a snapshot");

    let mut late = translator(subagents());
    late.restore_delegation_observations(&[snapshot]);
    let first = updates(&mut late, &child_message());

    let kinds: Vec<_> = first
        .iter()
        .map(|(session, update)| (session.as_str(), kind(update)))
        .collect();
    assert_eq!(
        kinds,
        [
            (PARENT, "subagent_update"),
            (PARENT, "tool_call_update"),
            (CHILD, "agent_message_chunk"),
        ],
        "the child's first event is preceded by its announcement"
    );
    assert_eq!(subagent_update(&first)["sessionId"], CHILD);

    let second = updates(&mut late, &child_message());
    assert_eq!(second.len(), 1, "announced once");
}

#[test]
fn client_without_the_subagents_capability() {
    let mut translator = translator(AcpSessionUpdateCapabilities::default());
    translator.translate_event(&requested());
    let fork = translator.translate_event(&forked(None, None));
    assert!(fork.updates.is_empty(), "no subagent_update: {fork:?}");
    assert!(
        fork.delegation.is_some(),
        "the delegation state is still projected"
    );

    // The delegate tool call and the child's events still arrive.
    let child = updates(&mut translator, &child_message());
    assert_eq!(child.len(), 1);
    assert_eq!(kind(&child[0].1), "agent_message_chunk");
    let end = updates(&mut translator, &delegate_tool_end(false));
    assert!(
        end.iter()
            .any(|(_, update)| kind(update) == "tool_call_update")
    );
    assert!(translator.translate_event(&completed()).updates.is_empty());
}

#[test]
fn tool_call_gains_a_session_reference() {
    let mut translator = translator(subagents());
    let fork = fork_updates(&mut translator);

    let kinds: Vec<_> = fork.iter().map(|(_, update)| kind(update)).collect();
    assert_eq!(kinds, ["subagent_update", "tool_call_update"]);
    let (session, reference) = &fork[1];
    assert_eq!(session, PARENT);
    assert_eq!(reference["toolCallId"], "call-1");
    assert_eq!(reference["content"][0]["type"], "session");
    assert_eq!(reference["content"][0]["sessionId"], CHILD);
}

#[test]
fn verification_is_reported_as_running_with_a_stage() {
    let mut translator = translator(subagents());
    fork_updates(&mut translator);

    let verifying = updates(
        &mut translator,
        &stage(10, Some(DelegationStage::Verifying)),
    );
    let update = subagent_update(&verifying);
    assert_eq!(update["sessionId"], CHILD);
    assert_eq!(update["state"]["state"], "running");
    assert_eq!(update["_meta"]["querymt"]["stage"], "verifying");
    assert!(update["capabilities"]["cancel"].is_object());

    // Leaving the stage clears it.
    let cleared = updates(&mut translator, &stage(11, None));
    let update = subagent_update(&cleared);
    assert!(
        update["_meta"]["querymt"].get("stage").is_none(),
        "{update}"
    );
}

#[test]
fn completion_reports_idle() {
    let mut translator = translator(subagents());
    fork_updates(&mut translator);

    let done = updates(&mut translator, &completed());
    assert_eq!(subagent_update(&done)["state"]["state"], "idle");

    let end = updates(&mut translator, &delegate_tool_end(false));
    let call = end
        .iter()
        .map(|(_, update)| update)
        .find(|update| kind(update) == "tool_call_update")
        .expect("the delegate tool call ends");
    assert_eq!(call["status"], "completed");
}

#[test]
fn failure_reports_idle_and_a_failed_tool_call() {
    let mut translator = translator(subagents());
    fork_updates(&mut translator);

    let failure = updates(&mut translator, &failed());
    assert_eq!(subagent_update(&failure)["state"]["state"], "idle");

    let end = updates(&mut translator, &delegate_tool_end(true));
    let call = end
        .iter()
        .map(|(_, update)| update)
        .find(|update| kind(update) == "tool_call_update")
        .expect("the delegate tool call ends");
    assert_eq!(call["status"], "failed");
}

#[test]
fn selected_model_is_reported_in_metadata() {
    let mut translator = translator(subagents());
    translator.translate_event(&requested());
    let fork = updates(&mut translator, &forked(Some("gpt-5"), Some("gpu-node")));

    let meta = &subagent_update(&fork)["_meta"]["querymt"];
    assert_eq!(meta["modelId"], "gpt-5");
    assert_eq!(meta["providerNodeId"], "gpu-node");
    assert_eq!(meta["delegationId"], "delegation-1");
    assert!(meta["requestedAt"].is_number() && meta["forkedAt"].is_number());
}

#[test]
fn cancel_capability_is_withdrawn_after_completion() {
    let mut translator = translator(subagents());
    let fork = fork_updates(&mut translator);
    assert!(
        subagent_update(&fork)["capabilities"]["cancel"].is_object(),
        "a running child can be cancelled"
    );

    let done = updates(&mut translator, &completed());
    let update = subagent_update(&done);
    assert_eq!(
        update["capabilities"],
        json!(null),
        "the final update clears the capability set: {update}"
    );
}

#[test]
fn clients_advertise_different_preview_capabilities_for_subagents() {
    // Two connections over the same events: only the one that advertised
    // `subagents` receives subagent variants, both get the child's traffic.
    let mut with = translator(subagents());
    let mut without = translator(AcpSessionUpdateCapabilities {
        cursors: true,
        ..Default::default()
    });
    for translator in [&mut with, &mut without] {
        translator.translate_event(&requested());
    }
    let fork = forked(Some("gpt-5"), None);
    let kinds = |translator: &mut AcpLiveEventTranslator, event: &EventEnvelope| {
        updates(translator, event)
            .iter()
            .map(|(_, update)| kind(update).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        kinds(&mut with, &fork),
        ["subagent_update", "tool_call_update"]
    );
    assert!(kinds(&mut without, &fork).is_empty());
    let message = child_message();
    assert_eq!(kinds(&mut with, &message), ["agent_message_chunk"]);
    assert_eq!(kinds(&mut without, &message), ["agent_message_chunk"]);
}

#[test]
fn delegation_before_its_fork_has_no_subagent_update() {
    let mut translator = translator(subagents());
    assert!(updates(&mut translator, &requested()).is_empty());
}

fn agent_event(seq: i64, kind: AgentEventKind) -> querymt_agent_core::events::AgentEvent {
    querymt_agent_core::events::AgentEvent {
        session_seq: seq,
        seq,
        timestamp: seq,
        session_id: PARENT.into(),
        origin: EventOrigin::Local,
        source_node: None,
        kind,
    }
}

/// A completed delegation as the journal records it: the delegate tool call
/// starts, the delegation is requested, forks its child and completes.
fn delegation_history(
    n: i64,
    delegation_id: &str,
    tool_call_id: &str,
    child: &str,
) -> Vec<querymt_agent_core::events::AgentEvent> {
    let mut delegation = delegation();
    delegation.public_id = delegation_id.into();
    vec![
        agent_event(
            n * 10,
            AgentEventKind::ToolCallStart {
                tool_call_id: tool_call_id.into(),
                tool_name: "delegate".into(),
                arguments: "{}".into(),
            },
        ),
        agent_event(
            n * 10 + 1,
            AgentEventKind::DelegationRequested {
                delegation,
                tool_call_id: Some(tool_call_id.into()),
                batch_id: None,
                cycle_id: None,
            },
        ),
        agent_event(
            n * 10 + 2,
            AgentEventKind::SessionForked {
                parent_session_id: PARENT.into(),
                child_session_id: child.into(),
                target_agent_id: "coder".into(),
                origin: ForkOrigin::Delegation,
                fork_point_type: ForkPointType::ProgressEntry,
                fork_point_ref: delegation_id.into(),
                instructions: None,
                selected_model_id: Some("gpt-5".into()),
                selected_provider_node_id: None,
            },
        ),
        agent_event(
            n * 10 + 3,
            AgentEventKind::DelegationCompleted {
                delegation_id: delegation_id.into(),
                result: Some("done".into()),
            },
        ),
        agent_event(
            n * 10 + 4,
            AgentEventKind::ToolCallEnd {
                tool_call_id: tool_call_id.into(),
                tool_name: "delegate".into(),
                is_error: false,
                result: "{}".into(),
            },
        ),
    ]
}

fn replayed(
    events: Vec<querymt_agent_core::events::AgentEvent>,
    capabilities: AcpSessionUpdateCapabilities,
) -> Vec<Value> {
    crate::acp::shared::replay_agent_events_materialized(
        PARENT,
        events,
        &std::collections::HashMap::new(),
        &capabilities,
    )
    .into_iter()
    .map(|notification| {
        assert_eq!(notification.session_id.0.as_ref(), PARENT);
        serde_json::to_value(notification.update).expect("update serializes")
    })
    .collect()
}

#[test]
fn replay_restores_completed_delegations() {
    let mut history = delegation_history(1, "delegation-1", "call-1", "c1");
    history.extend(delegation_history(2, "delegation-2", "call-2", "c2"));
    let updates = replayed(history.clone(), subagents());

    let announced: Vec<_> = updates
        .iter()
        .enumerate()
        .filter(|(_, update)| kind(update) == "subagent_update")
        .collect();
    assert_eq!(announced.len(), 2, "one subagent_update per child");
    for (child, (position, update)) in ["c1", "c2"].into_iter().zip(announced) {
        assert_eq!(update["sessionId"], child);
        assert_eq!(update["state"]["state"], "idle");
        assert_eq!(update["capabilities"], json!(null));
        let referenced = updates
            .iter()
            .position(|update| {
                kind(update) == "tool_call_update" && update["content"][0]["sessionId"] == child
            })
            .unwrap_or_else(|| panic!("no session reference to {child}"));
        assert!(
            position < referenced,
            "{child} is announced at {position}, before its reference at {referenced}"
        );
    }

    // A client without the capability replays no subagent state.
    let without = replayed(history, AcpSessionUpdateCapabilities::default());
    assert!(
        without
            .iter()
            .all(|update| kind(update) != "subagent_update")
    );
    assert!(
        without
            .iter()
            .all(|update| update["content"][0]["type"] != "session")
    );
}

#[tokio::test]
async fn snapshot_shows_a_running_delegation() {
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
        child_session_id: CHILD.into(),
        target_agent_id: "coder".into(),
        origin: ForkOrigin::Delegation,
        fork_point_type: ForkPointType::ProgressEntry,
        fork_point_ref: "delegation-1".into(),
        instructions: None,
        selected_model_id: None,
        selected_provider_node_id: None,
    });
    stream.emit(AgentEventKind::DelegationStageChanged {
        delegation_id: "delegation-1".into(),
        stage: Some(DelegationStage::Verifying),
        source_epoch: "workflow-1".into(),
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while stream.published_tip() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("events are published");

    let snapshot = crate::session::load_session_snapshot(
        fixture.handle.as_ref(),
        crate::session::backend::StorageBackend::view_store(fixture.storage.as_ref()).unwrap(),
        &session_id,
    )
    .await
    .unwrap();

    assert_eq!(snapshot.subagent_updates.len(), 1);
    let update = serde_json::to_value(&snapshot.subagent_updates[0]).unwrap();
    assert_eq!(update["sessionId"], CHILD);
    assert_eq!(update["state"]["state"], "running");
    assert_eq!(update["_meta"]["querymt"]["stage"], "verifying");
    assert_eq!(
        snapshot.delegation_updates.len(),
        1,
        "the snapshot still carries delegationUpdates"
    );
    let wire = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(wire["subagentUpdates"][0]["sessionId"], CHILD);
    assert_eq!(wire["delegationUpdates"][0]["stage"], "verifying");
}
