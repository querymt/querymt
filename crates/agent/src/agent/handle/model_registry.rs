use super::*;

// ── Model registry convenience ────────────────────────────────────────────

impl LocalAgentHandle {
    /// Invalidate the model cache, forcing a fresh enumeration on next call.
    ///
    /// Delegates to `ModelInventory::invalidate_all`.
    pub async fn invalidate_model_cache(&self) {
        self.model_inventory.invalidate_all().await;
    }

    /// Attempt to re-attach a remote session through the shared coordinator.
    #[cfg(feature = "remote")]
    pub async fn reattach_from_bookmark(
        &self,
        bookmark: &crate::session::store::RemoteSessionBookmark,
    ) -> Result<crate::agent::remote::SessionActorRef, crate::error::AgentError> {
        self.ensure_remote_session_connected(
            &bookmark.session_id,
            Some(&bookmark.node_id),
            super::remote_connect::RemoteConnectReason::StartupReattach,
        )
        .await
        .map(|connected| connected.session_ref)
        .map_err(|error| crate::error::AgentError::RemoteActor(error.to_string()))
    }

    /// Resolve a `SessionHandoff` into a concrete remote actor reference.
    ///
    /// - `DirectRemote` → return the embedded ref directly.
    /// - `LookupOnly` → DHT lookup for the session.
    /// - `NoAttachPath` → error.
    #[cfg(feature = "remote")]
    pub async fn resolve_handoff(
        &self,
        session_id: &str,
        handoff: crate::agent::remote::node_manager::SessionHandoff,
    ) -> Result<
        kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        agent_client_protocol::Error,
    > {
        use crate::error::AgentError;

        match handoff {
            crate::agent::remote::node_manager::SessionHandoff::DirectRemote { session_ref } => {
                Ok(session_ref)
            }
            crate::agent::remote::node_manager::SessionHandoff::LookupOnly => {
                let mesh = self.mesh().ok_or_else(|| {
                    agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped)
                })?;
                let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
                for scope in runtime.active_scopes() {
                    let dht_name = crate::agent::remote::scope::scoped_session(&scope, session_id);
                    if let Some(found) = runtime
                        .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name)
                        .await
                        .map_err(|e| {
                            agent_client_protocol::Error::from(AgentError::SwarmLookupFailed {
                                key: session_id.to_string(),
                                reason: e.to_string(),
                            })
                        })?
                    {
                        return Ok(found);
                    }
                }
                Err(agent_client_protocol::Error::from(
                    AgentError::RemoteSessionNotFound {
                        details: format!(
                            "session {} registered but not found in DHT after lookup",
                            session_id
                        ),
                    },
                ))
            }
            crate::agent::remote::node_manager::SessionHandoff::NoAttachPath => Err(
                agent_client_protocol::Error::from(AgentError::RemoteSessionNotFound {
                    details: format!(
                        "session {} was created but the remote node cannot provide an attach path",
                        session_id
                    ),
                }),
            ),
        }
    }

    /// Make a connected remote session's history local, then build its load
    /// snapshot from the local journal (the same builder local sessions use).
    ///
    /// The whole transfer shares one budget (60 s by default). A failed or
    /// timed-out transfer is served from the local copy, marked stale, only
    /// when the requested window is already local; otherwise the load fails
    /// with a retryable `remote_transfer_failed` error.
    #[cfg(feature = "remote")]
    pub(crate) async fn load_remote_snapshot(
        &self,
        session_id: &str,
        connected: &super::remote_connect::ConnectedRemoteSession,
        window: Option<crate::session::history_window::LoadWindow>,
    ) -> Result<serde_json::Value, agent_client_protocol::Error> {
        // One deadline for the sync and the snapshot's own backfill.
        let deadline = tokio::time::Instant::now() + remote_transfer_budget();
        let sync = match tokio::time::timeout_at(
            deadline,
            self.sync_remote_for_load(session_id, connected, window.is_some()),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(crate::error::AgentError::Internal(
                "history transfer timed out".into(),
            )),
        };
        if let Err(error) = &sync {
            log::warn!("remote history sync before load failed: session_id={session_id}: {error}");
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        self.build_remote_attach_snapshot_with_budget(session_id, window, sync, remaining)
            .await
    }

    /// The full event history of an attached remote session, read from the
    /// local journal in the owner's order (`seq` = owner position).
    ///
    /// Waits for any running sync and transfers only ranges that are not yet
    /// local, so a load followed by a connection subscription moves each event
    /// across the mesh once. `Ok(None)` when the session is not a remote
    /// session with a bookmark (the caller keeps its existing source).
    #[cfg(feature = "remote")]
    pub(crate) async fn remote_session_events_from_journal(
        &self,
        session_id: &str,
        session_ref: &crate::agent::remote::SessionActorRef,
    ) -> Result<Option<Vec<crate::events::AgentEvent>>, crate::error::AgentError> {
        if !session_ref.is_remote() {
            return Ok(None);
        }
        let Some(bookmark) = self
            .config
            .provider
            .history_store()
            .get_remote_session_bookmark(session_id)
            .await
            .map_err(|error| crate::error::AgentError::Internal(error.to_string()))?
        else {
            return Ok(None);
        };
        let sink = self.config.event_sink.clone();
        let state = sink.remote_sync(session_id, &bookmark.node_id);
        let internal = |error: crate::session::error::SessionError| {
            crate::error::AgentError::Internal(error.to_string())
        };
        state.hydrate().await.map_err(internal)?;
        let deadline = tokio::time::Instant::now() + remote_transfer_budget();
        let _ = tokio::time::timeout_at(deadline, state.wait_synced()).await;
        let tip = state.tip();
        if tip > 0 && !state.is_covered(1, tip) {
            let fetch = crate::agent::remote::event_backfill::RemoteSync::new(
                sink.clone(),
                session_ref.clone(),
                session_id,
                &bookmark.node_id,
                &bookmark.peer_label,
            );
            match tokio::time::timeout_at(deadline, fetch.ensure_covered(1, tip)).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(crate::error::AgentError::Internal(
                        "history transfer timed out".into(),
                    ));
                }
            }
        }
        let history = sink
            .journal()
            .load_remote_history(session_id, &bookmark.node_id)
            .await
            .map_err(internal)?;
        // Nothing synced and nothing local: let the caller use its own source.
        if tip == 0 && history.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            history
                .into_iter()
                .map(|(position, durable)| {
                    let mut event = crate::events::AgentEvent::from(durable);
                    event.seq = position;
                    event
                })
                .collect(),
        ))
    }

    /// A snapshot served purely from the local copy when the owner cannot be
    /// reached at all; `None` when the requested window is not already local.
    #[cfg(feature = "remote")]
    pub(crate) async fn stale_remote_snapshot(
        &self,
        session_id: &str,
        window: Option<crate::session::history_window::LoadWindow>,
    ) -> Option<serde_json::Value> {
        self.build_remote_attach_snapshot_with_budget(
            session_id,
            window,
            Err(crate::error::AgentError::Internal(
                "owner unreachable".into(),
            )),
            remote_transfer_budget(),
        )
        .await
        .ok()
    }

    /// Build the load snapshot of a remote session from the local journal,
    /// after the sync barrier (see [`Self::load_remote_snapshot`]).
    #[cfg(feature = "remote")]
    pub(crate) async fn build_remote_attach_snapshot_with_budget(
        &self,
        session_id: &str,
        window: Option<crate::session::history_window::LoadWindow>,
        sync: Result<(), crate::error::AgentError>,
        budget: std::time::Duration,
    ) -> Result<serde_json::Value, agent_client_protocol::Error> {
        use crate::error::AgentError;
        let transfer_failed = |message: String| {
            agent_client_protocol::Error::from(AgentError::RemoteTransferFailed {
                session_id: session_id.to_string(),
                message,
            })
        };
        let internal = |message: String| {
            agent_client_protocol::Error::internal_error().data(serde_json::json!({
                "message": message,
                "sessionId": session_id,
            }))
        };
        let bookmark = self
            .config
            .provider
            .history_store()
            .get_remote_session_bookmark(session_id)
            .await
            .map_err(|error| internal(error.to_string()))?
            .ok_or_else(|| {
                agent_client_protocol::Error::from(AgentError::SessionNotFound {
                    session_id: session_id.to_string(),
                })
            })?;
        let view_store = self
            .config
            .storage
            .view_store()
            .ok_or_else(|| internal("view store unavailable".into()))?;
        let sink = self.config.event_sink.clone();
        let state = sink.remote_sync(session_id, &bookmark.node_id);
        let deadline = tokio::time::Instant::now() + budget;

        // Barrier: whatever sync is running (or just ran) must finish first.
        let mut stale = sync.is_err();
        state
            .hydrate()
            .await
            .map_err(|error| internal(error.to_string()))?;
        if !stale
            && tokio::time::timeout_at(deadline, state.wait_synced())
                .await
                .is_err()
        {
            stale = true;
        }

        let probe = crate::agent::load_telemetry::LoadProbe::start();
        // Each pass moves the window's lower edge down by one chunk; the
        // absolute deadline is the only bound on the transfer.
        let mut snapshot = loop {
            let built = crate::session::load_snapshot::load_session_snapshot_windowed(
                self,
                view_store.clone(),
                session_id,
                window,
            )
            .await
            .map_err(|error| internal(format!("Failed to build session load snapshot: {error}")))?;
            let Some((start, end)) = missing_older_range(&state, &built, window.is_some()) else {
                break built;
            };
            if stale || !state.has_data() && state.tip() == 0 && sync.is_err() {
                return Err(transfer_failed(
                    "the requested history is not available locally and the owner did not respond"
                        .into(),
                ));
            }
            // A window already in the local journal is served even with no
            // budget left; only a new transfer needs time.
            if tokio::time::Instant::now() >= deadline {
                return Err(transfer_failed("history transfer timed out".into()));
            }
            let session_ref = {
                let registry = self.registry.lock().await;
                registry.get(session_id).cloned()
            };
            let Some(session_ref) = session_ref else {
                return Err(transfer_failed("session is no longer attached".into()));
            };
            let fetch = crate::agent::remote::event_backfill::RemoteSync::new(
                sink.clone(),
                session_ref,
                session_id,
                &bookmark.node_id,
                &bookmark.peer_label,
            );
            match tokio::time::timeout_at(deadline, fetch.ensure_covered(start, end)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(transfer_failed(error.to_string())),
                Err(_) => return Err(transfer_failed("history transfer timed out".into())),
            }
        };
        if stale && !state.has_data() {
            return Err(transfer_failed(
                "the owner did not respond and no history is available locally".into(),
            ));
        }
        // Older history may still be filling: whole-session state covers only
        // what is local so far.
        if window.is_some()
            && state.tip() > 0
            && !state.is_covered(1, state.tip())
            && let Some(summary) = snapshot.summary.as_mut()
        {
            summary.usage.partial = true;
        }
        if stale {
            match snapshot.history.as_mut() {
                Some(history) => history.stale = Some(true),
                None => {
                    snapshot.history = Some(crate::session::history_window::HistoryInfo {
                        complete: true,
                        before: None,
                        stale: Some(true),
                    })
                }
            }
        }
        let value = serde_json::to_value(snapshot).map_err(|error| internal(error.to_string()))?;
        probe.log_finished("remote_attach", session_id, &value);
        Ok(value)
    }
}

#[cfg(feature = "remote")]
const DEFAULT_REMOTE_TRANSFER_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// Total budget for one load's remote history transfer, aligned with the
/// 60 s history reply timeout; overridable for operators.
#[cfg(feature = "remote")]
pub(crate) fn remote_transfer_budget() -> std::time::Duration {
    std::env::var("QUERYMT_REMOTE_TRANSFER_BUDGET_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(DEFAULT_REMOTE_TRANSFER_BUDGET)
}

/// The `source_seq` range that must be transferred before the snapshot is
/// valid, if any: a window must not span an uncovered gap nor claim to start
/// the history while older events are not local.
#[cfg(feature = "remote")]
fn missing_older_range(
    state: &crate::session::remote_sync::RemoteSyncState,
    snapshot: &crate::session::SessionLoadSnapshot,
    windowed: bool,
) -> Option<(i64, i64)> {
    let tip = state.tip();
    if tip <= 0 {
        return None;
    }
    if !windowed {
        return (!state.is_covered(1, tip)).then_some((1, tip));
    }
    let coverage = state.coverage();
    let &(top_start, _) = coverage.ranges().last()?;
    if top_start <= 1 {
        return None;
    }
    let first = snapshot.audit.events.first().map(|event| event.seq);
    let reached_top = match (&snapshot.history, first) {
        (Some(history), _) if history.complete => true,
        (_, Some(seq)) => seq <= top_start,
        (_, None) => true,
    };
    reached_top.then(|| {
        let start =
            (top_start - crate::agent::remote::event_backfill::WINDOW_CHUNK_EVENTS as i64).max(1);
        (start, top_start - 1)
    })
}

/// Run the load-time requests of a connected remote session concurrently.
///
/// Only the snapshot can fail the load. The profile lookup is already
/// best-effort, and a failed configuration request yields an empty default
/// (logged). Total latency is the slowest request, not their sum.
#[cfg(feature = "remote")]
pub(crate) async fn join_remote_load_requests<T, P, C, Sf, Pf, Cf>(
    session_id: &str,
    snapshot: Sf,
    profile: Pf,
    config: Cf,
) -> Result<(T, P, C), agent_client_protocol::Error>
where
    Sf: std::future::Future<Output = Result<T, agent_client_protocol::Error>>,
    Pf: std::future::Future<Output = P>,
    Cf: std::future::Future<Output = Result<C, agent_client_protocol::Error>>,
    C: Default,
{
    let (snapshot, profile, config) = tokio::join!(snapshot, profile, config);
    let config = config.unwrap_or_else(|error| {
        tracing::warn!(session_id, error = %error, "remote configuration unavailable during load");
        C::default()
    });
    Ok((snapshot?, profile, config))
}

#[cfg(all(test, feature = "remote"))]
mod join_tests {
    use super::join_remote_load_requests;
    use agent_client_protocol::Error;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn slow_profile_bounds_the_load_not_the_sum() {
        let start = tokio::time::Instant::now();
        let (snapshot, profile, config) = join_remote_load_requests(
            "s",
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, Error>(1)
            },
            async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                Some("profile")
            },
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, Error>(vec![7])
            },
        )
        .await
        .unwrap();
        assert_eq!((snapshot, profile, config), (1, Some("profile"), vec![7]));
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn config_failure_still_loads_without_options() {
        let (snapshot, _, config) = join_remote_load_requests(
            "s",
            async { Ok::<_, Error>(1) },
            async { None::<()> },
            async { Err::<Vec<u8>, _>(Error::internal_error()) },
        )
        .await
        .unwrap();
        assert_eq!(snapshot, 1);
        assert!(config.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_failure_fails_the_load() {
        let result = join_remote_load_requests(
            "s",
            async { Err::<u8, _>(Error::internal_error()) },
            async { None::<()> },
            async { Ok::<Vec<u8>, Error>(Vec::new()) },
        )
        .await;
        assert!(result.is_err());
    }
}
