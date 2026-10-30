//! Side-effect-free runtime reads: no profile/session materialization or remote replay.
use super::*;
use crate::agent::messages::SessionRuntimeStatus;
use crate::session::location::SessionLocation;
use crate::session::run_lease::RunLeaseRoot;
use futures_util::{StreamExt, stream};
use std::collections::HashMap;
use std::time::Duration;

const OBSERVATION_BUDGET: Duration = Duration::from_secs(2);
const OBSERVATION_CONCURRENCY: usize = 16;
type ActorMap = HashMap<String, SessionActorRef>;
type ProfileActors = HashMap<(String, Option<String>), ActorMap>;

// Observation probes must not mistake one another for execution owners. Stripes
// bound lock storage; canonical roots also serialize aliases of the same lease.
static PROBE_LOCKS: std::sync::LazyLock<[parking_lot::Mutex<()>; 64]> =
    std::sync::LazyLock::new(|| std::array::from_fn(|_| parking_lot::Mutex::new(())));

fn probe_lock_index(root: &std::path::Path, session_id: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    session_id.hash(&mut hasher);
    (hasher.finish() % PROBE_LOCKS.len() as u64) as usize
}

async fn observe_lease_free(root: RunLeaseRoot, session_id: &str) -> std::io::Result<bool> {
    let worker_root = root.clone();
    let id = session_id.to_string();
    let free = tokio::task::spawn_blocking(move || {
        worker_root.lease_path(&id)?;
        std::fs::create_dir_all(worker_root.path())?;
        let canonical = std::fs::canonicalize(worker_root.path())?;
        let _probe = PROBE_LOCKS[probe_lock_index(&canonical, &id)].lock();
        // Return only a verdict: the actual lease is dropped before the worker
        // releases its probe lock, never at a delayed async result handoff.
        Ok::<_, std::io::Error>(worker_root.try_acquire(&id)?.is_some())
    })
    .await
    .map_err(std::io::Error::other)??;
    #[cfg(test)]
    pause_probe_result(root.path(), session_id).await;
    Ok(free)
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct ProbePause {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(test)]
type ProbePauses = HashMap<(std::path::PathBuf, String), Arc<ProbePause>>;
#[cfg(test)]
static PROBE_PAUSES: std::sync::LazyLock<parking_lot::Mutex<ProbePauses>> =
    std::sync::LazyLock::new(Default::default);
#[cfg(test)]
pub(crate) fn pause_next_probe(root: &std::path::Path, session_id: &str) -> Arc<ProbePause> {
    let pause = Arc::new(ProbePause::default());
    PROBE_PAUSES
        .lock()
        .insert((root.to_path_buf(), session_id.to_string()), pause.clone());
    pause
}
#[cfg(test)]
async fn pause_probe_result(root: &std::path::Path, session_id: &str) {
    let pause = PROBE_PAUSES
        .lock()
        .remove(&(root.to_path_buf(), session_id.to_string()));
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.release.notified().await;
    }
}

#[cfg(test)]
#[tokio::test]
async fn probe_workers_serialize_canonical_roots_before_trying_the_lease() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let sid = "probe-worker-overlap";
    let (release, held) = std::sync::mpsc::channel();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let holder = std::thread::spawn(move || {
        let _guard = PROBE_LOCKS[probe_lock_index(&canonical, sid)].lock();
        entered.send(()).unwrap();
        held.recv_timeout(Duration::from_secs(2)).unwrap();
    });
    ready.await.unwrap();
    let root = RunLeaseRoot::new(dir.path().to_path_buf());
    let alias = RunLeaseRoot::new(dir.path().join("."));
    let probes = async {
        tokio::join!(
            observe_lease_free(root, sid),
            observe_lease_free(alias, sid)
        )
    };
    tokio::pin!(probes);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut probes)
            .await
            .is_err(),
        "same-root probes must serialize before acquiring any lease"
    );
    release.send(()).unwrap();
    let (first, second) = tokio::time::timeout(Duration::from_secs(1), probes)
        .await
        .unwrap();
    holder.join().unwrap();
    assert!(first.unwrap());
    assert!(second.unwrap());
}

impl LocalAgentHandle {
    pub(crate) async fn observe_session_runtimes(
        &self,
        session_ids: &[String],
    ) -> HashMap<String, SessionRuntimeStatus> {
        let deadline = tokio::time::Instant::now() + OBSERVATION_BUDGET;
        let owners = tokio::time::timeout_at(deadline, async {
            let owned = self
                .registry
                .lock()
                .await
                .get_many(session_ids.iter().map(String::as_str));
            let mut root = owned.clone();
            {
                let registry = &self.config.agent_registry;
                for info in registry.list_agents() {
                    if let Some(handle) = registry.get_handle(&info.id)
                        && let Some(local) = handle.as_any().downcast_ref::<LocalAgentHandle>()
                    {
                        root.extend(
                            local
                                .registry
                                .lock()
                                .await
                                .get_many(session_ids.iter().map(String::as_str)),
                        );
                    }
                }
            }
            let mut profiles = ProfileActors::new();
            if let Some(manager) = self.profiles() {
                for runtime in manager.materialized_runtimes().await {
                    let mut ids = vec![None];
                    if let Some(handle) = runtime.session_handle(None)
                        && let Some(local) = handle.as_any().downcast_ref::<LocalAgentHandle>()
                    {
                        ids.extend(
                            local
                                .config
                                .agent_registry
                                .list_agents()
                                .into_iter()
                                .map(|info| Some(info.id)),
                        );
                    }
                    for id in ids {
                        if let Some(handle) = runtime.session_handle(id.as_deref())
                            && let Some(local) = handle.as_any().downcast_ref::<LocalAgentHandle>()
                        {
                            profiles.insert(
                                (runtime.profile_id().to_string(), id),
                                local
                                    .registry
                                    .lock()
                                    .await
                                    .get_many(session_ids.iter().map(String::as_str)),
                            );
                        }
                    }
                }
            }
            (owned, root, profiles)
        })
        .await;
        let Ok((owned, root, profiles)) = owners else {
            return session_ids
                .iter()
                .map(|id| {
                    (
                        id.clone(),
                        SessionRuntimeStatus::unavailable("observation_timeout"),
                    )
                })
                .collect();
        };
        // One deadline covers resolution, locks, and all rows, not N successive RPC timeouts.
        stream::iter(session_ids.iter().cloned().map(|session_id| {
            let owned = &owned;
            let root = &root;
            let profiles = &profiles;
            async move {
                let state = tokio::time::timeout_at(
                    deadline,
                    self.observe_session_runtime(&session_id, owned, root, profiles),
                )
                .await
                .unwrap_or_else(|_| SessionRuntimeStatus::unavailable("observation_timeout"));
                (session_id, state)
            }
        }))
        .buffer_unordered(OBSERVATION_CONCURRENCY)
        .collect()
        .await
    }

    async fn observe_session_runtime(
        &self,
        session_id: &str,
        owned: &ActorMap,
        root: &ActorMap,
        profiles: &ProfileActors,
    ) -> SessionRuntimeStatus {
        let store = self.config.provider.history_store();
        let location =
            match crate::session::location::resolve_session_location(store.as_ref(), session_id)
                .await
            {
                Ok(location) => location,
                Err(_) => return SessionRuntimeStatus::unavailable("storage_unavailable"),
            };
        let actor = match &location {
            SessionLocation::Remote { .. } => root.get(session_id),
            SessionLocation::Local => {
                // Read persisted routing directly. ProfileRuntimeManager::session_binding may build a profile.
                match store.get_session_runtime_binding(session_id).await {
                    Ok(Some(binding)) => profiles
                        .get(&(binding.profile_id, binding.agent_id))
                        .and_then(|actors| actors.get(session_id))
                        .or_else(|| {
                            // A directly used profile/delegate owns its registry without a manager.
                            // Server-root lookups must still honor the persisted profile route.
                            if self.profiles().is_none() {
                                owned.get(session_id).filter(|actor| !actor.is_remote())
                            } else {
                                None
                            }
                        }),
                    Ok(None) => root.get(session_id),
                    Err(_) => return SessionRuntimeStatus::unavailable("binding_unavailable"),
                }
            }
            SessionLocation::Conflict { .. } => {
                return SessionRuntimeStatus::unavailable("location_conflict");
            }
            SessionLocation::NotFound => {
                return SessionRuntimeStatus::unavailable("session_not_found");
            }
        };
        if let Some(actor) = actor {
            let status = actor
                .get_runtime_status()
                .await
                .unwrap_or_else(|_| SessionRuntimeStatus::unavailable("owner_unavailable"));
            // An idle actor may outlive its own run while another process owns the session.
            // Never probe an active actor's lease: it legitimately holds that lease itself.
            if status.phase == crate::agent::messages::SessionRuntimePhase::Idle
                && matches!(location, SessionLocation::Local)
                && let Some(root) = self.config.storage.run_lease_root()
            {
                match observe_lease_free(RunLeaseRoot::new(root), session_id).await {
                    Ok(true) => {}
                    Ok(false) => return SessionRuntimeStatus::unavailable("live_owner_elsewhere"),
                    Err(_) => return SessionRuntimeStatus::unavailable("lease_unavailable"),
                }
            }
            return status;
        }
        if matches!(location, SessionLocation::Remote { .. }) {
            return SessionRuntimeStatus::unavailable("owner_unavailable");
        }
        // A missing registry entry is not proof of idle: another process may own the lease.
        let mut status = if let Some(root) = self.config.storage.run_lease_root() {
            match observe_lease_free(RunLeaseRoot::new(root), session_id).await {
                Ok(true) => SessionRuntimeStatus::Idle,
                Ok(false) => SessionRuntimeStatus::unavailable("live_owner_elsewhere"),
                Err(_) => SessionRuntimeStatus::unavailable("lease_unavailable"),
            }
        } else {
            SessionRuntimeStatus::unavailable("owner_unavailable")
        };
        status.delegation_activity = self.config.event_sink.live_delegation_activity(session_id);
        status
    }
}
