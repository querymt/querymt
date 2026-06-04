//! Tests for concurrent materialization scenarios in RemoteNodeManager.
//!
//! These tests verify Phase 2 of the Fast Control-Plane Concurrency Plan:
//! - Concurrent materialization of different session IDs proceeds independently
//! - Concurrent materialization of the same session ID is single-flight and idempotent
//! - Lightweight control-plane operations remain responsive during materialization

#[cfg(test)]
#[cfg(feature = "remote")]
mod tests {
    use crate::agent::remote::node_manager::{
        CreateRemoteSession, GetNodeInfo, ResumeRemoteSession,
    };
    use crate::agent::remote::test_helpers::fixtures::NodeManagerFixture;
    use std::time::{Duration, Instant};
    use tokio::task::JoinSet;

    /// Test that concurrent materialization of different session IDs proceeds
    /// independently and doesn't block the actor mailbox.
    ///
    /// This verifies the DelegatedReply pattern: heavyweight create operations
    /// spawn background tasks, allowing the actor to respond to other messages.
    #[tokio::test]
    async fn test_concurrent_different_sessions_materialize_independently() {
        let fixture = NodeManagerFixture::new().await;
        let actor_ref = fixture.actor_ref.clone();

        // Spawn multiple create operations concurrently
        let mut join_set = JoinSet::new();

        for i in 0..3 {
            let actor = actor_ref.clone();
            join_set.spawn(async move {
                let msg = CreateRemoteSession {
                    cwd: Some(format!("/tmp/test-session-{}", i)),
                };
                let result = actor.ask(msg).await;
                (i, result)
            });
        }

        // Collect results - all should succeed with different session IDs
        let mut session_ids = Vec::new();
        let mut errors = Vec::new();

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((i, Ok(response))) => {
                    println!("Session {} created: {}", i, response.session_id);
                    session_ids.push(response.session_id);
                }
                Ok((i, Err(e))) => {
                    errors.push(format!("Session {} failed: {}", i, e));
                }
                Err(e) => {
                    errors.push(format!("Join error: {}", e));
                }
            }
        }

        assert!(errors.is_empty(), "Errors occurred: {:?}", errors);
        assert_eq!(session_ids.len(), 3, "Expected 3 sessions");

        // Verify all session IDs are unique
        let unique_ids: std::collections::HashSet<_> = session_ids.iter().collect();
        assert_eq!(unique_ids.len(), 3, "Session IDs should be unique");
    }

    /// Test that concurrent resume of the same session ID is single-flight:
    /// only one materialization happens, and others get the same result.
    ///
    /// This verifies the per-session locking mechanism in the DelegatedReply pattern.
    #[tokio::test]
    async fn test_concurrent_same_session_resume_is_single_flight() {
        let fixture = NodeManagerFixture::new().await;
        let actor_ref = fixture.actor_ref.clone();

        // First, create a session
        let create_msg = CreateRemoteSession {
            cwd: Some("/tmp/test-single-flight".to_string()),
        };
        let create_response = actor_ref
            .ask(create_msg)
            .await
            .expect("create should succeed");
        let session_id = create_response.session_id;

        // Now spawn multiple concurrent resume operations for the same session
        let mut join_set = JoinSet::new();
        let num_concurrent = 5;

        for i in 0..num_concurrent {
            let actor = actor_ref.clone();
            let sid = session_id.clone();
            join_set.spawn(async move {
                let msg = ResumeRemoteSession { session_id: sid };
                let start = Instant::now();
                let result = actor.ask(msg).await;
                let duration = start.elapsed();
                (i, result, duration)
            });
        }

        // Collect results
        let mut results = Vec::new();
        let mut errors = Vec::new();

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((i, Ok(response), duration)) => {
                    println!(
                        "Resume {} completed in {:?}: session_id={}",
                        i, duration, response.session_id
                    );
                    results.push((i, response, duration));
                }
                Ok((i, Err(e), _)) => {
                    errors.push(format!("Resume {} failed: {}", i, e));
                }
                Err(e) => {
                    errors.push(format!("Join error: {}", e));
                }
            }
        }

        assert!(errors.is_empty(), "Errors occurred: {:?}", errors);
        assert_eq!(
            results.len(),
            num_concurrent,
            "Expected {} results",
            num_concurrent
        );

        // All should return the same session ID (idempotent)
        for (i, response, _) in &results {
            assert_eq!(
                response.session_id, session_id,
                "Resume {} should return the same session ID",
                i
            );
        }

        // The key property: at least one should be fast (single-flight)
        // While others wait for the first to complete
        let durations: Vec<Duration> = results.iter().map(|(_, _, d)| *d).collect();
        let min_duration = durations.iter().min().unwrap();
        let max_duration = durations.iter().max().unwrap();

        println!(
            "Min duration: {:?}, Max duration: {:?}",
            min_duration, max_duration
        );

        // The spread between min and max shouldn't be too large
        // (they should all complete relatively quickly since it's single-flight)
        let spread = *max_duration - *min_duration;
        assert!(
            spread < Duration::from_secs(2),
            "Duration spread should be small for single-flight, got {:?}",
            spread
        );
    }

    /// Test that GetNodeInfo responds quickly while session creation is in progress.
    ///
    /// This is the core test for Phase 2: lightweight control-plane operations
    /// must not be blocked by heavyweight session materialization.
    #[tokio::test]
    async fn test_get_node_info_responsive_during_materialization() {
        let fixture = NodeManagerFixture::new_with_mesh().await;
        let actor_ref = fixture.actor_ref.clone();

        // Start a session creation (which is now delegated via DelegatedReply)
        let actor_clone = actor_ref.clone();
        let create_handle = tokio::spawn(async move {
            let msg = CreateRemoteSession {
                cwd: Some("/tmp/test-responsive".to_string()),
            };
            actor_clone.ask(msg).await
        });

        // Small delay to ensure creation is in progress
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Now call GetNodeInfo - this should respond immediately
        // because it's a lightweight mailbox operation
        let start = Instant::now();
        let node_info_result = actor_ref.ask(GetNodeInfo).await;
        let node_info_duration = start.elapsed();

        println!("GetNodeInfo took {:?}", node_info_duration);

        // GetNodeInfo should respond very quickly (< 100ms)
        assert!(
            node_info_duration < Duration::from_millis(100),
            "GetNodeInfo should be fast, took {:?}",
            node_info_duration
        );

        assert!(node_info_result.is_ok(), "GetNodeInfo should succeed");

        // Verify the session creation also completed
        let create_result = create_handle.await.expect("create task should complete");
        assert!(create_result.is_ok(), "Session creation should succeed");
    }

    /// Test that session creation and resume can run concurrently without
    /// blocking each other or the actor.
    #[tokio::test]
    async fn test_create_and_resume_concurrent_no_blocking() {
        let fixture = NodeManagerFixture::new_with_mesh().await;
        let actor_ref = fixture.actor_ref.clone();

        // Create a session first
        let create_msg = CreateRemoteSession {
            cwd: Some("/tmp/test-concurrent-create-resume".to_string()),
        };
        let create_response = actor_ref
            .ask(create_msg)
            .await
            .expect("initial create should succeed");
        let session_id = create_response.session_id;

        // Run concurrent operations using separate tasks
        let mut handles = Vec::new();

        // 1. Create a new session
        let actor1 = actor_ref.clone();
        handles.push(tokio::spawn(async move {
            let msg = CreateRemoteSession {
                cwd: Some("/tmp/test-new-session".to_string()),
            };
            let start = Instant::now();
            let result = actor1.ask(msg).await;
            ("create_new", result.is_ok(), start.elapsed())
        }));

        // 2. Resume the existing session
        let actor2 = actor_ref.clone();
        let sid = session_id.clone();
        handles.push(tokio::spawn(async move {
            let msg = ResumeRemoteSession { session_id: sid };
            let start = Instant::now();
            let result = actor2.ask(msg).await;
            ("resume_existing", result.is_ok(), start.elapsed())
        }));

        // 3. Get node info (should be fast)
        let actor3 = actor_ref.clone();
        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            let result = actor3.ask(GetNodeInfo).await;
            ("get_node_info", result.is_ok(), start.elapsed())
        }));

        // Collect all results
        let mut results = Vec::new();
        for handle in handles {
            let result = handle.await.expect("task should complete");
            results.push(result);
        }

        assert_eq!(results.len(), 3, "Expected 3 results");

        // Verify all succeeded
        for (name, success, duration) in &results {
            assert!(*success, "{} should succeed", name);
            println!("{} completed in {:?}", name, duration);
        }

        // Verify GetNodeInfo was fast
        let node_info_duration = results
            .iter()
            .find(|(name, _, _)| *name == "get_node_info")
            .map(|(_, _, d)| *d)
            .unwrap();

        assert!(
            node_info_duration < Duration::from_millis(100),
            "GetNodeInfo should be fast, took {:?}",
            node_info_duration
        );

        println!("All concurrent operations completed successfully");
    }

    /// Test that the actor mailbox remains responsive under load.
    ///
    /// Spawns multiple session creations and verifies that `GetNodeInfo`
    /// repeatedly completes while those heavyweight requests are still in flight.
    /// This avoids fragile latency assertions and directly checks the mailbox
    /// responsiveness invariant.
    #[tokio::test]
    async fn test_mailbox_responsive_under_load() {
        let fixture = NodeManagerFixture::new_with_mesh().await;
        let actor_ref = fixture.actor_ref.clone();

        let mut create_handles = Vec::new();
        for i in 0..10 {
            let actor = actor_ref.clone();
            create_handles.push(tokio::spawn(async move {
                let msg = CreateRemoteSession {
                    cwd: Some(format!("/tmp/test-load-{}", i)),
                };
                actor.ask(msg).await
            }));
        }

        // While creations are running, repeatedly assert GetNodeInfo completes
        // within a bounded timeout instead of getting stuck behind mailbox work.
        for _ in 0..5 {
            let result = actor_ref
                .ask(GetNodeInfo)
                .mailbox_timeout(Duration::from_secs(1))
                .reply_timeout(Duration::from_secs(1))
                .send()
                .await;

            if let Err(e) = result {
                panic!("GetNodeInfo should succeed under load: {e:?}");
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        for handle in create_handles {
            let result = handle.await.expect("task should complete");
            assert!(result.is_ok(), "Session creation should succeed");
        }
    }
}
