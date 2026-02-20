//! Tests for session repository implementations using in-memory SQLite.
//!
//! Each test creates a fresh in-memory database with the full schema applied,
//! then exercises the repository trait implementations.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use time::OffsetDateTime;

    use rusqlite::Connection;

    use crate::session::domain::{
        Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
        DelegationStatus, ForkOrigin, ForkPointType, IntentSnapshot, ProgressEntry, ProgressKind,
        Task, TaskKind, TaskStatus,
    };
    use crate::session::repository::{
        ArtifactRepository, DecisionRepository, DelegationRepository, IntentRepository,
        ProgressRepository, SessionRepository, TaskRepository,
    };
    use crate::session::schema;
    use crate::session::{
        SqliteArtifactRepository, SqliteDecisionRepository, SqliteDelegationRepository,
        SqliteIntentRepository, SqliteProgressRepository, SqliteSessionRepository,
        SqliteTaskRepository,
    };

    fn open_db() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        schema::init_schema(&mut conn).expect("schema init");
        Arc::new(Mutex::new(conn))
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    // =========================================================================
    // repo_session.rs
    // =========================================================================

    mod repo_session {
        use super::*;

        fn make_repo() -> SqliteSessionRepository {
            SqliteSessionRepository::new(open_db())
        }

        #[tokio::test]
        async fn create_and_get_session() {
            let repo = make_repo();
            let session = repo
                .create_session(Some("test".to_string()), None, None, None)
                .await
                .unwrap();

            assert!(!session.public_id.is_empty());
            assert_eq!(session.name.as_deref(), Some("test"));
            assert!(session.cwd.is_none());
            assert!(session.created_at.is_some());

            let fetched = repo.get_session(&session.public_id).await.unwrap();
            assert!(fetched.is_some());
            let fetched = fetched.unwrap();
            assert_eq!(fetched.public_id, session.public_id);
            assert_eq!(fetched.name.as_deref(), Some("test"));
        }

        #[tokio::test]
        async fn create_session_with_cwd() {
            let repo = make_repo();
            let cwd = PathBuf::from("/tmp/workspace");
            let session = repo
                .create_session(None, Some(cwd.clone()), None, None)
                .await
                .unwrap();
            let fetched = repo.get_session(&session.public_id).await.unwrap().unwrap();
            assert_eq!(fetched.cwd, Some(cwd));
        }

        #[tokio::test]
        async fn get_nonexistent_session_returns_none() {
            let repo = make_repo();
            let result = repo.get_session("nonexistent-id").await.unwrap();
            assert!(result.is_none());
        }

        #[tokio::test]
        async fn list_sessions_empty() {
            let repo = make_repo();
            let sessions = repo.list_sessions().await.unwrap();
            assert!(sessions.is_empty());
        }

        #[tokio::test]
        async fn list_sessions_returns_all() {
            let repo = make_repo();
            repo.create_session(Some("s1".to_string()), None, None, None)
                .await
                .unwrap();
            repo.create_session(Some("s2".to_string()), None, None, None)
                .await
                .unwrap();
            let sessions = repo.list_sessions().await.unwrap();
            assert_eq!(sessions.len(), 2);
        }

        #[tokio::test]
        async fn delete_session() {
            let repo = make_repo();
            let session = repo
                .create_session(Some("del-me".to_string()), None, None, None)
                .await
                .unwrap();
            repo.delete_session(&session.public_id).await.unwrap();
            let fetched = repo.get_session(&session.public_id).await.unwrap();
            assert!(fetched.is_none());
        }

        #[tokio::test]
        async fn delete_nonexistent_session_errors() {
            let repo = make_repo();
            let result = repo.delete_session("ghost-id").await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn fork_session_creates_child() {
            let repo = make_repo();
            let parent = repo
                .create_session(Some("parent".to_string()), None, None, None)
                .await
                .unwrap();

            let fork_id = repo
                .fork_session(
                    &parent.public_id,
                    ForkPointType::MessageIndex,
                    "msg-001",
                    ForkOrigin::User,
                    Some("try differently".to_string()),
                )
                .await
                .unwrap();

            assert!(!fork_id.is_empty());
            assert_ne!(fork_id, parent.public_id);

            let fork = repo.get_session(&fork_id).await.unwrap().unwrap();
            assert_eq!(fork.fork_origin, Some(ForkOrigin::User));
            assert_eq!(fork.fork_point_type, Some(ForkPointType::MessageIndex));
            assert_eq!(fork.fork_point_ref.as_deref(), Some("msg-001"));
            assert_eq!(fork.fork_instructions.as_deref(), Some("try differently"));
        }

        #[tokio::test]
        async fn get_session_fork_info_for_non_fork_returns_none() {
            let repo = make_repo();
            let session = repo.create_session(None, None, None, None).await.unwrap();
            let info = repo
                .get_session_fork_info(&session.public_id)
                .await
                .unwrap();
            assert!(info.is_none());
        }

        #[tokio::test]
        async fn get_session_fork_info_for_fork() {
            let repo = make_repo();
            let parent = repo.create_session(None, None, None, None).await.unwrap();
            let fork_id = repo
                .fork_session(
                    &parent.public_id,
                    ForkPointType::ProgressEntry,
                    "prog-42",
                    ForkOrigin::Delegation,
                    None,
                )
                .await
                .unwrap();

            let info = repo.get_session_fork_info(&fork_id).await.unwrap();
            assert!(info.is_some());
            let info = info.unwrap();
            assert_eq!(info.fork_origin, Some(ForkOrigin::Delegation));
            assert_eq!(info.fork_point_type, Some(ForkPointType::ProgressEntry));
            assert_eq!(info.fork_point_ref.as_deref(), Some("prog-42"));
        }

        #[tokio::test]
        async fn list_child_sessions() {
            let repo = make_repo();
            let parent = repo.create_session(None, None, None, None).await.unwrap();

            let fork1 = repo
                .fork_session(
                    &parent.public_id,
                    ForkPointType::MessageIndex,
                    "m1",
                    ForkOrigin::User,
                    None,
                )
                .await
                .unwrap();

            let fork2 = repo
                .fork_session(
                    &parent.public_id,
                    ForkPointType::MessageIndex,
                    "m2",
                    ForkOrigin::User,
                    None,
                )
                .await
                .unwrap();

            let children = repo.list_child_sessions(&parent.public_id).await.unwrap();
            assert_eq!(children.len(), 2);
            assert!(children.contains(&fork1));
            assert!(children.contains(&fork2));
        }
    }

    // =========================================================================
    // repo_task.rs
    // =========================================================================

    mod repo_task {
        use super::*;

        async fn make_session_and_repo() -> (String, i64, SqliteTaskRepository) {
            let db = open_db();
            let session_repo = SqliteSessionRepository::new(db.clone());
            let session = session_repo
                .create_session(None, None, None, None)
                .await
                .unwrap();
            let task_repo = SqliteTaskRepository::new(db);
            let session_internal_id = session.id;
            (session.public_id, session_internal_id, task_repo)
        }

        fn make_task(session_id: i64, kind: TaskKind, status: TaskStatus) -> Task {
            Task {
                id: 0,
                public_id: String::new(), // will be assigned by repo
                session_id,
                kind,
                status,
                expected_deliverable: Some("deliver X".to_string()),
                acceptance_criteria: Some("X is done".to_string()),
                created_at: now(),
                updated_at: now(),
            }
        }

        #[tokio::test]
        async fn create_and_get_task() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            let task = make_task(session_id, TaskKind::Finite, TaskStatus::Active);
            let created = repo.create_task(task).await.unwrap();

            assert!(!created.public_id.is_empty());
            assert_eq!(created.kind, TaskKind::Finite);
            assert_eq!(created.status, TaskStatus::Active);

            let fetched = repo.get_task(&created.public_id).await.unwrap();
            assert!(fetched.is_some());
            let fetched = fetched.unwrap();
            assert_eq!(fetched.public_id, created.public_id);
            assert_eq!(fetched.expected_deliverable.as_deref(), Some("deliver X"));

            // list_tasks uses public session_id
            let _ = session_public_id; // already fetched above
        }

        #[tokio::test]
        async fn list_tasks_for_session() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.create_task(make_task(session_id, TaskKind::Finite, TaskStatus::Active))
                .await
                .unwrap();
            repo.create_task(make_task(
                session_id,
                TaskKind::Recurring,
                TaskStatus::Paused,
            ))
            .await
            .unwrap();

            let tasks = repo.list_tasks(&session_public_id).await.unwrap();
            assert_eq!(tasks.len(), 2);
        }

        #[tokio::test]
        async fn update_task_status() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            let task = repo
                .create_task(make_task(session_id, TaskKind::Finite, TaskStatus::Active))
                .await
                .unwrap();

            repo.update_task_status(&task.public_id, TaskStatus::Done)
                .await
                .unwrap();

            let fetched = repo.get_task(&task.public_id).await.unwrap().unwrap();
            assert_eq!(fetched.status, TaskStatus::Done);
            let _ = session_public_id;
        }

        #[tokio::test]
        async fn delete_task() {
            let (_session_public_id, session_id, repo) = make_session_and_repo().await;
            let task = repo
                .create_task(make_task(session_id, TaskKind::Finite, TaskStatus::Active))
                .await
                .unwrap();
            repo.delete_task(&task.public_id).await.unwrap();
            let fetched = repo.get_task(&task.public_id).await.unwrap();
            assert!(fetched.is_none());
        }

        #[tokio::test]
        async fn get_nonexistent_task_returns_none() {
            let (_session_public_id, _session_id, repo) = make_session_and_repo().await;
            let result = repo.get_task("ghost").await.unwrap();
            assert!(result.is_none());
        }
    }

    // =========================================================================
    // repo_intent.rs
    // =========================================================================

    mod repo_intent {
        use super::*;

        async fn make_session_and_repo() -> (String, i64, SqliteIntentRepository) {
            let db = open_db();
            let session_repo = SqliteSessionRepository::new(db.clone());
            let session = session_repo
                .create_session(None, None, None, None)
                .await
                .unwrap();
            (
                session.public_id,
                session.id,
                SqliteIntentRepository::new(db),
            )
        }

        fn make_snapshot(session_id: i64, summary: &str) -> IntentSnapshot {
            IntentSnapshot {
                id: 0,
                session_id,
                task_id: None,
                summary: summary.to_string(),
                constraints: Some("no side effects".to_string()),
                next_step_hint: None,
                created_at: now(),
            }
        }

        #[tokio::test]
        async fn create_and_list_intent_snapshots() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.create_intent_snapshot(make_snapshot(session_id, "Build feature X"))
                .await
                .unwrap();
            repo.create_intent_snapshot(make_snapshot(session_id, "Refine feature X"))
                .await
                .unwrap();

            let snapshots = repo
                .list_intent_snapshots(&session_public_id)
                .await
                .unwrap();
            assert_eq!(snapshots.len(), 2);
        }

        #[tokio::test]
        async fn get_current_intent_snapshot_returns_latest() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.create_intent_snapshot(make_snapshot(session_id, "first"))
                .await
                .unwrap();
            repo.create_intent_snapshot(make_snapshot(session_id, "second"))
                .await
                .unwrap();

            let current = repo
                .get_current_intent_snapshot(&session_public_id)
                .await
                .unwrap();
            assert!(current.is_some());
            assert_eq!(current.unwrap().summary, "second");
        }

        #[tokio::test]
        async fn list_intent_snapshots_empty() {
            let (session_public_id, _session_id, repo) = make_session_and_repo().await;
            let snapshots = repo
                .list_intent_snapshots(&session_public_id)
                .await
                .unwrap();
            assert!(snapshots.is_empty());
        }
    }

    // =========================================================================
    // repo_decision.rs
    // =========================================================================

    mod repo_decision {
        use super::*;

        async fn make_session_and_repo() -> (String, i64, SqliteDecisionRepository) {
            let db = open_db();
            let session_repo = SqliteSessionRepository::new(db.clone());
            let session = session_repo
                .create_session(None, None, None, None)
                .await
                .unwrap();
            (
                session.public_id,
                session.id,
                SqliteDecisionRepository::new(db),
            )
        }

        fn make_decision(session_id: i64, desc: &str, status: DecisionStatus) -> Decision {
            Decision {
                id: 0,
                session_id,
                task_id: None,
                description: desc.to_string(),
                rationale: Some("because it's better".to_string()),
                status,
                created_at: now(),
            }
        }

        fn make_alternative(session_id: i64, desc: &str) -> Alternative {
            Alternative {
                id: 0,
                session_id,
                task_id: None,
                description: desc.to_string(),
                status: AlternativeStatus::Active,
                created_at: now(),
            }
        }

        #[tokio::test]
        async fn record_and_list_decisions() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.record_decision(make_decision(
                session_id,
                "Use approach A",
                DecisionStatus::Accepted,
            ))
            .await
            .unwrap();
            repo.record_decision(make_decision(
                session_id,
                "Skip step B",
                DecisionStatus::Rejected,
            ))
            .await
            .unwrap();

            let decisions = repo.list_decisions(&session_public_id, None).await.unwrap();
            assert_eq!(decisions.len(), 2);
        }

        #[tokio::test]
        async fn record_and_list_alternatives() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.record_alternative(make_alternative(session_id, "Alt approach A"))
                .await
                .unwrap();
            repo.record_alternative(make_alternative(session_id, "Alt approach B"))
                .await
                .unwrap();

            let alts = repo
                .list_alternatives(&session_public_id, None)
                .await
                .unwrap();
            assert_eq!(alts.len(), 2);
        }

        #[tokio::test]
        async fn update_decision_status() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.record_decision(make_decision(session_id, "Do X", DecisionStatus::Accepted))
                .await
                .unwrap();

            let decisions = repo.list_decisions(&session_public_id, None).await.unwrap();
            // Decision uses integer id as identifier
            let decision_id = decisions[0].id.to_string();

            repo.update_decision_status(&decision_id, DecisionStatus::Rejected)
                .await
                .unwrap();

            let updated = repo.get_decision(&decision_id).await.unwrap().unwrap();
            assert_eq!(updated.status, DecisionStatus::Rejected);
        }

        #[tokio::test]
        async fn update_alternative_status() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.record_alternative(make_alternative(session_id, "Alt X"))
                .await
                .unwrap();

            let alts = repo
                .list_alternatives(&session_public_id, None)
                .await
                .unwrap();
            // Alternative uses integer id as identifier
            let alt_id = alts[0].id.to_string();

            repo.update_alternative_status(&alt_id, AlternativeStatus::Discarded)
                .await
                .unwrap();

            let updated_alts = repo
                .list_alternatives(&session_public_id, None)
                .await
                .unwrap();
            assert_eq!(updated_alts[0].status, AlternativeStatus::Discarded);
        }

        #[tokio::test]
        async fn list_decisions_empty() {
            let (session_public_id, _session_id, repo) = make_session_and_repo().await;
            let decisions = repo.list_decisions(&session_public_id, None).await.unwrap();
            assert!(decisions.is_empty());
        }
    }

    // =========================================================================
    // repo_progress.rs
    // =========================================================================

    mod repo_progress {
        use super::*;

        async fn make_session_and_repo() -> (String, i64, SqliteProgressRepository) {
            let db = open_db();
            let session_repo = SqliteSessionRepository::new(db.clone());
            let session = session_repo
                .create_session(None, None, None, None)
                .await
                .unwrap();
            (
                session.public_id,
                session.id,
                SqliteProgressRepository::new(db),
            )
        }

        fn make_entry(session_id: i64, kind: ProgressKind, content: &str) -> ProgressEntry {
            ProgressEntry {
                id: 0,
                session_id,
                task_id: None,
                kind,
                content: content.to_string(),
                metadata: None,
                created_at: now(),
            }
        }

        #[tokio::test]
        async fn append_and_list_progress_entries() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.append_progress_entry(make_entry(
                session_id,
                ProgressKind::ToolCall,
                "called shell",
            ))
            .await
            .unwrap();
            repo.append_progress_entry(make_entry(session_id, ProgressKind::Note, "observed X"))
                .await
                .unwrap();

            let entries = repo
                .list_progress_entries(&session_public_id, None)
                .await
                .unwrap();
            assert_eq!(entries.len(), 2);
        }

        #[tokio::test]
        async fn list_progress_by_kind_filters_correctly() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.append_progress_entry(make_entry(session_id, ProgressKind::ToolCall, "tc1"))
                .await
                .unwrap();
            repo.append_progress_entry(make_entry(session_id, ProgressKind::Note, "note1"))
                .await
                .unwrap();
            repo.append_progress_entry(make_entry(session_id, ProgressKind::ToolCall, "tc2"))
                .await
                .unwrap();

            let tool_calls = repo
                .list_progress_by_kind(&session_public_id, ProgressKind::ToolCall)
                .await
                .unwrap();
            assert_eq!(tool_calls.len(), 2);

            let notes = repo
                .list_progress_by_kind(&session_public_id, ProgressKind::Note)
                .await
                .unwrap();
            assert_eq!(notes.len(), 1);
        }

        #[tokio::test]
        async fn list_progress_entries_empty() {
            let (session_public_id, _session_id, repo) = make_session_and_repo().await;
            let entries = repo
                .list_progress_entries(&session_public_id, None)
                .await
                .unwrap();
            assert!(entries.is_empty());
        }
    }

    // =========================================================================
    // repo_artifact.rs
    // =========================================================================

    mod repo_artifact {
        use super::*;

        async fn make_session_and_repo() -> (String, i64, SqliteArtifactRepository) {
            let db = open_db();
            let session_repo = SqliteSessionRepository::new(db.clone());
            let session = session_repo
                .create_session(None, None, None, None)
                .await
                .unwrap();
            (
                session.public_id,
                session.id,
                SqliteArtifactRepository::new(db),
            )
        }

        fn make_artifact(session_id: i64, kind: &str) -> Artifact {
            Artifact {
                id: 0,
                session_id,
                task_id: None,
                kind: kind.to_string(),
                uri: Some("file:///tmp/out.txt".to_string()),
                path: Some("/tmp/out.txt".to_string()),
                summary: Some("output file".to_string()),
                created_at: now(),
            }
        }

        #[tokio::test]
        async fn record_and_list_artifacts() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.record_artifact(make_artifact(session_id, "file"))
                .await
                .unwrap();
            repo.record_artifact(make_artifact(session_id, "patch"))
                .await
                .unwrap();

            let artifacts = repo.list_artifacts(&session_public_id, None).await.unwrap();
            assert_eq!(artifacts.len(), 2);
        }

        #[tokio::test]
        async fn list_artifacts_by_kind() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.record_artifact(make_artifact(session_id, "file"))
                .await
                .unwrap();
            repo.record_artifact(make_artifact(session_id, "patch"))
                .await
                .unwrap();
            repo.record_artifact(make_artifact(session_id, "file"))
                .await
                .unwrap();

            let file_artifacts = repo
                .list_artifacts_by_kind(&session_public_id, "file")
                .await
                .unwrap();
            assert_eq!(file_artifacts.len(), 2);

            let patch_artifacts = repo
                .list_artifacts_by_kind(&session_public_id, "patch")
                .await
                .unwrap();
            assert_eq!(patch_artifacts.len(), 1);
        }

        #[tokio::test]
        async fn list_artifacts_empty() {
            let (session_public_id, _session_id, repo) = make_session_and_repo().await;
            let artifacts = repo.list_artifacts(&session_public_id, None).await.unwrap();
            assert!(artifacts.is_empty());
        }

        #[tokio::test]
        async fn get_artifact_by_id() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.record_artifact(make_artifact(session_id, "file"))
                .await
                .unwrap();

            let artifacts = repo.list_artifacts(&session_public_id, None).await.unwrap();
            // Artifact uses integer id as identifier
            let artifact_id = artifacts[0].id.to_string();

            let fetched = repo.get_artifact(&artifact_id).await.unwrap();
            assert!(fetched.is_some());
            assert_eq!(fetched.unwrap().kind, "file");
        }
    }

    // =========================================================================
    // repo_delegation.rs
    // =========================================================================

    mod repo_delegation {
        use super::*;
        use crate::hash::RapidHash;

        async fn make_session_and_repo() -> (String, i64, SqliteDelegationRepository) {
            let db = open_db();
            let session_repo = SqliteSessionRepository::new(db.clone());
            let session = session_repo
                .create_session(None, None, None, None)
                .await
                .unwrap();
            (
                session.public_id,
                session.id,
                SqliteDelegationRepository::new(db),
            )
        }

        fn make_delegation(session_id: i64, target: &str, objective: &str) -> Delegation {
            Delegation {
                id: 0,
                public_id: String::new(),
                session_id,
                task_id: None,
                target_agent_id: target.to_string(),
                objective: objective.to_string(),
                objective_hash: RapidHash::new(objective.as_bytes()),
                context: None,
                constraints: None,
                expected_output: None,
                verification_spec: None,
                planning_summary: None,
                status: DelegationStatus::Requested,
                retry_count: 0,
                created_at: now(),
                completed_at: None,
            }
        }

        #[tokio::test]
        async fn create_and_get_delegation() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            let d = make_delegation(session_id, "coder-agent", "Write a function");
            let created = repo.create_delegation(d).await.unwrap();

            assert!(!created.public_id.is_empty());
            assert_eq!(created.target_agent_id, "coder-agent");
            assert_eq!(created.status, DelegationStatus::Requested);

            let fetched = repo.get_delegation(&created.public_id).await.unwrap();
            assert!(fetched.is_some());
            assert_eq!(fetched.unwrap().objective, "Write a function");

            let _ = session_public_id;
        }

        #[tokio::test]
        async fn list_delegations_for_session() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            repo.create_delegation(make_delegation(session_id, "agent-a", "obj-1"))
                .await
                .unwrap();
            repo.create_delegation(make_delegation(session_id, "agent-b", "obj-2"))
                .await
                .unwrap();

            let delegations = repo.list_delegations(&session_public_id).await.unwrap();
            assert_eq!(delegations.len(), 2);
        }

        #[tokio::test]
        async fn update_delegation_status() {
            let (session_public_id, session_id, repo) = make_session_and_repo().await;
            let created = repo
                .create_delegation(make_delegation(session_id, "agent-x", "run task"))
                .await
                .unwrap();

            repo.update_delegation_status(&created.public_id, DelegationStatus::Running)
                .await
                .unwrap();

            let fetched = repo
                .get_delegation(&created.public_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(fetched.status, DelegationStatus::Running);

            repo.update_delegation_status(&created.public_id, DelegationStatus::Complete)
                .await
                .unwrap();

            let fetched2 = repo
                .get_delegation(&created.public_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(fetched2.status, DelegationStatus::Complete);

            let _ = session_public_id;
        }

        #[tokio::test]
        async fn list_delegations_empty() {
            let (session_public_id, _session_id, repo) = make_session_and_repo().await;
            let delegations = repo.list_delegations(&session_public_id).await.unwrap();
            assert!(delegations.is_empty());
        }

        #[tokio::test]
        async fn get_nonexistent_delegation_returns_none() {
            let (_session_public_id, _session_id, repo) = make_session_and_repo().await;
            let result = repo.get_delegation("ghost").await.unwrap();
            assert!(result.is_none());
        }
    }
}
