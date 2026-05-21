//! Tests for local session concurrency issues.
//!
//! These tests verify that:
//! 1. Concurrent load_session_with_preconnected for the same session ID doesn't create duplicate actors
//! 2. Concurrent resume_session for the same session ID doesn't create duplicate actors
//! 3. The SessionMaterializer's single-flight mechanism actually prevents concurrent materialization
//!
//! These tests are written in TDD red/green style:
//! - RED: Test demonstrates the bug (duplicate actors created)
//! - GREEN: After fix, test passes (single-flight prevents duplicates)

#[cfg(test)]
mod tests {
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::agent::handle::LocalAgentHandle;
    use crate::agent::session_materializer::{PreparedSessionResult, SessionMaterializer};
    use crate::agent::session_registry::SessionRegistry;
    use crate::send_agent::SendAgent;
    use crate::session::backend::StorageBackend;
    use crate::test_utils::{
        MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
    };
    use agent_client_protocol::schema::{
        LoadSessionRequest, NewSessionRequest, ResumeSessionRequest, SessionId,
    };
    use querymt::LLMParams;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    enum LoadAttempt {
        Prepared(kameo::actor::ActorId),
        Existing,
    }

    /// Helper to create a test setup with SessionMaterializer and SessionRegistry
    async fn create_test_setup() -> (
        Arc<SessionMaterializer>,
        Arc<Mutex<SessionRegistry>>,
        Arc<crate::agent::agent_config::AgentConfig>,
        tempfile::TempDir,
    ) {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider.clone(),
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory { provider: shared });
        let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");

        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event store"),
        );

        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                storage.session_store(),
                storage.event_journal(),
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .build(),
        );

        let materializer = Arc::new(SessionMaterializer::new(config.clone()));
        let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));

        (materializer, registry, config, temp_dir)
    }

    /// Test that concurrent prepare_load_session calls for the same session ID
    /// with a registry re-check create only ONE actor.
    ///
    /// This tests the complete single-flight mechanism:
    /// 1. First caller acquires lock, checks registry (empty), materializes actor
    /// 2. Subsequent callers acquire lock sequentially, check registry (session exists), return early
    ///
    /// GREEN: After fix, this test should PASS (only one actor created)
    #[tokio::test]
    async fn test_concurrent_load_same_session_with_registry_creates_only_one_actor() {
        let (materializer, registry, _config, _temp_dir) = create_test_setup().await;

        // First, create a session to load
        let create_req = NewSessionRequest::new(PathBuf::from("/tmp/test-session"));
        let prepared = materializer
            .prepare_new_session(create_req, vec![])
            .await
            .expect("create session");
        let session_id = prepared.session_id.clone();

        // Register this initial session in the registry
        {
            let mut reg = registry.lock().await;
            reg.register_prepared_session(&prepared).await;
        }
        drop(prepared); // Don't keep the first prepared session around

        // Now spawn multiple concurrent load requests for the same session
        let mut handles = Vec::new();
        for i in 0..5 {
            let mat = materializer.clone();
            let reg = registry.clone();
            let load_req = LoadSessionRequest::new(
                SessionId::from(session_id.clone()),
                PathBuf::from(format!("/tmp/test-{}", i)),
            );

            // Spawn concurrent calls with the registry
            handles.push(tokio::spawn(async move {
                match mat.prepare_load_session(load_req, vec![], Some(&reg)).await {
                    Ok(PreparedSessionResult::Prepared(prepared)) => {
                        let actor_id = prepared.actor_ref.id();
                        {
                            let mut registry = reg.lock().await;
                            registry.register_prepared_session(&prepared).await;
                        }
                        drop(prepared);
                        Ok(LoadAttempt::Prepared(actor_id))
                    }
                    Ok(PreparedSessionResult::AlreadyRegistered(_)) => Ok(LoadAttempt::Existing),
                    Err(e) => Err(e),
                }
            }));
        }

        // Collect all results
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // Count how many materialized vs returned early
        let mut materialized_count = 0;
        let mut early_return_count = 0;
        let mut actor_ref_ids = Vec::new();

        for (i, result) in results.iter().enumerate() {
            match result {
                Ok(LoadAttempt::Prepared(actor_id)) => {
                    materialized_count += 1;
                    actor_ref_ids.push(*actor_id);
                    println!("Load {}: Materialized new actor", i);
                }
                Ok(LoadAttempt::Existing) => {
                    early_return_count += 1;
                    println!("Load {}: Returned existing session", i);
                }
                Err(e) => {
                    panic!("Load {} failed: {:?}", i, e);
                }
            }
        }

        println!(
            "Summary: {} materialized, {} early returns",
            materialized_count, early_return_count
        );

        // KEY ASSERTIONS:
        // 1. The initial session was already registered, so ALL calls should return None
        //    (they check registry and find the session)
        // 2. No duplicate actors should be created
        assert_eq!(
            early_return_count, 5,
            "Expected all 5 calls to return early (session already in registry), but {} materialized",
            materialized_count
        );
        assert_eq!(
            materialized_count, 0,
            "Expected 0 new materializations, but got {}",
            materialized_count
        );

        println!(
            "✓ All {} loads correctly returned early for already-registered session",
            5
        );
    }

    /// Test that concurrent prepare_load_session calls for the same session ID
    /// WITHOUT prior registration still only create ONE actor due to single-flight locking.
    ///
    /// This tests the edge case where multiple callers all see an empty registry,
    /// but the single-flight lock ensures only one materializes the actor.
    #[tokio::test]
    async fn test_concurrent_load_same_session_without_prior_registration_creates_only_one_actor() {
        let (materializer, registry, _config, _temp_dir) = create_test_setup().await;

        // Create a session in the DB but DON'T register it in the registry yet
        let create_req = NewSessionRequest::new(PathBuf::from("/tmp/test-session"));
        let prepared = materializer
            .prepare_new_session(create_req, vec![])
            .await
            .expect("create session");
        let session_id = prepared.session_id.clone();
        drop(prepared); // Don't register it

        // Now spawn multiple concurrent load requests for the same session
        let mut handles = Vec::new();
        for i in 0..5 {
            let mat = materializer.clone();
            let reg = registry.clone();
            let load_req = LoadSessionRequest::new(
                SessionId::from(session_id.clone()),
                PathBuf::from(format!("/tmp/test-{}", i)),
            );

            // Spawn concurrent calls with the registry
            handles.push(tokio::spawn(async move {
                match mat.prepare_load_session(load_req, vec![], Some(&reg)).await {
                    Ok(PreparedSessionResult::Prepared(prepared)) => {
                        let actor_id = prepared.actor_ref.id();
                        {
                            let mut registry = reg.lock().await;
                            registry.register_prepared_session(&prepared).await;
                        }
                        drop(prepared);
                        Ok(LoadAttempt::Prepared(actor_id))
                    }
                    Ok(PreparedSessionResult::AlreadyRegistered(_)) => Ok(LoadAttempt::Existing),
                    Err(e) => Err(e),
                }
            }));
        }

        // Collect all results
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // Count how many materialized vs returned early
        let mut materialized_count = 0;
        let mut early_return_count = 0;
        let mut actor_ref_ids = Vec::new();

        for (i, result) in results.iter().enumerate() {
            match result {
                Ok(LoadAttempt::Prepared(actor_id)) => {
                    materialized_count += 1;
                    actor_ref_ids.push(*actor_id);
                    println!("Load {}: Materialized new actor", i);
                }
                Ok(LoadAttempt::Existing) => {
                    early_return_count += 1;
                    println!("Load {}: Returned existing session", i);
                }
                Err(e) => {
                    panic!("Load {} failed: {:?}", i, e);
                }
            }
        }

        println!(
            "Summary: {} materialized, {} early returns",
            materialized_count, early_return_count
        );

        // KEY ASSERTIONS:
        // 1. Only ONE caller should materialize (the first to acquire lock and find empty registry)
        // 2. The remaining callers should return None (they find the session already registered)
        assert_eq!(
            materialized_count, 1,
            "Expected exactly 1 materialization, but got {}",
            materialized_count
        );
        assert_eq!(
            early_return_count, 4,
            "Expected 4 early returns, but got {}",
            early_return_count
        );

        // Verify all actor refs point to the same actor (only 1 was created)
        let first_id = &actor_ref_ids[0];
        for (i, actor_id) in actor_ref_ids.iter().enumerate() {
            assert_eq!(
                first_id, actor_id,
                "Actor ref {} should match the first one",
                i
            );
        }

        println!("✓ All 5 loads correctly resulted in only 1 actor creation");
    }

    /// Test that concurrent prepare_load_session calls for DIFFERENT session IDs
    /// create DIFFERENT actors (parallelism should still work for different sessions).
    ///
    /// This is the counter-test: while same-session should be single-flight,
    /// different sessions should proceed independently in parallel.
    #[tokio::test]
    async fn test_concurrent_load_different_sessions_create_different_actors() {
        let (materializer, registry, _config, _temp_dir) = create_test_setup().await;

        // Create multiple different sessions
        let mut session_ids = Vec::new();
        for i in 0..5 {
            let create_req = NewSessionRequest::new(PathBuf::from(format!("/tmp/test-{}", i)));
            let prepared = materializer
                .prepare_new_session(create_req, vec![])
                .await
                .expect("create session");
            session_ids.push(prepared.session_id.clone());
            drop(prepared);
        }

        // Now load all different sessions concurrently
        let mut handles = Vec::new();
        for (i, session_id) in session_ids.iter().enumerate() {
            let mat = materializer.clone();
            let reg = registry.clone();
            let sid = session_id.clone();
            let load_req = LoadSessionRequest::new(
                SessionId::from(session_id.clone()),
                PathBuf::from(format!("/tmp/test-{}", i)),
            );

            // Different sessions should proceed independently
            handles.push(tokio::spawn(async move {
                let result = match mat.prepare_load_session(load_req, vec![], Some(&reg)).await {
                    Ok(PreparedSessionResult::Prepared(prepared)) => {
                        let actor_id = prepared.actor_ref.id();
                        {
                            let mut registry = reg.lock().await;
                            registry.register_prepared_session(&prepared).await;
                        }
                        drop(prepared);
                        Ok(LoadAttempt::Prepared(actor_id))
                    }
                    Ok(PreparedSessionResult::AlreadyRegistered(_)) => Ok(LoadAttempt::Existing),
                    Err(e) => Err(e),
                };
                (sid.clone(), result)
            }));
        }

        // Collect all results
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // Count how many materialized
        let mut materialized_count = 0;
        let mut actor_ref_ids = Vec::new();

        for (i, (session_id, result)) in results.iter().enumerate() {
            match result {
                Ok(LoadAttempt::Prepared(actor_id)) => {
                    materialized_count += 1;
                    actor_ref_ids.push((session_id.clone(), *actor_id));
                    println!("Load {}: Materialized actor for session {}", i, session_id);
                }
                Ok(LoadAttempt::Existing) => {
                    panic!(
                        "Load {}: Expected materialization for new session, but got existing session",
                        i
                    );
                }
                Err(e) => {
                    panic!("Load {} failed: {:?}", i, e);
                }
            }
        }

        // KEY ASSERTION: All 5 different sessions should materialize (parallelism works!)
        assert_eq!(
            materialized_count, 5,
            "Expected 5 materializations for 5 different sessions, but got {}",
            materialized_count
        );

        // Verify all session IDs map to different actors
        let unique_actor_ids: std::collections::HashSet<_> =
            actor_ref_ids.iter().map(|(_, id)| *id).collect();

        assert_eq!(
            unique_actor_ids.len(),
            5,
            "Expected 5 different actors for 5 different sessions, but got {} unique actors",
            unique_actor_ids.len()
        );

        println!(
            "✓ {} different sessions created {} different actors (parallelism preserved)",
            actor_ref_ids.len(),
            unique_actor_ids.len()
        );
    }

    #[tokio::test]
    async fn test_concurrent_resume_same_session_returns_success_for_all_callers() {
        let (materializer, _registry, config, _temp_dir) = create_test_setup().await;

        let prepared = materializer
            .prepare_new_session(
                NewSessionRequest::new(PathBuf::from("/tmp/test-session")),
                vec![],
            )
            .await
            .expect("create session");
        let session_id = prepared.session_id.clone();
        drop(prepared);

        let handle = Arc::new(LocalAgentHandle::from_config(config));
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let handle = handle.clone();
            let req = ResumeSessionRequest::new(
                SessionId::from(session_id.clone()),
                PathBuf::from("/tmp/test-session"),
            );
            tasks.push(tokio::spawn(
                async move { handle.resume_session(req).await },
            ));
        }

        for task in tasks {
            let result = task.await.expect("task join");
            assert!(
                result.is_ok(),
                "resume should succeed for all callers: {result:?}"
            );
        }

        let registry = handle.registry.lock().await;
        assert_eq!(registry.len(), 1, "only one session should be registered");
    }
}
