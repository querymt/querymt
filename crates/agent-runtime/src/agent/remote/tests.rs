#![cfg_attr(not(test), allow(unused_imports, dead_code))]
//! Tests for remote session support: SessionActorRef, SessionRegistry,
//! RemoteNodeManager, EventRelayActor, and EventForwarder.
//!
//! These tests exercise the local-variant code paths end-to-end without
//! requiring a live kameo mesh / libp2p swarm.  Feature-gated tests that
//! exercise the `Remote` variant live in a separate `#[cfg(feature = "remote")]`
//! submodule at the bottom.

use crate::agent::agent_config::AgentConfig;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::remote::SessionActorRef;
use crate::agent::session_actor::SessionActor;
use crate::agent::session_registry::SessionRegistry;
use crate::session::backend::StorageBackend;
use crate::session::sqlite_storage::SqliteStorage;
use crate::test_utils::{
    MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
};
use kameo::actor::Spawn;
use querymt::LLMParams;
use querymt::chat::ReasoningEffort;
use querymt_agent_core::events::{AgentEvent, AgentEventKind, EventOrigin};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

// ═══════════════════════════════════════════════════════════════════════════
//  Test helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Build a minimal `AgentConfig` suitable for unit tests that don't need LLM calls.
///
/// Returns `(Arc<AgentConfig>, TempDir)` — the `TempDir` must be kept alive for
/// the duration of the test.
#[doc(hidden)]
pub async fn test_agent_config() -> (Arc<AgentConfig>, TempDir) {
    let mut provider = MockLlmProvider::new();
    provider.expect_chat_with_tools().returning(|_, _| {
        Ok(crate::test_utils::MockChatResponse::text_only("scheduled fixture reply").into())
    });
    let provider = Arc::new(Mutex::new(provider));
    let factory = Arc::new(TestProviderFactory::new(SharedLlmProvider {
        inner: provider,
        tools: vec![].into_boxed_slice(),
    }));
    let (registry, temp_dir) = mock_plugin_registry(factory).expect("create plugin registry");
    let plugin_registry = Arc::new(registry);

    let storage = Arc::new(
        SqliteStorage::connect(":memory:".into())
            .await
            .expect("create sqlite storage"),
    );
    let llm = LLMParams::new().provider("mock").model("mock");

    let mut builder = AgentConfigBuilder::new(plugin_registry, storage.clone(), llm);
    if let Some(repo) = storage.schedule_repository() {
        builder = builder.with_schedule_repository(repo);
    }
    let config = Arc::new(builder.build());
    (config, temp_dir)
}

/// Persist and spawn a `SessionActor` with default runtime.
async fn spawn_test_session_with_id(
    config: Arc<AgentConfig>,
    session_id: &str,
) -> (SessionActorRef, String) {
    let store = config.provider.history_store();
    let session = store
        .create_session(Some(session_id.to_string()), None, None, None)
        .await
        .expect("create test session");
    let llm_config = store
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("mock"))
        .await
        .expect("create test LLM config");
    store
        .set_session_llm_config(&session.public_id, llm_config.id)
        .await
        .expect("bind test LLM config");
    let public_id = session.public_id;
    let runtime = SessionRuntime::new(
        None,
        HashMap::new(),
        crate::agent::core::McpToolState::empty(),
    );
    let actor = SessionActor::new(config, public_id.clone(), runtime);
    let actor_ref = SessionActor::spawn(actor);
    (SessionActorRef::Local(actor_ref), public_id)
}

async fn spawn_test_session(config: Arc<AgentConfig>, session_id: &str) -> SessionActorRef {
    spawn_test_session_with_id(config, session_id).await.0
}

// ═══════════════════════════════════════════════════════════════════════════
//  SessionActorRef — Local variant
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_local_ref_is_not_remote() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-local-1").await;
    assert!(!session_ref.is_remote());
    assert_eq!(session_ref.node_label(), "local");
}

#[tokio::test]
async fn test_local_ref_get_mode_default_is_build() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-mode-1").await;
    let mode = session_ref
        .get_mode()
        .await
        .expect("get_mode should succeed");
    assert_eq!(mode, AgentMode::Build);
}

#[tokio::test]
async fn test_local_ref_set_mode_roundtrip() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-mode-2").await;

    session_ref
        .set_mode(AgentMode::Plan)
        .await
        .expect("set_mode Plan");
    let mode = session_ref.get_mode().await.expect("get_mode");
    assert_eq!(mode, AgentMode::Plan);

    session_ref
        .set_mode(AgentMode::Build)
        .await
        .expect("set_mode Build");
    let mode = session_ref.get_mode().await.expect("get_mode");
    assert_eq!(mode, AgentMode::Build);
}

#[tokio::test]
async fn test_local_ref_mode_model_bindings_are_session_scoped() {
    let (config, _td) = test_agent_config().await;
    let (first, first_session_id) =
        spawn_test_session_with_id(config.clone(), "binding-first").await;
    let second = spawn_test_session(config.clone(), "binding-second").await;
    let remote_node = "12D3KooWRemotePlanNode";

    first
        .set_session_model(crate::agent::session_control::SessionModelSelection {
            model_id: "mock/build-model".to_string(),
            provider_node_id: None,
        })
        .await
        .expect("set first build model");
    first.set_mode(AgentMode::Plan).await.expect("enter plan");
    first
        .set_session_model(crate::agent::session_control::SessionModelSelection {
            model_id: "mock/plan-model".to_string(),
            provider_node_id: Some(remote_node.to_string()),
        })
        .await
        .expect("set first remote plan model");

    for _ in 0..2 {
        let build = first
            .set_mode(AgentMode::Build)
            .await
            .expect("return to build");
        assert_eq!(build.current.effective_model.model, "build-model");
        assert_eq!(build.current.effective_model.provider_node_id, None);

        let plan = first
            .set_mode(AgentMode::Plan)
            .await
            .expect("return to plan");
        assert_eq!(plan.current.effective_model.model, "plan-model");
        assert_eq!(
            plan.current.effective_model.provider_node_id.as_deref(),
            Some(remote_node)
        );
    }

    first.shutdown().await.expect("shutdown first actor");
    let runtime = SessionRuntime::new(
        None,
        HashMap::new(),
        crate::agent::core::McpToolState::empty(),
    );
    let resumed = SessionActorRef::Local(SessionActor::spawn(SessionActor::new(
        config,
        first_session_id,
        runtime,
    )));

    let resumed_plan = resumed
        .get_session_control()
        .await
        .expect("load persisted plan control state");
    assert_eq!(resumed_plan.active_mode, AgentMode::Plan);
    assert_eq!(resumed_plan.effective_model.model, "plan-model");
    assert_eq!(
        resumed_plan.effective_model.provider_node_id.as_deref(),
        Some(remote_node)
    );

    let resumed_build = resumed
        .set_mode(AgentMode::Build)
        .await
        .expect("restore persisted build model");
    assert_eq!(resumed_build.current.effective_model.model, "build-model");
    assert_eq!(resumed_build.current.effective_model.provider_node_id, None);

    let resumed_plan = resumed
        .set_mode(AgentMode::Plan)
        .await
        .expect("restore persisted remote plan model");
    assert_eq!(resumed_plan.current.effective_model.model, "plan-model");
    assert_eq!(
        resumed_plan
            .current
            .effective_model
            .provider_node_id
            .as_deref(),
        Some(remote_node)
    );

    let second_state = second
        .get_session_control()
        .await
        .expect("second control state");
    assert_eq!(second_state.effective_model.model, "mock");
}

#[tokio::test]
async fn stale_actor_rebases_mode_after_revision_conflict() {
    let (config, _td) = test_agent_config().await;
    let (fresh, session_id) = spawn_test_session_with_id(config.clone(), "stale-mode").await;
    let initial = fresh
        .get_session_control()
        .await
        .expect("initialize control state");
    assert_eq!(initial.revision, 1);

    let stale = SessionActorRef::Local(SessionActor::spawn(
        SessionActor::new(
            config.clone(),
            session_id.clone(),
            SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        )
        .with_control_state(initial),
    ));

    let advanced = fresh
        .set_mode(AgentMode::Plan)
        .await
        .expect("fresh actor advances revision");
    assert_eq!(advanced.current.revision, 2);
    assert_eq!(advanced.current.active_mode, AgentMode::Plan);

    let rebased = stale
        .set_mode(AgentMode::Build)
        .await
        .expect("stale actor rebases onto latest revision");
    assert_eq!(rebased.current.active_mode, AgentMode::Build);
    assert!(rebased.current.revision > 2);

    let persisted = config
        .provider
        .history_store()
        .get_session_control(&session_id)
        .await
        .expect("load persisted control")
        .expect("control state exists");
    assert_eq!(persisted.active_mode, AgentMode::Build);
    assert_eq!(persisted.revision, rebased.current.revision);
}

#[tokio::test]
async fn stale_actor_rebases_model_after_revision_conflict() {
    let (config, _td) = test_agent_config().await;
    let (fresh, session_id) = spawn_test_session_with_id(config.clone(), "stale-model").await;
    let initial = fresh
        .get_session_control()
        .await
        .expect("initialize control state");

    let stale = SessionActorRef::Local(SessionActor::spawn(
        SessionActor::new(
            config,
            session_id,
            SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        )
        .with_control_state(initial),
    ));

    fresh
        .set_mode(AgentMode::Plan)
        .await
        .expect("fresh actor switches to plan");

    let rebased = stale
        .set_session_model(crate::agent::session_control::SessionModelSelection {
            model_id: "mock/rebased-model".to_string(),
            provider_node_id: None,
        })
        .await
        .expect("stale actor rebases model onto current mode");
    assert_eq!(rebased.current.active_mode, AgentMode::Plan);
    assert_eq!(rebased.current.effective_model.model, "rebased-model");
    assert_eq!(
        rebased
            .current
            .binding_for(AgentMode::Plan)
            .map(|binding| binding.model.as_str()),
        Some("rebased-model")
    );
}

#[tokio::test]
async fn stale_actor_rebases_reasoning_effort_after_revision_conflict() {
    let (config, _td) = test_agent_config().await;
    let (fresh, session_id) = spawn_test_session_with_id(config.clone(), "stale-effort").await;
    let initial = fresh
        .get_session_control()
        .await
        .expect("initialize control state");

    let stale = SessionActorRef::Local(SessionActor::spawn(
        SessionActor::new(
            config.clone(),
            session_id.clone(),
            SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        )
        .with_control_state(initial),
    ));

    fresh
        .set_mode(AgentMode::Review)
        .await
        .expect("fresh actor switches to review");

    let rebased = stale
        .set_reasoning_effort(Some(ReasoningEffort::High))
        .await
        .expect("stale actor rebases effort onto current mode");
    assert_eq!(rebased.current.active_mode, AgentMode::Review);
    assert_eq!(
        rebased.current.reasoning_effort,
        Some(ReasoningEffort::High)
    );

    let persisted = config
        .provider
        .history_store()
        .get_session_control(&session_id)
        .await
        .expect("load persisted control")
        .expect("control state exists");
    assert_eq!(persisted.active_mode, AgentMode::Review);
    assert_eq!(persisted.reasoning_effort, Some(ReasoningEffort::High));
}

#[tokio::test]
async fn stale_actor_rebases_llm_config_after_revision_conflict() {
    let (config, _td) = test_agent_config().await;
    let (fresh, session_id) = spawn_test_session_with_id(config.clone(), "stale-config").await;
    let initial = fresh
        .get_session_control()
        .await
        .expect("initialize control state");

    let stale = SessionActorRef::Local(SessionActor::spawn(
        SessionActor::new(
            config.clone(),
            session_id.clone(),
            SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        )
        .with_control_state(initial),
    ));

    fresh
        .set_reasoning_effort(Some(ReasoningEffort::Low))
        .await
        .expect("fresh actor sets effort");
    fresh
        .set_mode(AgentMode::Plan)
        .await
        .expect("fresh actor switches to plan");

    stale
        .set_llm_config(
            LLMParams::new().provider("mock").model("rebased-config"),
            None,
        )
        .await
        .expect("stale actor rebases llm config onto current mode");

    let persisted = config
        .provider
        .history_store()
        .get_session_control(&session_id)
        .await
        .expect("load persisted control")
        .expect("control state exists");
    assert_eq!(persisted.active_mode, AgentMode::Plan);
    assert_eq!(persisted.effective_model.model, "rebased-config");
    assert_eq!(persisted.reasoning_effort, Some(ReasoningEffort::Low));
    let llm = config
        .provider
        .history_store()
        .get_llm_config(persisted.effective_model.llm_config_id)
        .await
        .expect("load llm config")
        .expect("llm config exists");
    let params: LLMParams = llm
        .params
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    assert_eq!(params.reasoning_effort, Some(ReasoningEffort::Low));
}

#[tokio::test]
async fn stale_actor_keeps_explicit_llm_config_effort_after_revision_conflict() {
    let (config, _td) = test_agent_config().await;
    let (fresh, session_id) = spawn_test_session_with_id(config.clone(), "stale-explicit").await;
    let initial = fresh
        .get_session_control()
        .await
        .expect("initialize control state");

    let stale = SessionActorRef::Local(SessionActor::spawn(
        SessionActor::new(
            config.clone(),
            session_id.clone(),
            SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        )
        .with_control_state(initial),
    ));

    fresh
        .set_reasoning_effort(Some(ReasoningEffort::Low))
        .await
        .expect("fresh actor sets effort");
    fresh
        .set_mode(AgentMode::Plan)
        .await
        .expect("fresh actor switches to plan");

    stale
        .set_llm_config(
            LLMParams::new()
                .provider("mock")
                .model("explicit-rebased")
                .reasoning_effort(ReasoningEffort::High),
            None,
        )
        .await
        .expect("stale actor rebases llm config keeping explicit effort");

    let persisted = config
        .provider
        .history_store()
        .get_session_control(&session_id)
        .await
        .expect("load persisted control")
        .expect("control state exists");
    assert_eq!(persisted.active_mode, AgentMode::Plan);
    assert_eq!(persisted.effective_model.model, "explicit-rebased");
    assert_eq!(persisted.reasoning_effort, Some(ReasoningEffort::Low));
    let llm = config
        .provider
        .history_store()
        .get_llm_config(persisted.effective_model.llm_config_id)
        .await
        .expect("load llm config")
        .expect("llm config exists");
    let params: LLMParams = llm
        .params
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    assert_eq!(params.reasoning_effort, Some(ReasoningEffort::High));
}

#[tokio::test]
async fn set_llm_config_keeps_params_effort_when_uncontended() {
    let (config, _td) = test_agent_config().await;
    let (session, session_id) =
        spawn_test_session_with_id(config.clone(), "uncontended-config").await;
    session
        .set_reasoning_effort(Some(ReasoningEffort::Low))
        .await
        .expect("set session effort");

    session
        .set_llm_config(
            LLMParams::new()
                .provider("mock")
                .model("explicit-effort")
                .reasoning_effort(ReasoningEffort::High),
            None,
        )
        .await
        .expect("uncontended llm config keeps params effort");

    let persisted = config
        .provider
        .history_store()
        .get_session_control(&session_id)
        .await
        .expect("load persisted control")
        .expect("control state exists");
    assert_eq!(persisted.reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(persisted.effective_model.model, "explicit-effort");
    let llm = config
        .provider
        .history_store()
        .get_llm_config(persisted.effective_model.llm_config_id)
        .await
        .expect("load llm config")
        .expect("llm config exists");
    let params: LLMParams = llm
        .params
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    assert_eq!(params.reasoning_effort, Some(ReasoningEffort::High));
}

#[tokio::test]
async fn test_local_ref_cancel_idle_session() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-cancel-1").await;
    // Cancel on idle session should succeed (no-op)
    session_ref.cancel().await.expect("cancel on idle session");
}

#[tokio::test]
async fn test_local_ref_shutdown() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-shutdown-1").await;
    session_ref.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn test_local_ref_get_session_limits_none() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-limits-1").await;
    let limits = session_ref
        .get_session_limits()
        .await
        .expect("get_session_limits");
    assert!(limits.is_none(), "no limits configured → None");
}

#[tokio::test]
async fn test_local_ref_get_llm_config() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-llm-1").await;
    let config = session_ref
        .get_llm_config()
        .await
        .expect("get LLM config")
        .expect("test session should have an LLM config");
    assert_eq!(config.provider, "mock");
    assert_eq!(config.model, "mock");
}

#[tokio::test]
async fn test_local_ref_subscribe_unsubscribe_events() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-events-1").await;

    // Subscribe with a dummy relay_actor_id. Subscribing by DHT relay name
    // requires a bootstrapped mesh to resolve the relay actor, so a bare
    // mesh-less local session must fail typed — mirroring
    // session_actor::test_subscribe_unsubscribe_events_no_panic. Success-path
    // coverage lives in the mesh tests.
    #[cfg(feature = "remote")]
    {
        let err = session_ref
            .subscribe_events(42, "event_relay::test-events-1::local".to_string())
            .await
            .expect_err("subscribe_events without a mesh must fail");
        assert!(err.message.contains("mesh not bootstrapped"));
    }
    #[cfg(not(feature = "remote"))]
    session_ref
        .subscribe_events(42, "event_relay::test-events-1::local".to_string())
        .await
        .expect("subscribe_events");
    // Unsubscribe
    session_ref
        .unsubscribe_events(42, "event_relay::test-events-1::local".to_string())
        .await
        .expect("unsubscribe_events");
}

#[tokio::test]
async fn test_local_ref_get_history() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-history-1").await;
    let history = session_ref.get_history().await.expect("get history");
    assert!(history.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  SessionRegistry — CRUD operations
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_registry_initially_empty() {
    let (config, _td) = test_agent_config().await;
    let registry = SessionRegistry::new(config);
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert!(registry.session_ids().is_empty());
}

#[tokio::test]
async fn test_registry_insert_and_get() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config.clone(), "reg-1").await;

    let mut registry = SessionRegistry::new(config);
    registry.insert("reg-1".to_string(), session_ref);

    assert_eq!(registry.len(), 1);
    assert!(!registry.is_empty());
    assert!(registry.get("reg-1").is_some());
    assert!(registry.get("reg-nonexistent").is_none());
}

#[tokio::test]
async fn test_registry_insert_and_remove() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config.clone(), "reg-2").await;

    let mut registry = SessionRegistry::new(config);
    registry.insert("reg-2".to_string(), session_ref);
    assert_eq!(registry.len(), 1);

    let removed = registry.remove("reg-2");
    assert!(removed.is_some());
    assert_eq!(registry.len(), 0);
    assert!(registry.get("reg-2").is_none());
}

#[tokio::test]
async fn test_registry_remove_nonexistent() {
    let (config, _td) = test_agent_config().await;
    let mut registry = SessionRegistry::new(config);
    let removed = registry.remove("does-not-exist");
    assert!(removed.is_none());
}

#[tokio::test]
async fn test_registry_session_ids() {
    let (config, _td) = test_agent_config().await;
    let ref1 = spawn_test_session(config.clone(), "id-a").await;
    let ref2 = spawn_test_session(config.clone(), "id-b").await;
    let ref3 = spawn_test_session(config.clone(), "id-c").await;

    let mut registry = SessionRegistry::new(config);
    registry.insert("id-a".to_string(), ref1);
    registry.insert("id-b".to_string(), ref2);
    registry.insert("id-c".to_string(), ref3);

    let mut ids = registry.session_ids();
    ids.sort();
    assert_eq!(ids, vec!["id-a", "id-b", "id-c"]);
}

#[tokio::test]
async fn test_registry_overwrite() {
    let (config, _td) = test_agent_config().await;
    let ref1 = spawn_test_session(config.clone(), "ow-1").await;
    let ref2 = spawn_test_session(config.clone(), "ow-1-replacement").await;

    let mut registry = SessionRegistry::new(config);
    registry.insert("ow-1".to_string(), ref1);
    assert_eq!(registry.len(), 1);

    // Insert again with same key — should overwrite
    registry.insert("ow-1".to_string(), ref2);
    assert_eq!(registry.len(), 1);

    // The stored ref should still be gettable
    assert!(registry.get("ow-1").is_some());
}

// ═══════════════════════════════════════════════════════════════════════════
//  EventRelayActor — extended tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "remote")]
mod event_relay_extended {
    use super::*;
    use crate::agent::remote::event_relay::{EventRelayActor, RelayedEvent};
    use querymt_agent_services::event_fanout::EventFanout;
    use querymt_agent_services::event_sink::EventSink;

    /// Create an EventSink backed by an in-memory journal.
    async fn make_sink() -> Arc<EventSink> {
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let journal = storage.event_journal();
        let fanout = Arc::new(EventFanout::new());
        Arc::new(EventSink::new(journal, fanout))
    }

    /// Gap recovery must stop before the received event's `session_seq`, so a
    /// repair never publishes host events that come after the event being
    /// delivered.
    #[tokio::test]
    async fn test_gap_recovery_stops_before_received_session_seq() {
        let (host_config, _td) = test_agent_config().await;
        let (host_ref, session_id) =
            spawn_test_session_with_id(host_config.clone(), "gap-bound").await;
        for _ in 0..6 {
            host_config
                .event_sink
                .emit_durable(&session_id, AgentEventKind::Cancelled)
                .await
                .unwrap();
        }
        let host_events = host_config
            .event_sink
            .journal()
            .load_session_stream(&session_id, None, None)
            .await
            .unwrap();
        assert!(host_events.len() >= 6);

        let local_sink = make_sink().await;
        let relay = EventRelayActor::new(
            local_sink.clone(),
            session_id.clone(),
            "host".to_string(),
            Some("node-a".to_string()),
            None,
        )
        .with_gap_source(host_ref);
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        // Deliver the first host event live, then skip ahead to an ephemeral
        // event numbered after host_events[3].
        relay_ref
            .ask(RelayedEvent {
                event: AgentEvent::from(host_events[0].clone()),
            })
            .await
            .unwrap();
        let received_seq = host_events[3].session_seq;
        relay_ref
            .ask(RelayedEvent {
                event: AgentEvent {
                    session_seq: received_seq,
                    seq: 0,
                    timestamp: 1,
                    session_id: session_id.clone(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    kind: AgentEventKind::AssistantContentDelta {
                        content: "t".into(),
                        message_id: "m".into(),
                    },
                },
            })
            .await
            .unwrap();

        let local: Vec<i64> = local_sink
            .journal()
            .load_session_stream(&session_id, None, None)
            .await
            .unwrap()
            .iter()
            .map(|event| event.session_seq)
            .collect();
        let expected: Vec<i64> = host_events[..3].iter().map(|e| e.session_seq).collect();
        assert_eq!(local, expected, "recovery must not reach the received seq");
    }

    #[tokio::test]
    async fn test_relay_multiple_events() {
        let sink = make_sink().await;
        let relay = EventRelayActor::new(
            sink.clone(),
            "s1".to_string(),
            "multi-test".to_string(),
            None,
            None,
        );
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        let mut rx = sink.fanout().subscribe();

        let events = vec![
            AgentEvent {
                session_seq: 0,
                seq: 1,
                timestamp: 100,
                session_id: "s1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::SessionCreated,
            },
            AgentEvent {
                session_seq: 0,
                seq: 2,
                timestamp: 200,
                session_id: "s1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::PromptReceived {
                    content: "hello".to_string(),
                    message_id: None,
                },
            },
            AgentEvent {
                session_seq: 0,
                seq: 3,
                timestamp: 300,
                session_id: "s1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::LlmRequestStart { message_count: 5 },
            },
        ];

        for event in &events {
            relay_ref
                .tell(RelayedEvent {
                    event: event.clone(),
                })
                .await
                .expect("tell should succeed");
        }

        // Receive all 3 events. The relay routes through EventSink which
        // assigns new journal stream_seq values (not the original seq).
        // We verify: session_id preserved, events arrive in order, seq monotonic.
        let mut received_seqs = Vec::new();
        for expected_event in &events {
            let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
                .await
                .expect("should receive within timeout")
                .expect("recv should succeed");

            assert_eq!(received.session_id(), expected_event.session_id);
            if let querymt_agent_core::events::EventEnvelope::Durable(de) = &received {
                assert!(de.stream_seq >= 1, "journal-assigned seq must be >= 1");
                received_seqs.push(de.stream_seq);
            } else {
                panic!("expected durable event envelope");
            }
        }

        // Journal assigns monotonically increasing seqs.
        for window in received_seqs.windows(2) {
            assert!(
                window[0] < window[1],
                "journal stream_seq must be monotonically increasing: {} < {}",
                window[0],
                window[1]
            );
        }
    }

    #[tokio::test]
    async fn test_relay_preserves_session_id() {
        let sink = make_sink().await;
        let relay = EventRelayActor::new(
            sink.clone(),
            "unique-session-id-xyz".to_string(),
            "preserve-test".to_string(),
            None,
            None,
        );
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        let mut rx = sink.fanout().subscribe();

        let event = AgentEvent {
            session_seq: 0,
            seq: 99,
            timestamp: 555,
            session_id: "unique-session-id-xyz".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent {
                event: event.clone(),
            })
            .await
            .expect("tell");

        let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        assert_eq!(received.session_id(), "unique-session-id-xyz");
    }

    #[tokio::test]
    async fn test_relay_preserves_event_kind() {
        let sink = make_sink().await;
        let relay = EventRelayActor::new(
            sink.clone(),
            "s1".to_string(),
            "kind-test".to_string(),
            None,
            None,
        );
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        let mut rx = sink.fanout().subscribe();

        let event = AgentEvent {
            session_seq: 0,
            seq: 7,
            timestamp: 777,
            session_id: "s1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::Error {
                message: "test error".to_string(),
            },
        };

        relay_ref
            .tell(RelayedEvent {
                event: event.clone(),
            })
            .await
            .expect("tell");

        let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        match received.kind() {
            AgentEventKind::Error { message } => {
                assert_eq!(message, "test error");
            }
            other => panic!("expected Error event, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_relay_ignores_mismatched_session() {
        let sink = make_sink().await;
        let relay = EventRelayActor::new(
            sink.clone(),
            "session-a".to_string(),
            "multi-session".to_string(),
            None,
            None,
        );
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        let mut rx = sink.fanout().subscribe();

        let event_a = AgentEvent {
            session_seq: 0,
            seq: 1,
            timestamp: 100,
            session_id: "session-a".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };
        let event_b = AgentEvent {
            session_seq: 0,
            seq: 2,
            timestamp: 200,
            session_id: "session-b".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent {
                event: event_a.clone(),
            })
            .await
            .expect("tell a");
        relay_ref
            .tell(RelayedEvent {
                event: event_b.clone(),
            })
            .await
            .expect("tell b");

        let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");
        assert_eq!(received.session_id(), "session-a");

        let extra = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(extra.is_err(), "mismatched session event should be ignored");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  EventForwarder — stub tests (no remote feature)
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(not(feature = "remote"))]
mod event_forwarder_stub {
    use crate::agent::remote::EventForwarder;

    #[test]
    #[should_panic(expected = "requires the 'remote' feature")]
    fn test_stub_panics_on_start() {
        let fanout = std::sync::Arc::new(querymt_agent_services::event_fanout::EventFanout::new());
        let _ = EventForwarder::start(fanout, (), "test".to_string(), "sess".to_string());
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  RemoteNodeManager — lifecycle tests (requires `remote` feature)
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(all(test, feature = "remote"))]
mod node_manager_tests {
    use super::*;
    use crate::agent::messages::{GetEventStream, GetMode};
    use crate::agent::remote::node_manager::{
        AdmissionRequest, AdmissionResponse, CreateRemoteSchedule, CreateRemoteSession,
        DeleteRemoteSchedule, ForkRemoteSession, GetNodeInfo, ListRemoteSchedules,
        ListRemoteSchedulesResponse, ListRemoteSessions, PauseRemoteSchedule, RemoteNodeManager,
        ResumeRemoteSchedule, ResumeRemoteSession, SessionHandoff, StopRemoteSessionRuntime,
    };
    use crate::agent::remote::test_helpers::fixtures::get_test_mesh;
    use crate::session::domain::IntentSnapshot;
    use kameo::actor::{ActorRef, Spawn};
    use kameo::error::{RemoteSendError, SendError};
    use querymt::chat::ChatRole;
    use querymt_agent_core::model::{AgentMessage, MessagePart};
    use std::time::Duration;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    /// Spawn a `RemoteNodeManager` actor without mesh and return its `ActorRef`.
    async fn spawn_test_node_manager_no_mesh()
    -> (ActorRef<RemoteNodeManager>, Arc<AgentConfig>, TempDir) {
        let (config, td) = test_agent_config().await;
        let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));
        let nm = RemoteNodeManager::new(
            config.clone(),
            registry,
            None,
            Arc::new(parking_lot::Mutex::new(None)),
        );
        let actor_ref = RemoteNodeManager::spawn(nm);
        (actor_ref, config, td)
    }

    /// Spawn a `RemoteNodeManager` actor with mesh enabled and return its `ActorRef`.
    async fn spawn_test_node_manager_with_mesh()
    -> (ActorRef<RemoteNodeManager>, Arc<AgentConfig>, TempDir) {
        let mesh = get_test_mesh().await;
        let (config, td) = test_agent_config().await;
        let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));
        let nm = RemoteNodeManager::new(
            config.clone(),
            registry,
            Some(mesh.clone()),
            Arc::new(parking_lot::Mutex::new(None)),
        );
        let actor_ref = RemoteNodeManager::spawn(nm);
        (actor_ref, config, td)
    }

    #[tokio::test]
    async fn guarded_admission_binds_peer_and_keeps_discovery_identity_only() {
        use kameo::remote::messaging::SwarmResponse;
        use querymt_remote::mesh_ingress::{InboundRouter, MeshIngress};
        use querymt_remote::peer_access::{
            GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState,
        };

        struct EmptyStore;
        #[async_trait::async_trait]
        impl GrantStore for EmptyStore {
            async fn grant(&self, _: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
                Ok(PeerGrantState::default())
            }
            async fn replace_grant(
                &self,
                _: &PeerGrantKey,
                _: Uuid,
                _: PeerGrant,
            ) -> Result<PeerGrantState, GrantStoreError> {
                panic!("admission must not write a grant")
            }
            async fn managed_peers(
                &self,
                _: &querymt_remote::NodeId,
            ) -> Result<Vec<querymt_remote::NodeId>, GrantStoreError> {
                Ok(Vec::new())
            }
        }

        let mesh = get_test_mesh().await;
        let (actor, _config, _td) = spawn_test_node_manager_with_mesh().await;
        let store = Arc::new(EmptyStore);
        let ingress = InboundRouter::new(*mesh.peer_id(), store.clone());
        RemoteNodeManager::register_guarded_discovery(&ingress, store, &actor);
        let invite = mesh
            .create_invite(Some("named-mesh".into()), None, Some(1), false)
            .unwrap();
        let joiner = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let request = AdmissionRequest::Invite {
            invite_id: invite.grant.invite_id,
            mesh_name: invite.grant.mesh_name,
            peer_id: joiner.to_string(),
        };
        let SwarmResponse::Ask(Err(RemoteSendError::HandlerError(bytes))) = ingress
            .handle_legacy(
                libp2p::PeerId::random(),
                node_ask::<RemoteNodeManager, _>(actor.id(), &request),
            )
            .await
        else {
            panic!("expected a caller mismatch")
        };
        assert_eq!(
            rmp_serde::from_slice::<crate::agent::remote::node_manager::AdmissionRejection>(&bytes)
                .unwrap(),
            crate::agent::remote::node_manager::AdmissionRejection::PeerIdMismatch
        );
        let SwarmResponse::Ask(Ok(bytes)) = ingress
            .handle_v1(
                joiner,
                node_ask::<RemoteNodeManager, _>(actor.id(), &request),
            )
            .await
            .unwrap()
        else {
            panic!("the rejected caller must not consume the single-use invite")
        };
        let AdmissionResponse::Admitted {
            membership_token, ..
        } = rmp_serde::from_slice(&bytes).unwrap()
        else {
            panic!("expected admission")
        };
        assert_eq!(membership_token.peer_id, joiner.to_string());
        assert_eq!(
            membership_token.mesh_id,
            crate::agent::remote::invite::mesh_id_for(
                &mesh.peer_id().to_string(),
                Some("named-mesh")
            )
        );
        let SwarmResponse::Ask(Ok(bytes)) = ingress
            .handle_v1(
                joiner,
                node_ask::<RemoteNodeManager, _>(actor.id(), &GetNodeInfo),
            )
            .await
            .unwrap()
        else {
            panic!("admitted peer should discover the host even without a grant")
        };
        let info: crate::agent::remote::NodeInfo = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(info.node_id.as_peer_id(), mesh.peer_id());
        assert!(info.capabilities.is_empty());
        assert_eq!(info.active_sessions, None);
    }

    fn node_ask<A, M>(
        actor_id: kameo::actor::ActorId,
        message: &M,
    ) -> kameo::remote::messaging::SwarmRequest
    where
        A: kameo::remote::RemoteActor + kameo::remote::RemoteMessage<M>,
        M: serde::Serialize,
    {
        kameo::remote::messaging::SwarmRequest::Ask {
            actor_id,
            actor_remote_id: <A as kameo::remote::RemoteActor>::REMOTE_ID.into(),
            message_remote_id: <A as kameo::remote::RemoteMessage<M>>::REMOTE_ID.into(),
            payload: rmp_serde::to_vec_named(message).unwrap(),
            mailbox_timeout: None,
            reply_timeout: None,
            immediate: false,
        }
    }

    #[tokio::test]
    async fn sessions_off_blocks_an_existing_reference() {
        use kameo::remote::messaging::SwarmResponse;
        use querymt_remote::mesh_ingress::{InboundRouter, MeshIngress, SharingRejection};
        use querymt_remote::peer_access::{
            GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState,
        };

        struct Store(PeerGrantKey, parking_lot::Mutex<PeerGrantState>);
        #[async_trait::async_trait]
        impl GrantStore for Store {
            async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
                Ok(if *key == self.0 {
                    self.1.lock().clone()
                } else {
                    PeerGrantState::default()
                })
            }
            async fn replace_grant(
                &self,
                _: &PeerGrantKey,
                _: Uuid,
                _: PeerGrant,
            ) -> Result<PeerGrantState, GrantStoreError> {
                unreachable!("test changes its fixture state directly")
            }
            async fn managed_peers(
                &self,
                _: &querymt_remote::NodeId,
            ) -> Result<Vec<querymt_remote::NodeId>, GrantStoreError> {
                unreachable!("not used by reads")
            }
        }

        let mesh = get_test_mesh().await;
        let (manager, config, _dir) = spawn_test_node_manager_with_mesh().await;
        let workspace = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::new(
            Some(workspace.path().canonicalize().unwrap()),
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );
        let stored = config
            .provider
            .history_store()
            .create_session(Some("retained-reference".into()), None, None, None)
            .await
            .expect("persist fixture session");
        let session_id = stored.public_id;
        let session = SessionActor::spawn(SessionActor::new(config, session_id.clone(), runtime));
        let host = *mesh.peer_id();
        let peer = libp2p::PeerId::random();
        let store = Arc::new(Store(
            PeerGrantKey {
                host: querymt_remote::NodeId::from_peer_id(host),
                peer: querymt_remote::NodeId::from_peer_id(peer),
            },
            parking_lot::Mutex::new(PeerGrantState {
                revision: Uuid::now_v7(),
                grant: PeerGrant::Custom {
                    sessions: true,
                    filesystem: false,
                    providers: vec![],
                },
            }),
        ));
        let ingress = InboundRouter::new(host, store.clone());
        RemoteNodeManager::register_guarded_discovery(&ingress, store.clone(), &manager);
        crate::agent::remote::remote_actor_impl::register_session_history(&ingress, &session);
        crate::agent::remote::remote_actor_impl::register_file_proxy(&ingress, &session);
        let actor = session.id();
        let history = || node_ask::<SessionActor, _>(actor, &crate::agent::messages::GetHistory);
        let listing = || {
            node_ask::<RemoteNodeManager, _>(
                manager.id(),
                &ListRemoteSessions {
                    offset: None,
                    limit: None,
                },
            )
        };
        assert!(matches!(
            ingress.handle_v1(peer, history()).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        assert!(matches!(
            ingress.handle_v1(peer, listing()).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        let read = |path: &str| {
            node_ask::<SessionActor, _>(
                actor,
                &crate::agent::messages::ReadRemoteFile {
                    path: path.to_owned(),
                    offset: 0,
                    limit: 10,
                },
            )
        };
        assert!(matches!(
            ingress.handle_v1(peer, read("example.txt")).await,
            Err(SharingRejection::Denied)
        ));
        assert!(matches!(
            ingress
                .handle_v1(
                    peer,
                    node_ask::<SessionActor, _>(actor, &crate::agent::messages::GetFileIndex)
                )
                .await,
            Err(SharingRejection::Denied)
        ));
        #[cfg(target_os = "linux")]
        {
            use crate::agent::file_proxy::ReadRemoteFileResponse;
            std::fs::write(workspace.path().join("example.txt"), "from workspace\n").unwrap();
            let external = tempfile::NamedTempFile::new().unwrap();
            std::os::unix::fs::symlink(external.path(), workspace.path().join("external.txt"))
                .unwrap();
            *store.1.lock() = PeerGrantState {
                revision: Uuid::now_v7(),
                grant: PeerGrant::Custom {
                    sessions: true,
                    filesystem: true,
                    providers: vec![],
                },
            };
            let Ok(SwarmResponse::Ask(Ok(reply))) =
                ingress.handle_v1(peer, read("example.txt")).await
            else {
                panic!("granted workspace file must be readable")
            };
            assert!(
                matches!(rmp_serde::from_slice::<ReadRemoteFileResponse>(&reply).unwrap(), ReadRemoteFileResponse::Text(text) if text.contains("from workspace"))
            );
            for path in ["external.txt", external.path().to_str().unwrap()] {
                assert!(matches!(
                    ingress.handle_v1(peer, read(path)).await,
                    Ok(SwarmResponse::Ask(Err(RemoteSendError::HandlerError(_))))
                ));
            }
        }
        let resume = || {
            node_ask::<RemoteNodeManager, _>(
                manager.id(),
                &ResumeRemoteSession {
                    session_id: session_id.clone(),
                },
            )
        };
        assert!(matches!(
            ingress.handle_v1(peer, resume()).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        let discover_owner = || async {
            let Ok(SwarmResponse::Ask(Ok(bytes))) = ingress
                .handle_v1(
                    peer,
                    node_ask::<RemoteNodeManager, _>(manager.id(), &GetNodeInfo),
                )
                .await
            else {
                panic!("node discovery must be available independently of session sharing");
            };
            rmp_serde::from_slice::<crate::agent::remote::NodeInfo>(&bytes).unwrap()
        };
        let shared = discover_owner().await;
        assert_eq!(shared.node_id.as_peer_id(), &host);
        assert_eq!(shared.active_sessions, Some(1));
        assert!(
            !shared
                .capabilities
                .iter()
                .any(|capability| capability == "shell")
        );
        let create_schedule = || {
            node_ask::<RemoteNodeManager, _>(
                manager.id(),
                &CreateRemoteSchedule {
                    session_id: session_id.clone(),
                    prompt: "inert scheduled work".into(),
                    trigger: crate::session::domain_schedule::ScheduleTrigger::Interval {
                        seconds: 3600,
                    },
                    max_steps: None,
                    max_cost_usd: None,
                    max_runs: None,
                },
            )
        };
        let list_schedules = || {
            node_ask::<RemoteNodeManager, _>(
                manager.id(),
                &ListRemoteSchedules {
                    session_id: Some(session_id.clone()),
                },
            )
        };
        assert!(matches!(
            ingress.handle_v1(peer, create_schedule()).await,
            Err(SharingRejection::Denied)
        ));
        let revision = Uuid::now_v7();
        *store.1.lock() = PeerGrantState {
            revision,
            grant: PeerGrant::Full,
        };
        assert!(matches!(
            ingress.handle_v1(peer, create_schedule()).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        let Ok(SwarmResponse::Ask(Ok(bytes))) = ingress.handle_v1(peer, list_schedules()).await
        else {
            panic!("peer should see the schedule it created");
        };
        let schedules = rmp_serde::from_slice::<ListRemoteSchedulesResponse>(&bytes)
            .unwrap()
            .schedules;
        use crate::session::domain_schedule::{ScheduleOrigin, SchedulePeerProtocol};
        assert_eq!(schedules.len(), 1);
        assert_eq!(
            schedules[0].config.origin,
            ScheduleOrigin::Peer {
                host: host.to_string(),
                peer: peer.to_string(),
                protocol: SchedulePeerProtocol::V1,
            }
        );
        let pause = || {
            node_ask::<RemoteNodeManager, _>(
                manager.id(),
                &PauseRemoteSchedule {
                    schedule_public_id: schedules[0].public_id.clone(),
                },
            )
        };
        assert!(matches!(
            ingress.handle_v1(peer, pause()).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        // The CAS revision is not the identity of the schedule's owner.
        *store.1.lock() = PeerGrantState {
            revision: Uuid::now_v7(),
            grant: PeerGrant::Full,
        };
        let Ok(SwarmResponse::Ask(Ok(bytes))) = ingress.handle_v1(peer, list_schedules()).await
        else {
            panic!("peer list after a CAS revision change must succeed");
        };
        assert_eq!(
            rmp_serde::from_slice::<ListRemoteSchedulesResponse>(&bytes)
                .unwrap()
                .schedules[0]
                .public_id,
            schedules[0].public_id
        );
        assert!(matches!(
            ingress.handle_v1(peer, pause()).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));

        *store.1.lock() = PeerGrantState {
            revision: Uuid::now_v7(),
            grant: PeerGrant::None,
        };
        assert!(matches!(
            ingress.handle_v1(peer, history()).await,
            Err(SharingRejection::Denied)
        ));
        assert!(matches!(
            ingress.handle_v1(peer, listing()).await,
            Err(SharingRejection::Denied)
        ));
        assert!(matches!(
            ingress.handle_v1(peer, resume()).await,
            Err(SharingRejection::Denied)
        ));
        let denied = discover_owner().await;
        assert_eq!(denied.node_id, shared.node_id);
        assert_eq!(denied.active_sessions, None);
        assert!(denied.capabilities.is_empty());
        assert!(
            session
                .ask(crate::agent::messages::GetHistory)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn guarded_turns_do_not_control_owner_work_and_recheck_on_revocation() {
        use crate::agent::messages::{
            Cancel, DiscardQueuedInput, DiscardQueuedInputResult, GetRuntimeStatus, InputDelivery,
            Prompt, SubmitInput, SubmitInputResult, SubmitSessionInput,
        };
        use crate::agent::remote::remote_actor_impl::register_session_turns;
        use crate::error::AgentError;
        use kameo::remote::messaging::SwarmResponse;
        use querymt_agent_core::events::AgentEventKind;
        use querymt_remote::mesh_ingress::{InboundRouter, MeshIngress, SharingRejection};
        use querymt_remote::peer_access::{
            GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState,
        };

        struct Store(PeerGrantKey, parking_lot::Mutex<PeerGrantState>);
        #[async_trait::async_trait]
        impl GrantStore for Store {
            async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
                Ok(if *key == self.0 {
                    self.1.lock().clone()
                } else {
                    PeerGrantState::default()
                })
            }
            async fn replace_grant(
                &self,
                _: &PeerGrantKey,
                _: Uuid,
                _: PeerGrant,
            ) -> Result<PeerGrantState, GrantStoreError> {
                unreachable!("test changes its fixture state directly")
            }
            async fn managed_peers(
                &self,
                _: &querymt_remote::NodeId,
            ) -> Result<Vec<querymt_remote::NodeId>, GrantStoreError> {
                unreachable!("unused by turn routes")
            }
        }

        let mesh = get_test_mesh().await;
        let (config, _dir) = test_agent_config().await;
        let history = config.provider.history_store().clone();
        let id = history
            .create_session(Some("turn-revocation".into()), None, None, None)
            .await
            .unwrap()
            .public_id;
        let runtime = SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );
        // Keep the production execution future waiting before any history or provider IO.
        let _permit = runtime
            .execution_permit
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let session = SessionActor::spawn(SessionActor::new(config.clone(), id.clone(), runtime));
        let host = *mesh.peer_id();
        let peer = libp2p::PeerId::random();
        let store = Arc::new(Store(
            PeerGrantKey {
                host: querymt_remote::NodeId::from_peer_id(host),
                peer: querymt_remote::NodeId::from_peer_id(peer),
            },
            parking_lot::Mutex::new(PeerGrantState::default()),
        ));
        let set_grant = |grant| {
            *store.1.lock() = PeerGrantState {
                grant,
                revision: Uuid::now_v7(),
            }
        };
        let ingress = InboundRouter::new(host, store.clone());
        register_session_turns(&ingress, store.clone(), &session);
        let input = |delivery, expected_run_id| {
            SubmitSessionInput::new(SubmitInput {
                session_id: id.clone(),
                client_input_id: Some("same-key".into()),
                expected_run_id,
                delivery,
                prompt: vec!["inert prompt".into()],
            })
        };
        let queue =
            || node_ask::<SessionActor, _>(session.id(), &input(InputDelivery::Queue, None));
        let read_receipt = |response| {
            let SwarmResponse::Ask(Ok(bytes)) = response else {
                panic!("expected successful turn receipt: {response:?}");
            };
            rmp_serde::from_slice::<SubmitInputResult>(&bytes).unwrap()
        };
        let denied = |response| {
            let SwarmResponse::Ask(Err(RemoteSendError::HandlerError(bytes))) = response else {
                panic!("expected actor denial: {response:?}");
            };
            assert!(matches!(
                rmp_serde::from_slice::<AgentError>(&bytes).unwrap(),
                AgentError::MeshAccessDenied
            ));
        };
        let wait_idle = || async {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if session
                        .ask(GetRuntimeStatus)
                        .await
                        .unwrap()
                        .active_run_id
                        .is_none()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("run must stop after cancellation");
        };

        set_grant(PeerGrant::Custom {
            sessions: false,
            filesystem: false,
            providers: vec![],
        });
        assert!(matches!(
            ingress.handle_v1(peer, queue()).await,
            Err(SharingRejection::Denied)
        ));
        set_grant(PeerGrant::Full);
        let legacy_cancel = kameo::remote::messaging::SwarmRequest::Tell {
            actor_id: session.id(),
            actor_remote_id: "querymt::SessionActor".into(),
            message_remote_id: "querymt::Cancel".into(),
            payload: vec![0x90], // Legacy MessagePack unit struct.
            mailbox_timeout: None,
            immediate: false,
        };
        assert!(matches!(
            ingress.handle_legacy(peer, legacy_cancel).await,
            SwarmResponse::Tell(Ok(()))
        ));

        let mut prompt = Prompt::new(crate::acp::protocol::PromptRequest::new(id.clone(), vec![]));
        let legacy_payload = rmp_serde::to_vec_named(&prompt).unwrap();
        prompt.authority = crate::agent::session_actor::PeerTurn::test_authority(
            store.1.lock().grant.clone(),
            store.clone(),
        );
        assert_eq!(rmp_serde::to_vec_named(&prompt).unwrap(), legacy_payload);
        let decoded: Prompt = rmp_serde::from_slice(&legacy_payload).unwrap();
        assert!(matches!(
            decoded.authority,
            crate::agent::session_actor::TurnAuthority::Local
        ));
        let response = ingress
            .handle_v1(peer, node_ask::<SessionActor, _>(session.id(), &prompt))
            .await;
        let Ok(SwarmResponse::Ask(Err(RemoteSendError::HandlerError(bytes)))) = response else {
            panic!("expected Prompt to return a typed handler error: {response:?}");
        };
        assert!(matches!(
            rmp_serde::from_slice::<AgentError>(&bytes).unwrap(),
            AgentError::InvalidPromptContent { .. }
        ));

        let SubmitInputResult::Started {
            run_id: owner_run, ..
        } = session
            .ask(input(InputDelivery::Queue, None))
            .await
            .unwrap()
        else {
            panic!("owner prompt must start");
        };
        let status_request = || node_ask::<SessionActor, _>(session.id(), &GetRuntimeStatus);
        let Ok(SwarmResponse::Ask(Ok(bytes))) = ingress.handle_v1(peer, status_request()).await
        else {
            panic!("peer should be able to observe the owner's active run");
        };
        let status =
            rmp_serde::from_slice::<crate::agent::messages::SessionRuntimeStatus>(&bytes).unwrap();
        assert_eq!(status.active_run_id.as_deref(), Some(owner_run.as_str()));
        assert!(!status.steerable);
        assert!(session.ask(GetRuntimeStatus).await.unwrap().steerable);
        let steer = input(InputDelivery::Steer, Some(owner_run.clone()));
        denied(
            ingress
                .handle_v1(peer, node_ask::<SessionActor, _>(session.id(), &steer))
                .await
                .unwrap(),
        );
        denied(
            ingress
                .handle_v1(peer, node_ask::<SessionActor, _>(session.id(), &Cancel))
                .await
                .unwrap(),
        );

        // Same key as the owner must not return the owner's receipt.
        let first = read_receipt(ingress.handle_v1(peer, queue()).await.unwrap());
        let SubmitInputResult::Queued {
            input_id: peer_input,
            ..
        } = first
        else {
            panic!("peer input must queue");
        };
        assert_ne!(peer_input, "same-key");
        assert!(
            matches!(read_receipt(ingress.handle_v1(peer, queue()).await.unwrap()),
            SubmitInputResult::Queued { input_id, .. } if input_id == peer_input)
        );
        let owner_queue = session
            .ask(SubmitSessionInput::new(SubmitInput {
                client_input_id: Some("owner-pending".into()),
                ..input(InputDelivery::Queue, None).input
            }))
            .await
            .unwrap();
        assert!(matches!(owner_queue, SubmitInputResult::Queued { .. }));
        let discard = DiscardQueuedInput::new("owner-pending".into());
        let result = ingress
            .handle_v1(peer, node_ask::<SessionActor, _>(session.id(), &discard))
            .await
            .unwrap();
        let SwarmResponse::Ask(Ok(bytes)) = result else {
            panic!("expected discard receipt");
        };
        assert!(matches!(
            rmp_serde::from_slice::<DiscardQueuedInputResult>(&bytes).unwrap(),
            DiscardQueuedInputResult::NotPending { .. }
        ));
        session.ask(discard).await.unwrap();

        set_grant(PeerGrant::None);
        assert_eq!(
            session
                .ask(GetRuntimeStatus)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(owner_run.as_str())
        );
        session.tell(Cancel).await.unwrap();
        wait_idle().await;
        let stream = config.event_sink.session_stream(&id);
        stream.flush().await;
        let events = config
            .event_sink
            .journal()
            .load_session_stream(&id, None, None)
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(&event.kind,
            AgentEventKind::QueuedInputDiscarded { input_id, reason }
                if input_id == &peer_input && reason == "mesh_access_denied")));
        assert!(!events.iter().any(|event| matches!(&event.kind,
            AgentEventKind::QueuedInputStarted { input_id, .. } if input_id == &peer_input)));

        // The legacy admission path must also retain its authenticated origin.
        let mut live = config.subscribe_events();
        set_grant(PeerGrant::Full);
        let SubmitInputResult::Started {
            run_id: peer_run, ..
        } = read_receipt(ingress.handle_legacy(peer, queue()).await)
        else {
            panic!("legacy Full peer prompt must start");
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if matches!(
                    *live.recv().await.unwrap().kind(),
                    AgentEventKind::SessionQueued { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("legacy turn must reach the execution permit before revocation");
        set_grant(PeerGrant::None);
        wait_idle().await;
        stream.flush().await;
        let events = config
            .event_sink
            .journal()
            .load_session_stream(&id, None, None)
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(&event.kind,
            AgentEventKind::RunCompleted { run_id, outcome, .. } if run_id == &peer_run && outcome == "cancelled")));
        assert!(history.get_history(&id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn revocation_stops_session_relay_without_stopping_local_observers() {
        use crate::agent::messages::SubscribeEvents;
        use crate::agent::remote::EventRelayActor;
        use crate::agent::remote::remote_actor_impl::register_session_subscriptions;
        use kameo::remote::messaging::SwarmResponse;
        use querymt_remote::mesh_ingress::{InboundRouter, MeshIngress};
        use querymt_remote::peer_access::{
            GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState,
        };

        use querymt_remote::peer_access::{PolicyChange, PolicyEffect};
        use tokio::sync::broadcast;

        struct Store(
            parking_lot::Mutex<PeerGrantState>,
            broadcast::Sender<PolicyChange>,
        );
        #[async_trait::async_trait]
        impl GrantStore for Store {
            async fn grant(&self, _: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
                Ok(self.0.lock().clone())
            }
            fn subscribe_changes(&self) -> Option<broadcast::Receiver<PolicyChange>> {
                Some(self.1.subscribe())
            }
            async fn replace_grant(
                &self,
                key: &PeerGrantKey,
                _: Uuid,
                grant: PeerGrant,
            ) -> Result<PeerGrantState, GrantStoreError> {
                let state = PeerGrantState {
                    grant: grant.clone(),
                    revision: Uuid::now_v7(),
                };
                *self.0.lock() = state.clone();
                self.1
                    .send(PolicyChange {
                        key: key.clone(),
                        effect: PolicyEffect::Grant(grant),
                    })
                    .unwrap();
                Ok(state)
            }
            async fn managed_peers(
                &self,
                _: &querymt_remote::NodeId,
            ) -> Result<Vec<querymt_remote::NodeId>, GrantStoreError> {
                unreachable!("unused by subscriptions")
            }
        }
        let mesh = get_test_mesh().await;
        let (config, _dir) = test_agent_config().await;
        let (SessionActorRef::Local(session), id) =
            spawn_test_session_with_id(config.clone(), "relay-revocation").await
        else {
            panic!("expected local fixture")
        };
        let (viewer, _viewer_dir) = test_agent_config().await;
        let relay = EventRelayActor::spawn(EventRelayActor::new(
            viewer.event_sink.clone(),
            id.clone(),
            "test relay".into(),
            None,
            None,
        ));
        let mut observed = viewer.event_sink.fanout().subscribe();
        let mut local = config.event_sink.fanout().subscribe();
        let store = Arc::new(Store(
            parking_lot::Mutex::new(PeerGrantState {
                grant: PeerGrant::Full,
                revision: Uuid::now_v7(),
            }),
            broadcast::channel(8).0,
        ));
        let host = *mesh.peer_id();
        let ingress = InboundRouter::new(host, store.clone());
        register_session_subscriptions(&ingress, mesh, store.clone(), &session);
        let request = SubscribeEvents {
            relay_actor_id: relay.id().sequence_id(),
            relay_ref: Some(relay.into_remote_ref().await),
            relay_dht_name: Some("relay-fixture".into()),
            after_source_seq: None,
        };
        let subscribe = || node_ask::<SessionActor, _>(session.id(), &request);
        let stranger = libp2p::PeerId::random();
        assert!(matches!(
            ingress.handle_legacy(stranger, subscribe()).await,
            SwarmResponse::Ask(Err(RemoteSendError::HandlerError(_)))
        ));
        // The receive guard and the real legacy relay actor run in one process here.
        // The transport-authenticated caller is supplied at the ingress boundary.
        assert!(matches!(
            ingress.handle_legacy(host, subscribe()).await,
            SwarmResponse::Ask(Ok(_))
        ));
        let publish = |name: &'static str, seq| {
            config
                .event_sink
                .fanout()
                .publish(querymt_agent_core::events::EventEnvelope::Durable(
                    querymt_agent_core::events::DurableEvent {
                        event_id: name.into(),
                        session_id: id.clone(),
                        stream_seq: seq,
                        session_seq: 0,
                        timestamp: 100,
                        source_node: None,
                        origin: querymt_agent_core::events::EventOrigin::Local,
                        kind: querymt_agent_core::events::AgentEventKind::SessionCreated,
                    },
                ))
        };
        publish("before", 1);
        tokio::time::timeout(Duration::from_secs(3), observed.recv())
            .await
            .unwrap()
            .unwrap();
        local.recv().await.unwrap();
        let node = querymt_remote::NodeId::from_peer_id(host);
        store
            .replace_grant(
                &PeerGrantKey {
                    host: node.clone(),
                    peer: node,
                },
                Uuid::nil(),
                PeerGrant::None,
            )
            .await
            .unwrap();
        publish("after", 2);
        tokio::time::timeout(Duration::from_secs(3), local.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), observed.recv())
                .await
                .is_err()
        );
        assert!(session.is_alive());
    }

    #[tokio::test]
    async fn v1_subscription_survives_quota_edits_but_not_revocation() {
        use crate::agent::messages::{
            EventStreamPage, GetEventStreamSince, SubscribeEvents, UnsubscribeEvents,
        };
        use crate::agent::remote::EventRelayActor;
        use crate::agent::remote::event_relay::RelayLagged;
        use crate::agent::remote::remote_actor_impl::{
            register_session_relay, register_session_routes,
        };
        use crate::error::AgentError;
        use kameo::remote::messaging::SwarmResponse;
        use querymt_remote::NodeId;
        use querymt_remote::mesh_ingress::{MeshIngress, SharingRejection};
        use querymt_remote::peer_access::{PeerGrant, PeerGrantKey};

        let (config, dir) = test_agent_config().await;
        let owner_mesh = bootstrap_guarded_lan(&dir).await;
        let ingress = owner_mesh.inbound_router().unwrap();
        let store = owner_mesh.quota_store().unwrap();
        // Kameo exports fixture actors with the single process-local peer ID.
        let peer = *owner_mesh.peer_id();
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(*owner_mesh.peer_id()),
            peer: NodeId::from_peer_id(peer),
        };
        let grant = PeerGrant::Custom {
            sessions: true,
            filesystem: false,
            providers: vec![],
        };
        let initial = store.grant(&key).await.unwrap();
        let allowed = store
            .replace_grant(&key, initial.revision, grant.clone())
            .await
            .unwrap();
        let (SessionActorRef::Local(session), id) =
            spawn_test_session_with_id(config.clone(), "revision-bound-relay").await
        else {
            panic!("expected local fixture");
        };
        register_session_routes(&owner_mesh, &session);
        let (viewer, _viewer_dir) = test_agent_config().await;
        let relay = EventRelayActor::spawn(
            EventRelayActor::new(
                viewer.event_sink.clone(),
                id.clone(),
                "test relay".into(),
                Some(peer.to_string()),
                None,
            )
            .with_gap_source(SessionActorRef::Local(session.clone())),
        );
        register_session_relay(ingress, &relay, peer, &id);
        let relay_ref = relay.into_remote_ref().await;
        let relay_actor_id = relay_ref.id().sequence_id();
        let relay_name = "revision-bound-relay";
        let subscribe = || {
            node_ask::<SessionActor, _>(
                session.id(),
                &SubscribeEvents {
                    relay_actor_id,
                    relay_ref: Some(relay_ref.clone()),
                    relay_dht_name: Some(relay_name.into()),
                    after_source_seq: None,
                },
            )
        };
        let unsubscribe = |name: &str| {
            node_ask::<SessionActor, _>(
                session.id(),
                &UnsubscribeEvents {
                    relay_actor_id,
                    relay_dht_name: name.into(),
                },
            )
        };
        let replay = || {
            node_ask::<SessionActor, _>(
                session.id(),
                &GetEventStreamSince {
                    after_source_seq: None,
                    limit: 10,
                },
            )
        };
        let fanout = config.event_sink.fanout();
        let baseline = fanout.subscriber_count();
        let wait_for_retirement = || async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while fanout.subscriber_count() != baseline {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("forwarder did not release its subscription");
        };

        assert!(matches!(
            ingress.handle_v1(peer, subscribe()).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        assert!(matches!(
            ingress.handle_v1(peer, unsubscribe("other-name")).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        assert_eq!(fanout.subscriber_count(), baseline + 1);
        assert!(matches!(
            ingress.handle_v1(peer, unsubscribe(relay_name)).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        wait_for_retirement().await;

        assert!(matches!(
            ingress.handle_v1(peer, subscribe()).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        use querymt_remote::peer_access::quota::{QuotaEdit, QuotaLimit, QuotaScope, QuotaWindow};
        let revision = store
            .edit_quota(
                &key,
                allowed.revision,
                &QuotaScope {
                    provider_id: "mock".into(),
                    model_id: None,
                },
                QuotaEdit::Reconfigure(QuotaLimit::Finite {
                    tokens: 20,
                    window: QuotaWindow::Manual,
                }),
                time::OffsetDateTime::now_utc().unix_timestamp(),
            )
            .await
            .unwrap();
        // This fixture uses a process-local relay identity; measure the host's
        // held subscription rather than requiring the LAN transport to dial itself.
        assert!(
            tokio::time::timeout(Duration::from_millis(350), wait_for_retirement())
                .await
                .is_err()
        );
        assert_eq!(fanout.subscriber_count(), baseline + 1);
        let revoked = store
            .replace_grant(&key, revision, PeerGrant::None)
            .await
            .unwrap();
        assert!(matches!(
            ingress.handle_v1(peer, replay()).await,
            Err(SharingRejection::Denied)
        ));
        store
            .replace_grant(&key, revoked.revision, grant)
            .await
            .unwrap();
        wait_for_retirement().await;

        config
            .emit_event_persisted(&id, AgentEventKind::SessionCreated)
            .await
            .unwrap();
        let Ok(SwarmResponse::Ask(Ok(bytes))) = ingress.handle_v1(peer, replay()).await else {
            panic!("a fresh history request must use the current grant");
        };
        let page: EventStreamPage = rmp_serde::from_slice(&bytes).unwrap();
        let [event] = page.events.as_slice() else {
            panic!("expected the durable event written after regrant");
        };
        assert_eq!(event.session_id, id);
        assert!(matches!(event.kind, AgentEventKind::SessionCreated));

        // A lag notice is an owner-to-viewer callback, not a session-grant RPC.
        // The relay still requires both the attached owner and the attached session.
        let lag = |session_id: &str| {
            node_ask::<EventRelayActor, _>(
                relay.id(),
                &RelayLagged {
                    session_id: session_id.into(),
                    skipped: 1,
                },
            )
        };
        for (sender, session_id) in [(libp2p::PeerId::random(), id.as_str()), (peer, "other")] {
            let Ok(SwarmResponse::Ask(Err(RemoteSendError::HandlerError(error)))) =
                ingress.handle_v1(sender, lag(session_id)).await
            else {
                panic!("lag notice did not match the attachment");
            };
            assert!(matches!(
                rmp_serde::from_slice::<AgentError>(&error).unwrap(),
                AgentError::MeshAccessDenied
            ));
        }
        let mut events = viewer.event_sink.fanout().subscribe();
        assert!(matches!(
            ingress.handle_v1(peer, lag(&id)).await,
            Ok(SwarmResponse::Ask(Ok(_)))
        ));
        let recovered = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("lag notice should recover a durable tail without another live event")
            .unwrap();
        assert_eq!(recovered.session_id(), id);
        assert!(matches!(recovered.kind(), AgentEventKind::SessionCreated));
        assert_eq!(
            viewer
                .event_sink
                .journal()
                .latest_source_seq(&id, &peer.to_string())
                .await
                .unwrap(),
            Some(event.seq)
        );
        owner_mesh.shutdown().await;
    }

    #[tokio::test]
    async fn test_admission_readmits_valid_member_but_rejects_other_scopes_and_bad_signatures() {
        use querymt_remote::{InvitePermissions, MembershipToken, MeshScopeId};
        let mesh = get_test_mesh().await;
        let (config, _td) = test_agent_config().await;
        let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));
        let nm = RemoteNodeManager::spawn(RemoteNodeManager::new(
            config,
            registry,
            Some(mesh.clone()),
            Arc::new(parking_lot::Mutex::new(None)),
        ));
        let name = format!("admission-{}", Uuid::now_v7());
        let invite = mesh
            .create_invite(Some(name), None, Some(1), false)
            .unwrap();
        let subject = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_string();
        let response = nm
            .ask(AdmissionRequest::Invite {
                invite_id: invite.grant.invite_id.clone(),
                mesh_name: invite.grant.mesh_name.clone(),
                peer_id: subject.clone(),
            })
            .await
            .unwrap();
        let AdmissionResponse::Admitted {
            membership_token, ..
        } = response
        else {
            panic!("expected admission");
        };
        let response = nm
            .ask(AdmissionRequest::Token {
                membership_token: membership_token.clone(),
                peer_id: subject.clone(),
            })
            .await
            .unwrap();
        assert!(matches!(response, AdmissionResponse::Readmitted { .. }));

        let foreign_scope = format!("foreign-{}", Uuid::now_v7());
        let foreign = MembershipToken::issue(
            foreign_scope.clone(),
            &subject,
            mesh.keypair(),
            "foreign".into(),
            InvitePermissions::default(),
            0,
        )
        .unwrap();
        let response = nm
            .ask(AdmissionRequest::Token {
                membership_token: foreign,
                peer_id: subject.clone(),
            })
            .await;
        assert!(
            matches!(
                response,
                Err(kameo::error::SendError::HandlerError(
                    crate::agent::remote::node_manager::AdmissionRejection::Denied(_)
                        | crate::agent::remote::node_manager::AdmissionRejection::NotAMember
                ))
            ),
            "{response:?}"
        );
        assert!(!mesh.active_scopes().contains(&MeshScopeId::Iroh {
            mesh_id: foreign_scope
        }));

        let mut invalid = membership_token;
        invalid.signature.clear();
        let response = nm
            .ask(AdmissionRequest::Token {
                membership_token: invalid,
                peer_id: subject,
            })
            .await;
        assert!(
            matches!(
                response,
                Err(kameo::error::SendError::HandlerError(
                    crate::agent::remote::node_manager::AdmissionRejection::Denied(_)
                ))
            ),
            "{response:?}"
        );
    }

    #[tokio::test]
    async fn test_create_session_no_cwd() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create session should succeed");

        assert!(
            !resp.session_id.is_empty(),
            "session_id should not be empty"
        );
        assert!(resp.created_at >= 0, "created_at should be populated");
        assert!(matches!(resp.handoff, SessionHandoff::DirectRemote { .. }));
    }

    #[tokio::test]
    async fn test_create_session_no_mesh_never_claims_lookup_fallback() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_no_mesh().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create session without mesh should succeed");

        assert!(
            matches!(
                resp.handoff,
                SessionHandoff::DirectRemote { .. } | SessionHandoff::NoAttachPath
            ),
            "no-mesh environments must not advertise lookup-only handoff"
        );

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;
        assert!(sessions.iter().any(|s| s.session_id == resp.session_id));
    }

    #[tokio::test]
    async fn test_create_session_valid_cwd() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;
        let cwd_dir = TempDir::new().unwrap();

        let resp = nm_ref
            .ask(CreateRemoteSession {
                cwd: Some(cwd_dir.path().to_string_lossy().to_string()),
            })
            .await
            .expect("create session with valid cwd should succeed");

        assert!(!resp.session_id.is_empty());
    }

    #[tokio::test]
    async fn test_create_session_nonexistent_cwd() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let resp = nm_ref
            .ask(CreateRemoteSession {
                cwd: Some("/this/path/does/not/exist/anywhere".to_string()),
            })
            .await
            .expect("create session with nonexistent cwd should succeed (workspace index skipped)");

        assert!(!resp.session_id.is_empty());
    }

    #[tokio::test]
    async fn test_create_session_relative_cwd_rejected() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let result = nm_ref
            .ask(CreateRemoteSession {
                cwd: Some("relative/path".to_string()),
            })
            .await;

        match result {
            Err(SendError::HandlerError(err)) => {
                assert!(
                    err.to_string().contains("absolute"),
                    "error should mention absolute path: {:?}",
                    err
                );
            }
            Ok(_) => panic!("relative cwd should be rejected, but got Ok"),
            Err(e) => panic!("unexpected error variant: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_list_sessions_empty() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list should succeed")
            .sessions;

        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_list_sessions_after_create() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, resp.session_id);
        assert!(!sessions[0].peer_label.is_empty());
    }

    #[tokio::test]
    async fn test_remote_schedule_create_rejects_missing_peer_origin() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let session = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create session");

        let result = nm_ref
            .ask(CreateRemoteSchedule {
                session_id: session.session_id.clone(),
                prompt: "run every minute".to_string(),
                trigger: crate::session::domain_schedule::ScheduleTrigger::Interval { seconds: 60 },
                max_steps: Some(5),
                max_cost_usd: Some(1.5),
                max_runs: Some(3),
            })
            .await;

        match result {
            Err(SendError::HandlerError(err)) => {
                let message = err.to_string();
                assert!(
                    message.contains("requires an authenticated peer origin"),
                    "missing origin should be reported at creation: {message}"
                );
            }
            Ok(_) => panic!("expected missing peer origin to reject schedule creation"),
            Err(e) => panic!("unexpected error variant: {e:?}"),
        }
        assert!(
            nm_ref
                .ask(ListRemoteSchedules {
                    session_id: Some(session.session_id),
                })
                .await
                .unwrap()
                .schedules
                .is_empty()
        );
    }

    async fn bootstrap_guarded_lan(dir: &TempDir) -> querymt_remote::MeshHandle {
        use querymt_remote::{LanDiscovery, LanMeshConfig, MeshRuntimeConfig};

        let store = Arc::new(
            querymt_agent_sqlite::mesh_access::SqliteMeshAccess::open_exclusive(
                dir.path().join("guarded-host.access.sqlite3"),
            )
            .await
            .unwrap(),
        );
        let config = MeshRuntimeConfig {
            enabled: true,
            lan: Some(LanMeshConfig {
                listen: Some("/ip4/127.0.0.1/tcp/0".into()),
                discovery: LanDiscovery::None,
                directory: Default::default(),
            }),
            iroh_enabled: false,
            iroh_gso: true,
            iroh_dns: None,
            iroh_scopes: Vec::new(),
            identity_file: Some(dir.path().join("guarded-host.key")),
            request_timeout: Duration::from_secs(3),
            stream_reconnect_grace: Duration::from_secs(3),
            node_name: None,
            peers: Vec::new(),
            auto_fallback: false,
        };
        crate::agent::remote::mesh::bootstrap_mesh_runtime(&config, store)
            .await
            .unwrap()
            .as_mesh_handle()
            .clone()
    }

    #[tokio::test]
    async fn due_peer_schedule_waits_for_mesh_after_scheduler_restart() {
        verify_schedule_after_scheduler_restart(true).await;
    }

    #[tokio::test]
    async fn local_schedule_fires_after_scheduler_restart_without_mesh() {
        verify_schedule_after_scheduler_restart(false).await;
    }

    async fn verify_schedule_after_scheduler_restart(peer_origin: bool) {
        use crate::scheduler::{SchedulerActor, SchedulerConfig};
        use crate::session::domain_schedule::{
            ScheduleOrigin, SchedulePeerProtocol, ScheduleState,
        };
        use querymt_remote::peer_access::{PeerGrant, PeerGrantKey};

        let (config, dir) = test_agent_config().await;
        let mesh = bootstrap_guarded_lan(&dir).await;
        let handle = crate::agent::LocalAgentHandle::from_config(config.clone());
        let refs = crate::agent::remote::remote_setup::spawn_and_register_local_mesh_actors(
            &handle, &mesh,
        )
        .await;
        let session_id = refs
            .node_manager
            .ask(CreateRemoteSession { cwd: None })
            .await
            .unwrap()
            .session_id;
        let id = handle
            .create_scheduled_task(
                &session_id,
                "scheduled restart fixture",
                crate::session::domain_schedule::ScheduleTrigger::Interval { seconds: 3600 },
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let original_scheduler = handle.scheduler_handle.lock().take().unwrap();
        original_scheduler.shutdown().await;

        let store = mesh.quota_store().unwrap();
        let key = PeerGrantKey {
            host: querymt_remote::NodeId::from_peer_id(*mesh.peer_id()),
            peer: querymt_remote::NodeId::from_peer_id(libp2p::PeerId::random()),
        };
        let grant = store
            .replace_grant(
                &key,
                store.grant(&key).await.unwrap().revision,
                PeerGrant::Full,
            )
            .await
            .unwrap();
        let repo = config.schedule_repository.as_ref().unwrap().clone();
        let mut schedule = repo.get_schedule(&id).await.unwrap().unwrap();
        assert_eq!(schedule.config.origin, ScheduleOrigin::Local);
        if peer_origin {
            schedule.config.origin = ScheduleOrigin::Peer {
                host: key.host.to_string(),
                peer: key.peer.to_string(),
                protocol: SchedulePeerProtocol::V1,
            };
            repo.update_schedule(schedule).await.unwrap();
            use querymt_remote::peer_access::quota::{
                QuotaEdit, QuotaLimit, QuotaScope, QuotaWindow,
            };
            let scope = QuotaScope {
                provider_id: "mock".into(),
                model_id: None,
            };
            let revision = store
                .edit_quota(
                    &key,
                    grant.revision,
                    &scope,
                    QuotaEdit::Reconfigure(QuotaLimit::Finite {
                        tokens: 100,
                        window: QuotaWindow::Manual,
                    }),
                    time::OffsetDateTime::now_utc().unix_timestamp(),
                )
                .await
                .unwrap();
            assert_ne!(revision, grant.revision);
        }
        config.provider.set_mesh(None);

        let scheduler = SchedulerActor::spawn(
            repo.clone(),
            config.provider.history_store(),
            handle.registry.clone(),
            config.clone(),
            SchedulerConfig::default(),
        )
        .await
        .unwrap();
        let mut events = config.subscribe_events();
        scheduler.trigger_now(&id).await.unwrap();
        if peer_origin {
            assert_eq!(scheduler.metrics().await.fires_total, 0);
            assert_eq!(
                repo.get_schedule(&id).await.unwrap().unwrap().state,
                ScheduleState::Armed
            );
            config.provider.set_mesh(Some(mesh.clone()));
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.unwrap().kind() {
                    AgentEventKind::ScheduledExecutionCompleted {
                        schedule_public_id, ..
                    } if schedule_public_id == &id => break,
                    AgentEventKind::ScheduledExecutionFailed {
                        schedule_public_id,
                        error,
                        ..
                    } if schedule_public_id == &id => panic!("{error}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("deferred schedule should fire without a second trigger");
        scheduler.shutdown().await;
        mesh.shutdown().await;
    }

    #[tokio::test]
    async fn held_scheduled_turn_survives_quota_edit_but_not_revocation() {
        use crate::agent::messages::{GetRuntimeStatus, ScheduledPrompt};
        use crate::error::AgentError;
        use crate::session::domain_schedule::{ScheduleOrigin, SchedulePeerProtocol};
        use querymt_remote::NodeId;
        use querymt_remote::peer_access::{PeerGrant, PeerGrantKey};

        let (config, dir) = test_agent_config().await;
        let mesh = bootstrap_guarded_lan(&dir).await;
        let store = mesh.quota_store().unwrap();
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(*mesh.peer_id()),
            peer: NodeId::from_peer_id(libp2p::PeerId::random()),
        };
        let custom = PeerGrant::Custom {
            sessions: true,
            filesystem: false,
            providers: vec![querymt_remote::peer_access::ProviderGrant {
                provider_id: "mock".into(),
                models: querymt_remote::peer_access::ModelSelection::All,
            }],
        };
        let initial = store.grant(&key).await.unwrap();
        let granted = store
            .replace_grant(&key, initial.revision, custom.clone())
            .await
            .unwrap();
        let history = config.provider.history_store();
        let sid = config
            .provider
            .create_session(None, None, &Default::default())
            .await
            .unwrap()
            .session()
            .public_id
            .clone();
        let runtime = SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );
        let permit = runtime
            .execution_permit
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let actor = SessionActor::spawn(
            SessionActor::new(config.clone(), sid.clone(), runtime).with_mesh(Some(mesh.clone())),
        );
        let origin = |host, peer, protocol| ScheduleOrigin::Peer {
            host,
            peer,
            protocol,
        };
        let message = |origin| ScheduledPrompt {
            schedule_public_id: "fixture-schedule".into(),
            task_public_id: "fixture-task".into(),
            prompt_text: "inert scheduled prompt".into(),
            execution_limits: None,
            origin,
        };
        let denied = |reply| {
            assert!(matches!(
                reply,
                Err(SendError::HandlerError(AgentError::MeshAccessDenied))
            ))
        };
        for rejected in [
            origin(
                libp2p::PeerId::random().to_string(),
                key.peer.to_string(),
                SchedulePeerProtocol::V1,
            ),
            origin(
                key.host.to_string(),
                "invalid peer ID".into(),
                SchedulePeerProtocol::V1,
            ),
            origin(
                key.host.to_string(),
                key.peer.to_string(),
                SchedulePeerProtocol::Legacy,
            ),
        ] {
            denied(actor.ask(message(rejected)).await);
        }
        assert!(
            actor
                .ask(GetRuntimeStatus)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
        let saved = origin(
            key.host.to_string(),
            key.peer.to_string(),
            SchedulePeerProtocol::V1,
        );
        let start = actor.clone();
        let running = tokio::spawn(async move { start.ask(message(saved)).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while actor
                .ask(GetRuntimeStatus)
                .await
                .unwrap()
                .active_run_id
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("current V1 schedule must begin under its peer authority");
        use querymt_remote::peer_access::quota::{QuotaEdit, QuotaLimit, QuotaScope, QuotaWindow};
        let revision = store
            .edit_quota(
                &key,
                granted.revision,
                &QuotaScope {
                    provider_id: "mock".into(),
                    model_id: None,
                },
                QuotaEdit::Reconfigure(QuotaLimit::Finite {
                    tokens: 20,
                    window: QuotaWindow::Manual,
                }),
                time::OffsetDateTime::now_utc().unix_timestamp(),
            )
            .await
            .unwrap();
        let mut running = running;
        assert!(
            tokio::time::timeout(Duration::from_millis(350), &mut running)
                .await
                .is_err()
        );
        let revoked = store
            .replace_grant(&key, revision, PeerGrant::None)
            .await
            .unwrap();
        let regranted = store
            .replace_grant(&key, revoked.revision, custom)
            .await
            .unwrap();
        assert_ne!(regranted.revision, granted.revision);
        // Even immediate regrant does not resurrect this admitted turn.
        denied(
            tokio::time::timeout(Duration::from_secs(3), running)
                .await
                .expect("revoked scheduled run must stop while waiting")
                .unwrap(),
        );
        store
            .replace_grant(&key, regranted.revision, PeerGrant::None)
            .await
            .unwrap();
        denied(
            actor
                .ask(message(origin(
                    key.host.to_string(),
                    key.peer.to_string(),
                    SchedulePeerProtocol::V1,
                )))
                .await,
        );
        assert!(history.get_history(&sid).await.unwrap().is_empty());
        drop(permit);
        mesh.shutdown().await;
    }

    #[tokio::test]
    async fn peer_access_read_and_usage_hints_are_scoped_to_the_authenticated_peer() {
        use crate::agent::LocalAgentHandle;
        use crate::agent::remote::node_manager::{GetPeerSharing, PeerAccessInvalidated};
        use crate::agent::remote::remote_setup::spawn_and_register_local_mesh_actors;
        use kameo::remote::messaging::SwarmResponse;
        use querymt_remote::NodeId;
        use querymt_remote::mesh_ingress::MeshIngress;
        use querymt_remote::peer_access::quota::{
            AttemptUsage, QuotaAttempt, QuotaEdit, QuotaLimit, QuotaScope, QuotaWindow,
        };
        use querymt_remote::peer_access::{
            ModelSelection, PeerGrant, PeerGrantKey, ProviderGrant, SharingProtocol,
        };
        use serde_json::{Value, json};

        let (config, dir) = test_agent_config().await;
        let mesh = bootstrap_guarded_lan(&dir).await;
        let handle = LocalAgentHandle::from_config(config);
        handle.set_mesh(mesh.clone());
        let refs = spawn_and_register_local_mesh_actors(&handle, &mesh).await;
        let node = refs.node_manager.id();
        let router = mesh.inbound_router().unwrap();
        let store = mesh.quota_store().unwrap();
        let host = NodeId::from_peer_id(*mesh.peer_id());
        let recipient = libp2p::PeerId::random();
        let other = libp2p::PeerId::random();
        let key = PeerGrantKey {
            host,
            peer: NodeId::from_peer_id(recipient),
        };
        let get = |peer| async move {
            let SwarmResponse::Ask(Ok(bytes)) = router
                .handle_v1(
                    peer,
                    node_ask::<RemoteNodeManager, _>(node, &GetPeerSharing),
                )
                .await
                .unwrap()
            else {
                panic!("expected peer-specific access snapshot");
            };
            rmp_serde::from_slice::<Value>(&bytes).unwrap()
        };
        assert_eq!(get(recipient).await, json!({"status":"denied"}));
        let initial = store.grant(&key).await.unwrap();
        let custom = store
            .replace_grant(
                &key,
                initial.revision,
                PeerGrant::Custom {
                    sessions: true,
                    filesystem: false,
                    providers: vec![ProviderGrant {
                        provider_id: "mock".into(),
                        models: ModelSelection::All,
                    }],
                },
            )
            .await
            .unwrap();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let scope = QuotaScope {
            provider_id: "mock".into(),
            model_id: None,
        };
        let revision = store
            .edit_quota(
                &key,
                custom.revision,
                &scope,
                QuotaEdit::Reconfigure(QuotaLimit::Finite {
                    tokens: 30,
                    window: QuotaWindow::Manual,
                }),
                now,
            )
            .await
            .unwrap();
        let visible = get(recipient).await;
        assert_eq!(visible["status"], "granted");
        assert_eq!(visible["effective"]["mode"], "custom");
        assert_eq!(visible["effective"]["shell"], false);
        assert_eq!(visible["quotas"][0]["limit"]["limit_tokens"], "30");
        assert_eq!(get(other).await, json!({"status":"denied"}));

        let mut updates = handle.subscribe_ext_notifications();
        let attempt = store
            .reserve(QuotaAttempt {
                peer: key.clone(),
                protocol: SharingProtocol::V1,
                provider_id: "mock".into(),
                model_id: "fixture".into(),
                request_id: Uuid::now_v7(),
                bound_tokens: Some(20),
                now,
            })
            .await
            .unwrap();
        store.mark_dispatched(attempt).await.unwrap();
        store
            .settle(attempt, AttemptUsage::Observed(12))
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(3), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(event.params.get()).unwrap(),
            json!({"version":1,"peer_id":recipient.to_string(),"direction":"outgoing","reason":"quota"})
        );
        assert_eq!(
            get(recipient).await["quotas"][0]["usage"]["available_tokens"],
            "18"
        );
        assert_eq!(store.grant(&key).await.unwrap().revision, revision);

        let full = store
            .replace_grant(&key, revision, PeerGrant::Full)
            .await
            .unwrap();
        let visible = get(recipient).await;
        assert_eq!(visible["effective"]["mode"], "full");
        assert_eq!(visible["effective"]["shell"], true);
        assert_eq!(visible["quotas"], json!([]));
        store
            .replace_grant(&key, full.revision, PeerGrant::None)
            .await
            .unwrap();
        assert_eq!(get(recipient).await, json!({"status":"denied"}));
        let hint = router
            .handle_v1(
                other,
                node_ask::<RemoteNodeManager, _>(node, &PeerAccessInvalidated::Policy),
            )
            .await
            .unwrap();
        assert!(matches!(hint, SwarmResponse::Ask(Ok(_))));
        let event = tokio::time::timeout(Duration::from_secs(3), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(event.params.get()).unwrap(),
            json!({"version":1,"peer_id":other.to_string(),"direction":"incoming","reason":"policy"})
        );
        refs.node_manager.stop_gracefully().await.unwrap();
        mesh.shutdown().await;
    }

    #[tokio::test]
    async fn test_remote_schedule_lifecycle() {
        use kameo::remote::messaging::SwarmResponse;
        use querymt_remote::mesh_ingress::MeshIngress;
        use querymt_remote::peer_access::{PeerGrant, PeerGrantKey};

        let (config, dir) = test_agent_config().await;
        let mesh = bootstrap_guarded_lan(&dir).await;
        let handle = crate::agent::LocalAgentHandle::from_config(config);
        handle.set_mesh(mesh.clone());
        let nm_ref = crate::agent::remote::remote_setup::spawn_and_register_local_mesh_actors(
            &handle, &mesh,
        )
        .await
        .node_manager;
        let peer = libp2p::PeerId::random();
        let key = PeerGrantKey {
            host: querymt_remote::NodeId::from_peer_id(*mesh.peer_id()),
            peer: querymt_remote::NodeId::from_peer_id(peer),
        };
        let store = mesh.quota_store().unwrap();
        store
            .replace_grant(
                &key,
                store.grant(&key).await.unwrap().revision,
                PeerGrant::Full,
            )
            .await
            .unwrap();

        let session = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create session");

        let request = CreateRemoteSchedule {
            session_id: session.session_id.clone(),
            prompt: "run every minute".into(),
            trigger: crate::session::domain_schedule::ScheduleTrigger::Interval { seconds: 60 },
            max_steps: Some(5),
            max_cost_usd: Some(1.5),
            max_runs: Some(3),
        };
        let SwarmResponse::Ask(Ok(bytes)) = mesh
            .inbound_router()
            .unwrap()
            .handle_v1(
                peer,
                node_ask::<RemoteNodeManager, _>(nm_ref.id(), &request),
            )
            .await
            .unwrap()
        else {
            panic!("authenticated remote schedule creation must succeed");
        };
        let created: crate::agent::remote::CreateRemoteScheduleResponse =
            rmp_serde::from_slice(&bytes).unwrap();

        let list = nm_ref
            .ask(ListRemoteSchedules {
                session_id: Some(session.session_id.clone()),
            })
            .await
            .expect("list schedules");
        assert_eq!(list.schedules.len(), 1);
        assert_eq!(list.schedules[0].public_id, created.schedule_public_id);
        assert_eq!(list.schedules[0].config.max_runs, Some(3));
        assert_eq!(
            list.schedules[0]
                .config
                .execution_limits
                .as_ref()
                .and_then(|limits| limits.max_steps),
            Some(5)
        );

        nm_ref
            .ask(PauseRemoteSchedule {
                schedule_public_id: created.schedule_public_id.clone(),
            })
            .await
            .expect("pause schedule");
        let paused = nm_ref
            .ask(ListRemoteSchedules {
                session_id: Some(session.session_id.clone()),
            })
            .await
            .expect("list paused schedules");
        assert_eq!(paused.schedules[0].state.to_string(), "paused");

        nm_ref
            .ask(ResumeRemoteSchedule {
                schedule_public_id: created.schedule_public_id.clone(),
            })
            .await
            .expect("resume schedule");
        nm_ref
            .ask(DeleteRemoteSchedule {
                schedule_public_id: created.schedule_public_id.clone(),
            })
            .await
            .expect("delete schedule");
        let after_delete = nm_ref
            .ask(ListRemoteSchedules {
                session_id: Some(session.session_id),
            })
            .await
            .expect("list after delete");
        assert!(after_delete.schedules.is_empty());
        mesh.shutdown().await;
    }

    #[tokio::test]
    async fn test_list_sessions_uses_initial_intent_snapshot_as_title() {
        let (nm_ref, config, _td) = spawn_test_node_manager_with_mesh().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        let session = config
            .provider
            .history_store()
            .get_session(&resp.session_id)
            .await
            .expect("session lookup")
            .expect("created session should exist");

        config
            .provider
            .history_store()
            .create_intent_snapshot(IntentSnapshot {
                id: 0,
                session_id: session.id,
                task_id: None,
                summary: "Remote title from snapshot".to_string(),
                constraints: None,
                next_step_hint: None,
                revision: 1,
                source: "test".to_string(),
                source_ref: None,
                created_at: time::OffsetDateTime::now_utc(),
            })
            .await
            .expect("create intent snapshot");

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;
        let listed = sessions
            .iter()
            .find(|s| s.session_id == resp.session_id)
            .expect("created session should be listed");

        assert_eq!(listed.title.as_deref(), Some("Remote title from snapshot"));
    }

    #[tokio::test]
    async fn test_create_multiple_sessions() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let mut session_ids = Vec::new();
        for _ in 0..3 {
            let resp = nm_ref
                .ask(CreateRemoteSession { cwd: None })
                .await
                .expect("create");

            session_ids.push(resp.session_id);
        }

        // All IDs should be unique
        let mut sorted = session_ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "all session IDs should be unique");

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;
        assert_eq!(sessions.len(), 3);
    }

    #[tokio::test]
    async fn test_stop_session_runtime() {
        let (nm_ref, config, _td) = spawn_test_node_manager_with_mesh().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        nm_ref
            .ask(StopRemoteSessionRuntime {
                session_id: resp.session_id.clone(),
            })
            .await
            .expect("stop runtime should succeed");

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;
        // Stop removes the runtime actor but the session row persists in
        // SQLite, so ListRemoteSessions (which now queries the session store)
        // still returns it.
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, resp.session_id);
        assert!(
            config
                .provider
                .history_store()
                .get_session(&resp.session_id)
                .await
                .expect("session lookup")
                .is_some(),
            "stop should remove only the runtime, not persisted history"
        );
    }

    #[tokio::test]
    async fn test_stop_nonexistent_session_runtime() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let result = nm_ref
            .ask(StopRemoteSessionRuntime {
                session_id: "does-not-exist".to_string(),
            })
            .await;

        assert!(
            matches!(result, Err(SendError::HandlerError(_))),
            "stopping nonexistent session should fail with HandlerError"
        );
    }

    #[tokio::test]
    async fn test_get_node_info() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let info = nm_ref
            .ask(GetNodeInfo)
            .await
            .expect("get_node_info should succeed");

        assert!(!info.hostname.is_empty(), "hostname should not be empty");
        assert_eq!(info.active_sessions, Some(0));
        assert!(!info.capabilities.is_empty());
    }

    #[tokio::test]
    async fn test_get_node_info_reflects_session_count() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        for _ in 0..2 {
            nm_ref
                .ask(CreateRemoteSession { cwd: None })
                .await
                .expect("create");
        }

        let info = nm_ref.ask(GetNodeInfo).await.expect("info");
        assert_eq!(info.active_sessions, Some(2));
    }

    #[tokio::test]
    async fn test_create_session_emits_events() {
        let (nm_ref, config, _td) = spawn_test_node_manager_no_mesh().await;

        let mut rx = config.subscribe_events();

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            loop {
                let event = rx.recv().await.expect("recv");
                if event.session_id() == resp.session_id
                    && matches!(event.kind(), AgentEventKind::SessionCreated)
                {
                    break event;
                }
            }
        })
        .await
        .expect("should receive SessionCreated event within timeout");

        assert_eq!(event.session_id(), resp.session_id);
        assert!(matches!(event.kind(), AgentEventKind::SessionCreated));
    }

    #[tokio::test]
    async fn test_create_session_cwd_tracked_in_list() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;
        let cwd_dir = TempDir::new().unwrap();
        let cwd_str = cwd_dir.path().to_string_lossy().to_string();

        let resp = nm_ref
            .ask(CreateRemoteSession {
                cwd: Some(cwd_str.clone()),
            })
            .await
            .expect("create");

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, resp.session_id);
        assert_eq!(sessions[0].cwd, Some(cwd_str));
    }

    #[tokio::test]
    async fn test_stop_then_create_reuses_slot() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        // Create + destroy
        let resp1 = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create 1");

        nm_ref
            .ask(StopRemoteSessionRuntime {
                session_id: resp1.session_id.clone(),
            })
            .await
            .expect("stop");

        // Create again — should succeed and produce a different session ID
        let resp2 = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create 2");

        assert_ne!(resp1.session_id, resp2.session_id);

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;
        // Both sessions appear: resp1 is stopped but still persisted in
        // SQLite, resp2 is the newly created live session.
        assert_eq!(sessions.len(), 2);
        let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert!(
            ids.contains(&resp1.session_id.as_str()),
            "stopped session should still be listed from SQLite"
        );
        assert!(
            ids.contains(&resp2.session_id.as_str()),
            "new session should be listed"
        );
    }

    #[tokio::test]
    async fn test_resume_stopped_session() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        nm_ref
            .ask(StopRemoteSessionRuntime {
                session_id: resp.session_id.clone(),
            })
            .await
            .expect("stop");

        let resumed = nm_ref
            .ask(ResumeRemoteSession {
                session_id: resp.session_id.clone(),
            })
            .await
            .expect("resume");

        assert_eq!(resumed.session_id, resp.session_id);

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, resp.session_id);
    }

    /// Regression test: after stopping and resuming a session, the
    /// rematerialized actor's remote `GetEventStream` should include
    /// previously persisted events. Multi-thread runtime is required so
    /// mesh I/O on `MESH_RUNTIME` cannot deadlock against a current-thread
    /// test runtime waiting on the session actor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_resume_session_exposes_persisted_event_stream() {
        let (nm_ref, config, _td) = spawn_test_node_manager_with_mesh().await;
        let control_timeout = Duration::from_secs(5);

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .mailbox_timeout(control_timeout)
            .reply_timeout(control_timeout)
            .send()
            .await
            .unwrap_or_else(|err| panic!("create timed out after {control_timeout:?}: {err}"));

        // Persist a durable event through the agent's event sink.
        config
            .emit_event_persisted(
                &resp.session_id,
                querymt_agent_core::events::AgentEventKind::SessionCreated,
            )
            .await
            .expect("persist SessionCreated event");

        // Force the wrapper's full-history read to cross a remote page boundary.
        for _ in 0..crate::agent::messages::MAX_EVENT_STREAM_PAGE_SIZE + 1 {
            config
                .emit_event_persisted(
                    &resp.session_id,
                    querymt_agent_core::events::AgentEventKind::Cancelled,
                )
                .await
                .expect("persist paging event");
        }

        // Stop the live actor so the session is no longer in-memory.
        nm_ref
            .ask(StopRemoteSessionRuntime {
                session_id: resp.session_id.clone(),
            })
            .mailbox_timeout(control_timeout)
            .reply_timeout(control_timeout)
            .send()
            .await
            .unwrap_or_else(|err| panic!("stop timed out after {control_timeout:?}: {err}"));

        // Resume rematerializes from SQLite and exports a remote ref. kameo
        // reply_timeout does not cancel the DelegatedReply task, so bound
        // the wait with an outer timeout.
        let resumed = tokio::time::timeout(
            control_timeout,
            nm_ref
                .ask(ResumeRemoteSession {
                    session_id: resp.session_id.clone(),
                })
                .mailbox_timeout(control_timeout)
                .reply_timeout(control_timeout)
                .send(),
        )
        .await
        .unwrap_or_else(|_| panic!("resume outer timeout after {control_timeout:?}"))
        .unwrap_or_else(|err| panic!("resume timed out after {control_timeout:?}: {err}"));

        let session_ref = match resumed.handoff {
            SessionHandoff::DirectRemote { session_ref } => session_ref,
            other => panic!("expected DirectRemote handoff, got {other:?}"),
        };

        let events: Vec<querymt_agent_core::events::AgentEvent> = tokio::time::timeout(
            control_timeout,
            querymt_remote::ask_remote_with_timeout(&session_ref, &GetEventStream, control_timeout),
        )
        .await
        .unwrap_or_else(|_| panic!("GetEventStream outer timeout after {control_timeout:?}"))
        .unwrap_or_else(|err| match err {
            RemoteSendError::ReplyTimeout => {
                panic!("GetEventStream remote reply timed out after {control_timeout:?}")
            }
            other => panic!("GetEventStream RPC failed after {control_timeout:?}: {other}"),
        });

        let paged = crate::agent::remote::SessionActorRef::remote(session_ref, "test-peer".into())
            .get_event_stream()
            .await
            .expect("paged remote history");
        assert_eq!(paged.len(), events.len());
        assert_eq!(
            paged.iter().map(|event| event.seq).collect::<Vec<_>>(),
            events.iter().map(|event| event.seq).collect::<Vec<_>>()
        );
        assert!(events.len() > crate::agent::messages::MAX_EVENT_STREAM_PAGE_SIZE);
        assert!(
            events.iter().any(|e| e.session_id == resp.session_id),
            "events should belong to the resumed session"
        );
    }

    /// Sequential resume of the same session must reuse the existing actor
    /// instead of spawning a duplicate.
    #[tokio::test]
    async fn test_resume_same_session_reuses_actor() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        let first_handoff = nm_ref
            .ask(ResumeRemoteSession {
                session_id: resp.session_id.clone(),
            })
            .await
            .expect("first resume");

        let first_id = match first_handoff.handoff {
            SessionHandoff::DirectRemote { session_ref } => session_ref.id().sequence_id(),
            other => panic!("expected DirectRemote, got {other:?}"),
        };

        let second_handoff = nm_ref
            .ask(ResumeRemoteSession {
                session_id: resp.session_id.clone(),
            })
            .await
            .expect("second resume");

        let second_id = match second_handoff.handoff {
            SessionHandoff::DirectRemote { session_ref } => session_ref.id().sequence_id(),
            other => panic!("expected DirectRemote, got {other:?}"),
        };

        assert_eq!(
            first_id, second_id,
            "second resume must return the same actor as the first"
        );

        // Only one entry in the session list.
        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;
        let count = sessions
            .iter()
            .filter(|s| s.session_id == resp.session_id)
            .count();
        assert_eq!(count, 1, "session should appear exactly once in the list");
    }

    /// Resuming two different sessions concurrently should succeed for both.
    #[tokio::test]
    async fn test_resume_different_sessions_both_succeed() {
        let (nm_ref, _config, _td) = spawn_test_node_manager_with_mesh().await;

        let s1 = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create s1");

        let s2 = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create s2");

        // Resume both concurrently — neither should block the other.
        let (r1, r2) = tokio::join!(
            nm_ref.ask(ResumeRemoteSession {
                session_id: s1.session_id.clone()
            }),
            nm_ref.ask(ResumeRemoteSession {
                session_id: s2.session_id.clone()
            }),
        );

        let h1 = r1.expect("resume s1");
        let h2 = r2.expect("resume s2");

        let id1 = match h1.handoff {
            SessionHandoff::DirectRemote { session_ref } => session_ref.id().sequence_id(),
            other => panic!("s1: expected DirectRemote, got {other:?}"),
        };
        let id2 = match h2.handoff {
            SessionHandoff::DirectRemote { session_ref } => session_ref.id().sequence_id(),
            other => panic!("s2: expected DirectRemote, got {other:?}"),
        };

        assert_ne!(id1, id2, "different sessions must have different actors");
    }

    #[tokio::test]
    async fn test_fork_remote_session_returns_live_child_ref() {
        let (nm_ref, config, _td) = spawn_test_node_manager_with_mesh().await;

        let parent = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create parent");

        let message_id = Uuid::new_v4().to_string();
        config
            .provider
            .history_store()
            .add_message(
                &parent.session_id,
                AgentMessage {
                    id: message_id.clone(),
                    session_id: parent.session_id.clone(),
                    role: ChatRole::User,
                    parts: vec![MessagePart::Text {
                        content: "fork here".to_string(),
                    }],
                    created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                    parent_message_id: None,
                    source_provider: None,
                    source_model: None,
                },
            )
            .await
            .expect("insert parent message");

        let resp = nm_ref
            .ask(ForkRemoteSession {
                source_session_id: parent.session_id.clone(),
                message_id,
            })
            .await
            .expect("fork child");

        assert_ne!(resp.session_id, parent.session_id);
        let session_ref = match resp.handoff {
            SessionHandoff::DirectRemote { session_ref } => session_ref,
            SessionHandoff::LookupOnly => panic!("expected direct handoff in mesh test"),
            SessionHandoff::NoAttachPath => panic!("expected attachable handoff in mesh test"),
        };
        assert!(session_ref.ask(&GetMode).await.is_ok());

        let sessions = nm_ref
            .ask(ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list")
            .sessions;
        assert!(sessions.iter().any(|s| s.session_id == resp.session_id));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  SessionActorRef identification edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_from_actor_ref_into_local() {
    let (config, _td) = test_agent_config().await;
    let runtime = SessionRuntime::new(
        None,
        HashMap::new(),
        crate::agent::core::McpToolState::empty(),
    );
    let actor = SessionActor::new(config, "from-test".to_string(), runtime);
    let actor_ref = SessionActor::spawn(actor);

    // Test the From<ActorRef<SessionActor>> conversion
    let session_ref: SessionActorRef = actor_ref.into();
    assert!(!session_ref.is_remote());
    assert_eq!(session_ref.node_label(), "local");
}

#[tokio::test]
async fn test_session_actor_ref_clone() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "clone-test").await;
    let cloned = session_ref.clone();

    // Both should work independently
    let m1 = session_ref.get_mode().await.expect("get_mode original");
    let m2 = cloned.get_mode().await.expect("get_mode clone");
    assert_eq!(m1, m2);
}

// ═══════════════════════════════════════════════════════════════════════════
//  SessionRegistry — interaction with SessionActorRef
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_registry_get_returns_working_ref() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config.clone(), "reg-work-1").await;

    let mut registry = SessionRegistry::new(config);
    registry.insert("reg-work-1".to_string(), session_ref);

    // Get the ref from registry and use it
    let stored_ref = registry.get("reg-work-1").expect("should exist");
    let mode = stored_ref
        .get_mode()
        .await
        .expect("get_mode via registry ref");
    assert_eq!(mode, AgentMode::Build);
}

#[tokio::test]
async fn test_registry_insert_via_actor_ref() {
    let (config, _td) = test_agent_config().await;
    let runtime = SessionRuntime::new(
        None,
        HashMap::new(),
        crate::agent::core::McpToolState::empty(),
    );
    let actor = SessionActor::new(config.clone(), "raw-actor".to_string(), runtime);
    let actor_ref = SessionActor::spawn(actor);

    let mut registry = SessionRegistry::new(config);
    // insert() accepts `impl Into<SessionActorRef>`, including bare ActorRef
    registry.insert("raw-actor".to_string(), actor_ref);

    let stored = registry.get("raw-actor").expect("should exist");
    assert!(!stored.is_remote());
}
