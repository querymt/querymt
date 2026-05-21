//! Tests for Phase 3: Registry Lock Hygiene
//!
//! These tests verify that:
//! 1. The registry lock is never held across heavy async work
//! 2. Registry lock hold durations are in microseconds/milliseconds (not seconds)
//! 3. Concurrent session operations don't block each other
//! 4. The 3-phase materialization pattern works correctly

#[cfg(test)]
mod tests {
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::agent::session_actor::SessionActor;
    use crate::agent::session_registry::SessionRegistry;
    use crate::session::backend::StorageBackend;
    use crate::session::store::SessionStore;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory,
        mock_plugin_registry,
    };
    use kameo::actor::{ActorRef, Spawn};
    use querymt::LLMParams;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;

    /// Helper to create a test SessionRegistry with a config
    async fn create_test_registry() -> (SessionRegistry, tempfile::TempDir) {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider.clone(),
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory { provider: shared });
        let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");

        let store = MockSessionStore::new();
        let store: Arc<dyn SessionStore> = Arc::new(store);
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event store"),
        );

        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                store.clone(),
                storage.event_journal(),
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .build(),
        );

        (SessionRegistry::new(config), temp_dir)
    }

    /// Helper to spawn a test SessionActor
    fn spawn_test_actor(registry: &SessionRegistry, session_id: &str) -> ActorRef<SessionActor> {
        let runtime = crate::agent::core::SessionRuntime::new(
            None,
            std::collections::HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );
        let actor = SessionActor::new(registry.config.clone(), session_id.to_string(), runtime);
        SessionActor::spawn(actor)
    }

    /// Test that registry lock hold times are minimal for simple operations.
    ///
    /// This test verifies that operations like checking if a session exists
    /// or listing session IDs complete in microseconds, not milliseconds.
    #[tokio::test]
    async fn test_registry_lock_hold_time_minimal() {
        let (registry, _temp_dir) = create_test_registry().await;
        let registry = Arc::new(Mutex::new(registry));

        // Measure lock hold time for simple get operations
        let start = Instant::now();
        {
            let reg = registry.lock().await;
            // Simple map operations - should be microseconds
            let _ = reg.session_ids();
            let _ = reg.len();
            let _ = reg.is_empty();
        }
        let hold_time = start.elapsed();

        // Should complete in less than 1ms (typically microseconds)
        assert!(
            hold_time < Duration::from_millis(1),
            "Registry lock held for {:?}, expected < 1ms for simple operations",
            hold_time
        );
    }

    /// Test that concurrent registry reads don't block each other.
    ///
    /// Multiple readers should complete quickly even when accessing the registry concurrently.
    #[tokio::test]
    async fn test_concurrent_registry_reads_fast() {
        let (registry, _temp_dir) = create_test_registry().await;
        let registry = Arc::new(Mutex::new(registry));

        let mut tasks = vec![];
        for i in 0..10 {
            let reg_clone = registry.clone();
            tasks.push(tokio::spawn(async move {
                let start = Instant::now();
                let reg = reg_clone.lock().await;
                let _ids = reg.session_ids();
                let _len = reg.len();
                drop(reg);
                (i, start.elapsed())
            }));
        }

        let mut total_hold_time = Duration::ZERO;
        for task in tasks {
            let (i, hold_time) = task.await.unwrap();
            total_hold_time += hold_time;
            // Each individual operation should be fast
            assert!(
                hold_time < Duration::from_millis(10),
                "Task {}: Registry lock held for {:?}, expected < 10ms",
                i,
                hold_time
            );
        }

        // Total time for 10 sequential operations should be reasonable
        assert!(
            total_hold_time < Duration::from_millis(50),
            "Total lock time for 10 operations: {:?}, expected < 50ms",
            total_hold_time
        );
    }

    /// Test that session insertion (map-only operation) is fast.
    ///
    /// This verifies the "Register" phase of the 3-phase pattern is fast.
    #[tokio::test]
    async fn test_session_insertion_is_fast() {
        let (mut registry, _temp_dir) = create_test_registry().await;

        // Pre-spawn actor outside of lock
        let actor_ref = spawn_test_actor(&registry, "test-session");

        // Measure insertion time (this is what register_prepared_session does internally)
        let start = Instant::now();
        registry.insert("test-session".to_string(), actor_ref);
        let insert_time = start.elapsed();

        // Should complete in microseconds
        assert!(
            insert_time < Duration::from_millis(1),
            "Session insertion took {:?}, expected < 1ms (only map insert)",
            insert_time
        );

        // Verify the session was inserted
        assert_eq!(registry.len(), 1);
        assert!(registry.get("test-session").is_some());
    }

    /// Test that multiple session insertions are fast.
    ///
    /// This simulates the Register phase for multiple sessions.
    #[tokio::test]
    async fn test_multiple_session_insertions_fast() {
        let (mut registry, _temp_dir) = create_test_registry().await;

        let mut total_insert_time = Duration::ZERO;

        // Insert 10 sessions and measure total time
        for i in 0..10 {
            let session_id = format!("session-{}", i);
            let actor_ref = spawn_test_actor(&registry, &session_id);

            let start = Instant::now();
            registry.insert(session_id, actor_ref);
            total_insert_time += start.elapsed();
        }

        // All insertions should complete in < 5ms total
        assert!(
            total_insert_time < Duration::from_millis(5),
            "Total insertion time for 10 sessions: {:?}, expected < 5ms",
            total_insert_time
        );

        assert_eq!(registry.len(), 10);
    }

    /// Test that registry lock wait times are acceptable.
    ///
    /// Measures how long it takes to acquire the registry lock when
    /// another operation is holding it briefly.
    #[tokio::test]
    async fn test_registry_lock_wait_times() {
        let (registry, _temp_dir) = create_test_registry().await;
        let registry = Arc::new(Mutex::new(registry));

        // Spawn a task that holds the lock briefly
        let reg_clone = registry.clone();
        let hold_task = tokio::spawn(async move {
            let reg = reg_clone.lock().await;
            // Simulate brief lock hold (just map operations)
            let _ = reg.session_ids();
            let _ = reg.len();
            // Hold for a tiny bit
            tokio::time::sleep(Duration::from_micros(100)).await;
        });

        // Small delay to let the hold task acquire the lock
        tokio::time::sleep(Duration::from_micros(10)).await;

        // Try to acquire the lock from another task
        let start = Instant::now();
        let reg = registry.lock().await;
        let wait_time = start.elapsed();

        // Should acquire quickly (microseconds to low milliseconds)
        assert!(
            wait_time < Duration::from_millis(10),
            "Registry lock wait time: {:?}, expected < 10ms",
            wait_time
        );

        // Quick operation
        let _ = reg.session_ids();
        drop(reg);

        // Wait for hold task to complete
        hold_task.await.unwrap();
    }

    /// Test that session removal is fast (map-only operation).
    ///
    /// This verifies that cleanup operations are also fast.
    #[tokio::test]
    async fn test_session_removal_is_fast() {
        let (mut registry, _temp_dir) = create_test_registry().await;

        // Insert a session first
        let actor_ref = spawn_test_actor(&registry, "test-session");
        registry.insert("test-session".to_string(), actor_ref);
        assert_eq!(registry.len(), 1);

        // Measure removal time
        let start = Instant::now();
        let removed = registry.remove("test-session");
        let remove_time = start.elapsed();

        // Should complete in microseconds
        assert!(
            remove_time < Duration::from_millis(1),
            "Session removal took {:?}, expected < 1ms (only map remove)",
            remove_time
        );

        assert!(removed.is_some());
        assert_eq!(registry.len(), 0);
    }

    /// Test that session lookups are fast.
    ///
    /// This verifies that read operations (like checking if a session exists)
    /// are fast and don't block.
    #[tokio::test]
    async fn test_session_lookups_fast() {
        let (mut registry, _temp_dir) = create_test_registry().await;

        // Insert some sessions
        for i in 0..5 {
            let session_id = format!("session-{}", i);
            let actor_ref = spawn_test_actor(&registry, &session_id);
            registry.insert(session_id, actor_ref);
        }

        // Measure lookup time
        let start = Instant::now();
        for i in 0..5 {
            let session_id = format!("session-{}", i);
            let _ = registry.get(&session_id);
            let _ = registry.local_actor_ref(&session_id);
        }
        let lookup_time = start.elapsed();

        // All lookups should complete in < 1ms
        assert!(
            lookup_time < Duration::from_millis(1),
            "Total lookup time for 5 sessions: {:?}, expected < 1ms",
            lookup_time
        );
    }
}
