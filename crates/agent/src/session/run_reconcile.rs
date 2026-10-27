//! Closing runs abandoned by a process that died.
//!
//! A run whose `run_started` has no `run_completed`, and whose session run
//! lease is free, has no live owner. Reconciliation journals
//! `run_completed { outcome: "interrupted" }` for it while holding the lease,
//! which serializes concurrent reconcilers and a racing run start.

use crate::agent::agent_config::AgentConfig;
use crate::events::{AgentEventKind, RunOutcome};
use crate::session::run_lease::RunLeaseRoot;

/// Upper bound on orphaned runs closed in one call (a journal can hold several
/// unmatched `run_started` events, e.g. after repeated crashes).
const MAX_RUNS_PER_RECONCILE: usize = 64;

/// Close every orphaned run of the session. Best-effort: failures are logged
/// and never propagated, so a load, cancel or stop is never blocked by it.
///
/// Returns the run ids that were closed, most recent first.
pub async fn reconcile_orphaned_run(config: &AgentConfig, session_id: &str) -> Vec<String> {
    let Some(root) = config.storage.run_lease_root() else {
        return Vec::new();
    };
    reconcile_with_root(config, session_id, RunLeaseRoot::new(root))
        .await
        .unwrap_or_default()
}

async fn reconcile_with_root(
    config: &AgentConfig,
    session_id: &str,
    root: RunLeaseRoot,
) -> Option<Vec<String>> {
    // Common case first: a session with no unfinished run costs one indexed
    // journal query and nothing else.
    let journal = config.event_sink.journal();
    match journal.last_unfinished_run(session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return None,
        Err(error) => {
            log::warn!(
                "run reconciliation skipped for {session_id}: journal query failed: {error}"
            );
            return None;
        }
    }

    // Remote-hosted sessions are closed by their host, never locally.
    match config
        .provider
        .history_store()
        .get_remote_session_bookmark(session_id)
        .await
    {
        Ok(None) => {}
        Ok(Some(_)) => return None,
        Err(error) => {
            log::warn!(
                "run reconciliation skipped for {session_id}: bookmark lookup failed: {error}"
            );
            return None;
        }
    }

    // Busy: the run is live (here or in another process), or the OS is still
    // releasing a dead process's lock. Either way, not now.
    // The lease file I/O runs on the blocking pool.
    let _lease = match root.acquire(session_id).await {
        Ok(Some(lease)) => lease,
        Ok(None) => return None,
        Err(error) => {
            log::warn!("run reconciliation skipped for {session_id}: lease error: {error}");
            return None;
        }
    };

    // Under the lease no run of this session can start, so every unmatched
    // run_started is an orphan. Re-query each time: another reconciler may
    // have closed some between the first query and the lock.
    let mut closed = Vec::new();
    for _ in 0..MAX_RUNS_PER_RECONCILE {
        let run_id = match journal.last_unfinished_run(session_id).await {
            Ok(Some(run_id)) => run_id,
            Ok(None) => break,
            Err(error) => {
                log::warn!(
                    "run reconciliation failed for {session_id}: journal query failed: {error}"
                );
                break;
            }
        };
        match config
            .event_sink
            .emit_durable(
                session_id,
                AgentEventKind::RunCompleted {
                    run_id: run_id.clone(),
                    outcome: RunOutcome::Interrupted.into(),
                },
            )
            .await
        {
            Ok(_) => {
                log::info!("closed orphaned run {run_id} of session {session_id} as interrupted");
                closed.push(run_id);
            }
            Err(error) => {
                log::warn!(
                    "failed to close orphaned run {run_id} of session {session_id}: {error}"
                );
                break;
            }
        }
    }
    Some(closed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::session::backend::StorageBackend;
    use crate::session::projection::NewDurableEvent;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session::store::SessionStore as _;
    use std::sync::Arc;

    const SID: &str = "01a17c4b-3b1f-72d2-99e1-16024ff942d5";

    pub(crate) struct Fixture {
        pub config: Arc<AgentConfig>,
        pub storage: Arc<SqliteStorage>,
        pub lease_root: RunLeaseRoot,
        pub _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    pub(crate) async fn fixture() -> Fixture {
        fixture_at(None).await
    }

    /// `db`: on-disk database (lease root derived beside it) instead of an
    /// in-memory one with an explicit temp lease root.
    pub(crate) async fn fixture_at(db: Option<std::path::PathBuf>) -> Fixture {
        use crate::test_utils::{
            MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
        };
        let provider = Arc::new(tokio::sync::Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider,
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared));
        let (plugin_registry, plugin_dir) = mock_plugin_registry(factory).expect("registry");
        let lease_dir = tempfile::TempDir::new().unwrap();
        let lease_path = lease_dir.path().join("leases");
        let storage = Arc::new(match &db {
            Some(path) => SqliteStorage::connect(path.clone()).await.unwrap(),
            None => SqliteStorage::connect(":memory:".into())
                .await
                .unwrap()
                .with_run_lease_root(lease_path.clone()),
        });
        let lease_path = storage.run_lease_root().unwrap();
        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                storage.clone(),
                querymt::LLMParams::new()
                    .provider("mock")
                    .model("mock-model"),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .build(),
        );
        Fixture {
            config,
            storage,
            lease_root: RunLeaseRoot::new(lease_path),
            _dirs: (plugin_dir, lease_dir),
        }
    }

    pub(crate) async fn seed(storage: &SqliteStorage, session: &str, kind: AgentEventKind) {
        use crate::session::projection::EventJournal;
        storage
            .append_durable(&NewDurableEvent {
                session_seq: None,
                session_id: session.to_string(),
                origin: crate::events::EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                kind,
            })
            .await
            .unwrap();
    }

    pub(crate) fn started(run_id: &str) -> AgentEventKind {
        AgentEventKind::RunStarted {
            run_id: run_id.into(),
            origin: "interactive".into(),
        }
    }

    pub(crate) async fn interrupted_count(storage: &SqliteStorage, session: &str) -> usize {
        use crate::session::projection::EventJournal;
        storage
            .load_session_stream(session, None, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| {
                matches!(&e.kind, AgentEventKind::RunCompleted { outcome, .. }
                    if outcome == RunOutcome::Interrupted.as_str())
            })
            .count()
    }

    #[tokio::test]
    async fn orphan_is_closed_once_and_repeat_is_noop() {
        let f = fixture().await;
        seed(&f.storage, SID, started("r1")).await;

        assert_eq!(reconcile_orphaned_run(&f.config, SID).await, vec!["r1"]);
        assert!(reconcile_orphaned_run(&f.config, SID).await.is_empty());
        assert_eq!(interrupted_count(&f.storage, SID).await, 1);
        // The lease is released afterwards.
        assert!(f.lease_root.try_acquire(SID).unwrap().is_some());
    }

    #[tokio::test]
    async fn held_lease_or_no_runs_writes_nothing() {
        let f = fixture().await;
        seed(&f.storage, SID, started("r1")).await;
        seed(&f.storage, "other", AgentEventKind::SessionCreated).await;

        let held = f.lease_root.try_acquire(SID).unwrap().unwrap();
        assert!(reconcile_orphaned_run(&f.config, SID).await.is_empty());
        assert_eq!(interrupted_count(&f.storage, SID).await, 0);
        drop(held);

        assert!(reconcile_orphaned_run(&f.config, "other").await.is_empty());
        assert_eq!(interrupted_count(&f.storage, "other").await, 0);
    }

    #[tokio::test]
    async fn remote_bookmarked_session_is_not_reconciled() {
        let f = fixture().await;
        seed(&f.storage, SID, started("r1")).await;
        f.storage
            .save_remote_session_bookmark(&crate::session::store::RemoteSessionBookmark {
                session_id: SID.to_string(),
                node_id: "node-a".to_string(),
                peer_label: "peer".to_string(),
                cwd: None,
                created_at: 0,
                title: None,
            })
            .await
            .unwrap();

        assert!(reconcile_orphaned_run(&f.config, SID).await.is_empty());
        assert_eq!(interrupted_count(&f.storage, SID).await, 0);
    }

    #[tokio::test]
    async fn all_orphans_are_closed_in_one_call() {
        let f = fixture().await;
        seed(&f.storage, SID, started("r1")).await;
        seed(&f.storage, SID, started("r2")).await;
        seed(&f.storage, SID, started("r3")).await;
        seed(
            &f.storage,
            SID,
            AgentEventKind::RunCompleted {
                run_id: "r2".into(),
                outcome: RunOutcome::Completed.into(),
            },
        )
        .await;

        assert_eq!(
            reconcile_orphaned_run(&f.config, SID).await,
            vec!["r3", "r1"]
        );
        assert_eq!(interrupted_count(&f.storage, SID).await, 2);
        assert!(reconcile_orphaned_run(&f.config, SID).await.is_empty());
    }

    /// A dismissed remote session loses its bookmark but keeps the events
    /// mirrored from its host. Those runs belong to the host: the lease is
    /// always free here, so only the event origin keeps them from being closed.
    #[tokio::test]
    async fn mirrored_remote_runs_are_not_reconciled_without_a_bookmark() {
        use crate::session::projection::EventJournal;
        let f = fixture().await;
        f.storage
            .append_durable_from_source(&NewDurableEvent {
                session_seq: None,
                session_id: SID.to_string(),
                origin: crate::events::EventOrigin::Remote,
                source_node: Some("peer".to_string()),
                source_node_id: Some("node-a".to_string()),
                source_seq: Some(1),
                kind: started("remote-run"),
            })
            .await
            .unwrap();
        assert!(
            f.storage
                .get_remote_session_bookmark(SID)
                .await
                .unwrap()
                .is_none()
        );

        assert!(reconcile_orphaned_run(&f.config, SID).await.is_empty());
        assert_eq!(interrupted_count(&f.storage, SID).await, 0);

        // A local orphan in the same session is still closed.
        seed(&f.storage, SID, started("local-run")).await;
        assert_eq!(
            reconcile_orphaned_run(&f.config, SID).await,
            vec!["local-run"]
        );
    }

    #[tokio::test]
    async fn no_lease_root_disables_reconciliation() {
        let f = fixture().await;
        let memory = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        assert!(StorageBackend::run_lease_root(&*memory).is_none());
        seed(&memory, SID, started("r1")).await;
        let config = Arc::new(
            AgentConfigBuilder::from_provider(
                memory.clone(),
                f.config.provider.clone(),
                memory.event_journal(),
            )
            .build(),
        );
        assert!(reconcile_orphaned_run(&config, SID).await.is_empty());
        assert_eq!(interrupted_count(&memory, SID).await, 0);
    }

    #[tokio::test]
    async fn concurrent_reconcilers_record_exactly_one_event() {
        let f = fixture().await;
        seed(&f.storage, SID, started("r1")).await;

        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let config = f.config.clone();
                tokio::spawn(async move { reconcile_orphaned_run(&config, SID).await })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(interrupted_count(&f.storage, SID).await, 1);
    }

    /// Helper process: reconcile the session of the database in `QMT_RECON_DB`.
    #[tokio::test]
    #[ignore]
    async fn reconcile_helper() {
        let db = std::path::PathBuf::from(std::env::var_os("QMT_RECON_DB").unwrap());
        let f = fixture_at(Some(db)).await;
        reconcile_orphaned_run(&f.config, SID).await;
    }

    #[tokio::test]
    async fn concurrent_processes_record_exactly_one_event() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("sessions.db");
        let f = fixture_at(Some(db.clone())).await;
        seed(&f.storage, SID, started("r1")).await;

        let exe = std::env::current_exe().unwrap();
        let children: Vec<_> = (0..2)
            .map(|_| {
                std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "session::run_reconcile::tests::reconcile_helper",
                        "--ignored",
                    ])
                    .env("QMT_RECON_DB", &db)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(interrupted_count(&f.storage, SID).await, 1);
    }

    /// Helper process of the incident test: journal `run_started`, hold the
    /// session's run lease, signal readiness, then wait to be killed.
    #[tokio::test]
    #[ignore]
    async fn incident_run_helper() {
        let db = std::path::PathBuf::from(std::env::var_os("QMT_INCIDENT_DB").unwrap());
        let session = std::env::var("QMT_INCIDENT_SESSION").unwrap();
        let marker = std::path::PathBuf::from(std::env::var_os("QMT_INCIDENT_MARKER").unwrap());
        let f = fixture_at(Some(db)).await;
        let _lease = f
            .lease_root
            .try_acquire(&session)
            .unwrap()
            .expect("lease free");
        f.config
            .emit_event_persisted(&session, started("incident-run"))
            .await
            .unwrap();
        std::fs::write(marker, b"running").unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }

    /// The reported incident: a process dies mid-run; a live process that
    /// loads the session must not show it as running forever.
    #[tokio::test]
    async fn killed_run_is_closed_as_interrupted_on_load() {
        use crate::session::load_snapshot::load_session_snapshot;

        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("sessions.db");
        let marker = dir.path().join("running.marker");
        let f = fixture_at(Some(db.clone())).await;
        let agent = crate::agent::LocalAgentHandle::from_config(f.config.clone());
        let session = f
            .storage
            .create_session(None, None, None, None)
            .await
            .unwrap()
            .public_id;

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::run_reconcile::tests::incident_run_helper",
                "--ignored",
            ])
            .env("QMT_INCIDENT_DB", &db)
            .env("QMT_INCIDENT_SESSION", &session)
            .env("QMT_INCIDENT_MARKER", &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !marker.exists() {
            assert!(std::time::Instant::now() < deadline, "child never started");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        let last_kind =
            |events: &[crate::events::AgentEvent]| events.last().map(|e| e.kind.clone());

        // While the owner is alive its run stays open.
        let live = load_session_snapshot(&agent, f.storage.view_store().unwrap(), &session)
            .await
            .unwrap();
        assert!(matches!(
            last_kind(&live.audit.events),
            Some(AgentEventKind::RunStarted { .. })
        ));

        child.kill().unwrap();
        child.wait().unwrap();

        // Windows may release a terminated process's locks asynchronously.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snapshot = load_session_snapshot(&agent, f.storage.view_store().unwrap(), &session)
                .await
                .unwrap();
            if matches!(
                last_kind(&snapshot.audit.events),
                Some(AgentEventKind::RunCompleted { ref run_id, ref outcome })
                    if run_id == "incident-run" && outcome == "interrupted"
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "orphaned run was never closed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(interrupted_count(&f.storage, &session).await, 1);
        // The session is free for a new run.
        assert!(f.lease_root.try_acquire(&session).unwrap().is_some());
    }
}
