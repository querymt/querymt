use super::*;

async fn ext_method_json(
    handle: &LocalAgentHandle,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let req = agent_client_protocol::schema::ExtRequest::new(
        method,
        std::sync::Arc::from(serde_json::value::RawValue::from_string(params.to_string()).unwrap()),
    );
    let resp = handle.ext_method(req).await.expect("ext_method");
    serde_json::from_str(resp.0.get()).expect("valid JSON")
}

async fn wait_for_condition<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if f().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met before timeout"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_schedule_ext_create_remote_missing_session_surfaces_error() {
    use crate::agent::remote::RemoteNodeManager;
    use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
    use kameo::actor::Spawn;

    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
    let peer_id = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let node_id = peer_id.to_string();
    let node_manager = RemoteNodeManager::new(
        remote.handle.config.clone(),
        remote.handle.registry.clone(),
        Some(mesh.clone()),
        remote.handle.scheduler_handle.clone(),
    )
    .with_node_name("peer-alpha".to_string());
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    let per_peer_name = scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &peer_id);
    mesh.register_actor(node_manager_ref.clone(), per_peer_name)
        .await;
    mesh.inject_known_peer_for_test(peer_id);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let session = node_manager_ref
        .ask(crate::agent::remote::CreateRemoteSession { cwd: None })
        .await
        .expect("create remote session");

    remote
        .storage
        .session_store()
        .delete_session(&session.session_id)
        .await
        .expect("delete persisted remote session row");

    let req = agent_client_protocol::schema::ExtRequest::new(
        "querymt/schedules/create",
        std::sync::Arc::from(
            serde_json::value::RawValue::from_string(
                serde_json::json!({
                    "nodeId": node_id,
                    "sessionId": session.session_id,
                    "prompt": "daily summary",
                    "trigger": { "type": "interval", "seconds": 300 },
                    "maxRuns": 2,
                })
                .to_string(),
            )
            .unwrap(),
        ),
    );

    let err = f
        .handle
        .ext_method(req)
        .await
        .expect_err("missing persisted remote session should reject schedule create");

    assert_eq!(err.code, agent_client_protocol::ErrorCode::ResourceNotFound);
    assert!(
        err.message.contains("session not found") || err.message.contains("Session not found"),
        "error should mention missing session: {}",
        err.message
    );
    assert!(
        err.message.contains(&session.session_id),
        "error should include session id: {}",
        err.message
    );
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_remote_attach_session_missing_persisted_session_surfaces_error() {
    use crate::agent::remote::RemoteNodeManager;
    use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
    use kameo::actor::Spawn;

    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
    let peer_id = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let node_id = peer_id.to_string();
    let node_manager = RemoteNodeManager::new(
        remote.handle.config.clone(),
        remote.handle.registry.clone(),
        Some(mesh.clone()),
        remote.handle.scheduler_handle.clone(),
    )
    .with_node_name("peer-alpha".to_string());
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    let per_peer_name = scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &peer_id);
    mesh.register_actor(node_manager_ref.clone(), per_peer_name)
        .await;
    mesh.inject_known_peer_for_test(peer_id);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let missing_session_id = uuid::Uuid::now_v7().to_string();

    let req = agent_client_protocol::schema::ExtRequest::new(
        "querymt/remote/attachSession",
        std::sync::Arc::from(
            serde_json::value::RawValue::from_string(
                serde_json::json!({
                    "nodeId": node_id,
                    "sessionId": missing_session_id,
                })
                .to_string(),
            )
            .unwrap(),
        ),
    );

    let err = f
        .handle
        .ext_method(req)
        .await
        .expect_err("missing persisted remote session should reject attach");

    assert_eq!(err.code, agent_client_protocol::ErrorCode::ResourceNotFound);
    assert!(
        err.message.contains("session not found") || err.message.contains("Session not found"),
        "error should mention missing session: {}",
        err.message
    );
    assert!(
        err.message.contains(&missing_session_id),
        "error should include session id: {}",
        err.message
    );
}

#[tokio::test]
async fn test_querymt_schedule_ext_methods_local() {
    let f = RealStorageHandleFixture::new().await;
    let session = f
        .storage
        .session_store()
        .create_session(None, None, None, None)
        .await
        .expect("create session");
    let session_id = session.public_id;

    let created = ext_method_json(
        &f.handle,
        "querymt/schedules/create",
        serde_json::json!({
            "sessionId": session_id,
            "prompt": "daily summary",
            "trigger": { "type": "interval", "seconds": 300 },
            "maxRuns": 2,
        }),
    )
    .await;
    let schedule_id = created["schedulePublicId"]
        .as_str()
        .expect("schedule id")
        .to_string();

    wait_for_condition(|| {
        let storage = f.storage.clone();
        let session_id = session_id.clone();
        async move {
            storage
                .schedule_repository()
                .expect("schedule repo")
                .list_schedules(&session_id)
                .await
                .map(|s| s.len() == 1)
                .unwrap_or(false)
        }
    })
    .await;

    let listed = ext_method_json(
        &f.handle,
        "querymt/schedules/list",
        serde_json::json!({ "sessionId": session_id }),
    )
    .await;
    assert_eq!(listed["schedules"].as_array().map(Vec::len), Some(1));

    let _ = ext_method_json(
        &f.handle,
        "querymt/schedules/pause",
        serde_json::json!({ "schedulePublicId": schedule_id.clone() }),
    )
    .await;

    wait_for_condition(|| {
        let storage = f.storage.clone();
        let session_id = session_id.clone();
        let schedule_id = schedule_id.clone();
        async move {
            storage
                .schedule_repository()
                .expect("schedule repo")
                .list_schedules(&session_id)
                .await
                .map(|schedules| {
                    schedules.first().is_some_and(|s| {
                        s.public_id == schedule_id && s.state.to_string() == "paused"
                    })
                })
                .unwrap_or(false)
        }
    })
    .await;

    let listed = ext_method_json(
        &f.handle,
        "querymt/schedules/list",
        serde_json::json!({ "sessionId": session_id }),
    )
    .await;
    assert_eq!(listed["schedules"][0]["state"], "paused");

    let _ = ext_method_json(
        &f.handle,
        "querymt/schedules/delete",
        serde_json::json!({ "schedulePublicId": schedule_id.clone() }),
    )
    .await;

    let listed = ext_method_json(
        &f.handle,
        "querymt/schedules/list",
        serde_json::json!({ "sessionId": session_id }),
    )
    .await;
    assert_eq!(listed["schedules"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn test_list_schedules_returns_empty_when_scheduler_actor_stops() {
    let f = HandleFixture::new().await;
    assert!(f.handle.start_scheduler().await);

    if let Some(scheduler) = f.handle.scheduler() {
        scheduler.shutdown().await;
    }

    let schedules = f.handle.list_schedules(None).await.expect("list_schedules");
    assert!(schedules.is_empty());
}

#[tokio::test]
async fn test_get_schedule_returns_none_when_scheduler_actor_stops() {
    let f = HandleFixture::new().await;
    assert!(f.handle.start_scheduler().await);

    if let Some(scheduler) = f.handle.scheduler() {
        scheduler.shutdown().await;
    }

    let schedule = f
        .handle
        .get_schedule("missing-schedule")
        .await
        .expect("get_schedule");
    assert!(schedule.is_none());
}

#[tokio::test]
async fn test_trigger_schedule_now_recovers_from_stopped_scheduler_actor() {
    let f = HandleFixture::new().await;
    assert!(f.handle.start_scheduler().await);

    if let Some(scheduler) = f.handle.scheduler() {
        scheduler.shutdown().await;
    }

    // Triggering a missing schedule should still succeed at the transport level
    // once the scheduler is recovered.
    let result = f.handle.trigger_schedule_now("missing-schedule").await;
    assert!(result.is_ok(), "{result:?}");
}

/// After shutdown, background loops (reconciliation, event subscription) must
/// exit promptly instead of lingering and producing "actor not running" warnings.
///
/// This test verifies the fix for the background task leak: previously,
/// `abort_background_tasks()` only aborted the deadline wake handle but left
/// the reconciliation and event subscription loops running. They would keep
/// trying `tell()` on the dead actor until their next iteration happened to
/// fail, producing noisy WARN-level log messages in the meantime.
#[tokio::test]
async fn test_shutdown_stops_background_loops_promptly() {
    let f = HandleFixture::new().await;
    assert!(f.handle.start_scheduler().await);

    // Subscribe to events so we can emit one after shutdown
    let _rx = f.handle.subscribe_events();

    // Shut down the scheduler
    if let Some(scheduler) = f.handle.scheduler() {
        scheduler.shutdown().await;
    }

    // Emit an event that would have been forwarded to the scheduler's
    // event subscription loop. Before the fix, this would cause
    // "failed to send ProcessEvent: actor not running" warnings.
    f.handle
        .emit_event("test-session", crate::events::AgentEventKind::Cancelled);

    // Give the event loop a moment to process (or not, since it should be dead)
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Drain the broadcast receiver — the event should be there (it was emitted)
    // but the scheduler's background loop should NOT have tried to forward it.
    // We can't directly observe the absence of a log warning in a unit test,
    // but we verify the scheduler actor is truly dead by confirming that
    // metrics() returns the default (the ask fails and returns default).
    if let Some(scheduler) = f.handle.scheduler() {
        let metrics = scheduler.metrics().await;
        // If the actor is stopped, metrics() returns Default via unwrap_or_default.
        // A fresh default has fires_total == 0, which is fine — the point is
        // the call doesn't hang or panic.
        assert_eq!(metrics.fires_total, 0);
    }

    // The real assertion: we can immediately start a new scheduler without
    // the old background loops interfering with lease acquisition.
    f.handle.clear_scheduler_handle();
    assert!(
        f.handle.start_scheduler().await,
        "new scheduler must acquire lease immediately after shutdown — \
         old background loops must not interfere"
    );
}

/// After shutdown, the lease is released and a new scheduler can acquire it
/// without waiting for TTL expiry.
///
/// Before the fix, the lease renewal loop could still be running after the
/// actor was stopped and might re-acquire or interfere with the lease between
/// the release and the new scheduler's acquisition attempt.
#[tokio::test]
async fn test_shutdown_releases_lease_for_immediate_reacquisition() {
    let f = HandleFixture::new().await;

    // Start and stop the scheduler twice in quick succession.
    // If background loops leak, the second start would fail because the
    // first scheduler's renewal loop would still hold (or contest) the lease.
    for i in 0..3 {
        assert!(
            f.handle.start_scheduler().await,
            "scheduler start #{} should acquire lease",
            i + 1
        );

        if let Some(scheduler) = f.handle.scheduler() {
            scheduler.shutdown().await;
        }
        f.handle.clear_scheduler_handle();

        // No sleep between iterations — the old loops must already be dead
    }

    // Final start should also work
    assert!(
        f.handle.start_scheduler().await,
        "final scheduler start should acquire lease after rapid stop/start cycles"
    );
}
