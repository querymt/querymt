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
                match RunLeaseRoot::new(root).acquire(session_id).await {
                    Ok(Some(_lease)) => {}
                    Ok(None) => return SessionRuntimeStatus::unavailable("live_owner_elsewhere"),
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
            match RunLeaseRoot::new(root).acquire(session_id).await {
                Ok(Some(_lease)) => SessionRuntimeStatus::Idle,
                Ok(None) => SessionRuntimeStatus::unavailable("live_owner_elsewhere"),
                Err(_) => SessionRuntimeStatus::unavailable("lease_unavailable"),
            }
        } else {
            SessionRuntimeStatus::unavailable("owner_unavailable")
        };
        status.delegation_activity = self.config.event_sink.live_delegation_activity(session_id);
        status
    }
}
