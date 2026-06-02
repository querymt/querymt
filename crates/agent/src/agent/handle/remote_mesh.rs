use super::*;

impl LocalAgentHandle {
    // ── Remote session management (requires `remote` feature) ─────────────────

    /// List discovered peers in the kameo mesh.
    ///
    /// Looks up all `RemoteNodeManager` instances registered under
    /// `"node_manager"` in the Kademlia DHT and calls `GetNodeInfo` on each.
    /// Requires a bootstrapped swarm (`--mesh` flag).
    ///
    /// Without a swarm or with no peers, returns an empty list.
    /// Returns a clone of the `MeshHandle` if the mesh is active.
    #[cfg(feature = "remote")]
    pub fn mesh(&self) -> Option<crate::agent::remote::MeshHandle> {
        self.mesh.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Activate the mesh by storing the `MeshHandle` returned by `bootstrap_mesh()`.
    ///
    /// Also propagates into `config.provider` so that sessions created by a
    /// `RemoteNodeManager` (which holds `Arc<AgentConfig>` with this provider)
    /// can route LLM calls through the mesh even though the mesh was bootstrapped
    /// after the config was built.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&self, mesh: crate::agent::remote::MeshHandle) {
        *self.mesh.lock().unwrap_or_else(|e| e.into_inner()) = Some(mesh.clone());
        self.config.provider.set_mesh(Some(mesh.clone()));

        // Propagate to the session registry so remove/detach can clean up
        // re-registration closures (Phase 4 of Bug 1 fix).
        if let Ok(mut registry) = self.registry.try_lock() {
            registry.set_mesh(Some(mesh.clone()));
        }

        // Propagate to the session materializer for mesh-aware session creation
        self.session_materializer.set_mesh(mesh.clone());

        // Propagate to the model inventory for remote model enumeration
        self.model_inventory.set_mesh(mesh);
    }

    /// Enable/disable automatic mesh fallback for unpinned provider resolution.
    #[cfg(feature = "remote")]
    pub fn set_mesh_fallback(&self, enabled: bool) {
        self.config.provider.set_mesh_fallback(enabled);
    }

    #[cfg(feature = "remote")]
    pub async fn ensure_mesh_published(&self, node_name: Option<String>) -> anyhow::Result<()> {
        if node_name.is_some() {
            *self
                .local_mesh_node_name
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = node_name;
        }

        let mesh = self
            .mesh()
            .ok_or_else(|| anyhow::anyhow!("mesh not bootstrapped"))?;
        self.local_mesh_actor_refs
            .get_or_init(|| async {
                let node_name = self
                    .local_mesh_node_name
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                crate::agent::remote::spawn_and_register_local_mesh_actors_with_name(
                    self, &mesh, node_name,
                )
                .await
            })
            .await;
        Ok(())
    }

    #[cfg(feature = "remote")]
    pub async fn publish_mesh_scope(
        &self,
        runtime: &crate::agent::remote::MeshRuntimeHandle,
        scope: &crate::agent::remote::scope::MeshScopeId,
    ) -> anyhow::Result<()> {
        self.ensure_mesh_published(None).await?;

        let should_publish = self
            .published_mesh_scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(scope.clone());
        if should_publish && let Some(refs) = self.local_mesh_actor_refs.get() {
            crate::agent::remote::register_local_mesh_actor_scope(
                runtime.as_mesh_handle(),
                refs,
                scope,
            )
            .await;
        }

        let local_sessions = {
            let registry = self.registry.lock().await;
            registry
                .session_ids()
                .into_iter()
                .filter_map(|session_id| {
                    registry
                        .local_actor_ref(&session_id)
                        .cloned()
                        .map(|actor| (session_id, actor))
                })
                .collect::<Vec<_>>()
        };
        for (session_id, actor_ref) in local_sessions {
            let dht_name = crate::agent::remote::scope::scoped_session(scope, &session_id);
            runtime.register_actor(actor_ref, dht_name).await;
        }
        Ok(())
    }

    #[cfg(feature = "remote")]
    pub async fn join_mesh_invite(
        &self,
        invite: crate::agent::remote::invite::SignedInviteGrant,
    ) -> anyhow::Result<crate::api::MeshJoinOutcome> {
        self.ensure_mesh_published(None).await?;
        let mesh = self
            .mesh()
            .ok_or_else(|| anyhow::anyhow!("mesh not bootstrapped"))?;
        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let inviter_peer_id = invite.grant.inviter_peer_id.clone();
        let mesh_name = invite.grant.mesh_name.clone();
        let mesh_id = crate::agent::remote::invite::mesh_id_for(
            &invite.grant.inviter_peer_id,
            invite.grant.mesh_name.as_deref(),
        );
        let already_joined = runtime
            .joined_iroh_scopes()
            .into_iter()
            .any(|scope| matches!(scope, crate::agent::remote::MeshScopeId::Iroh { mesh_id: ref existing } if existing == &mesh_id));

        if !already_joined {
            let mut mesh_for_admission = mesh.clone();
            crate::api::mesh::admit_via_invite_on_runtime(&mut mesh_for_admission, &invite).await?;

            let mut peers = Vec::new();
            if let Ok(inviter_pid) = invite.grant.inviter_peer_id.parse() {
                peers.push(inviter_pid);
            }
            if let Some(entry) = runtime
                .mesh_state_store()
                .and_then(|store| store.read().get(&mesh_id).cloned())
            {
                for peer in entry.known_peers.values() {
                    if let Ok(pid) = peer.peer_id.parse() {
                        peers.push(pid);
                    }
                }
            }
            mesh.join_iroh_scope(&mesh_id, peers);

            self.set_mesh(mesh.clone());
            self.publish_mesh_scope(
                &crate::agent::remote::MeshRuntimeHandle::from(mesh.clone()),
                &crate::agent::remote::MeshScopeId::Iroh {
                    mesh_id: mesh_id.clone(),
                },
            )
            .await?;
        }

        Ok(crate::api::MeshJoinOutcome {
            mesh_id,
            mesh_name,
            inviter_peer_id,
            already_joined,
        })
    }

    #[cfg(feature = "remote")]
    pub(super) fn remote_node_info_timeout() -> std::time::Duration {
        let default_ms = 3_000_u64;
        let timeout_ms = std::env::var("QUERYMT_REMOTE_NODE_INFO_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms);
        std::time::Duration::from_millis(timeout_ms)
    }

    #[cfg(feature = "remote")]
    pub(super) fn remote_node_lookup_parallelism() -> usize {
        std::env::var("QUERYMT_REMOTE_NODE_LOOKUP_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(8)
    }

    #[cfg(feature = "remote")]
    pub(super) fn remote_node_cache_ttl() -> std::time::Duration {
        let default_ms = 10_000_u64;
        let ttl_ms = std::env::var("QUERYMT_REMOTE_NODE_CACHE_TTL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms);
        std::time::Duration::from_millis(ttl_ms)
    }

    #[cfg(feature = "remote")]
    pub(super) fn should_skip_stale_dht_record(
        scope: &crate::agent::remote::scope::MeshScopeId,
        is_peer_alive: bool,
    ) -> bool {
        !is_peer_alive && scope.is_iroh()
    }

    #[cfg(feature = "remote")]
    pub(super) fn stale_lan_probe_ttl() -> std::time::Duration {
        let default_ms = 1_500_u64;
        let ttl_ms = std::env::var("QUERYMT_REMOTE_NODE_STALE_LAN_TTL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms);
        std::time::Duration::from_millis(ttl_ms)
    }

    #[cfg(feature = "remote")]
    pub(super) fn mark_cached_remote_node_unreachable(
        &self,
        cache_key: String,
        ttl: std::time::Duration,
    ) {
        self.remote_node_cache.by_label.write().insert(
            cache_key,
            CachedNodeEntry::Unreachable {
                expires_at: std::time::Instant::now() + ttl,
            },
        );
    }

    #[cfg(feature = "remote")]
    pub(super) fn peer_cache_key(
        peer_id: Option<libp2p::PeerId>,
        fallback_actor_id: u64,
    ) -> String {
        if let Some(pid) = peer_id {
            format!("peer:{pid}")
        } else {
            format!("actor:{fallback_actor_id}")
        }
    }

    #[cfg(feature = "remote")]
    pub(super) fn get_cached_remote_node(
        &self,
        cache_key: &str,
    ) -> Option<crate::agent::remote::NodeInfo> {
        let now = std::time::Instant::now();
        if let Some(entry) = self
            .remote_node_cache
            .by_label
            .read()
            .get(cache_key)
            .cloned()
        {
            match entry {
                CachedNodeEntry::Ready { info, expires_at } if expires_at > now => {
                    return Some(info);
                }
                CachedNodeEntry::Unreachable { .. } => return None,
                _ => {}
            }
        }

        self.prune_expired_remote_node_cache_entry(cache_key);
        None
    }

    #[cfg(feature = "remote")]
    pub(super) fn is_remote_node_temporarily_unreachable(&self, cache_key: &str) -> bool {
        let now = std::time::Instant::now();
        if let Some(entry) = self
            .remote_node_cache
            .by_label
            .read()
            .get(cache_key)
            .cloned()
        {
            match entry {
                CachedNodeEntry::Unreachable { expires_at } if expires_at > now => return true,
                CachedNodeEntry::Unreachable { .. } | CachedNodeEntry::Ready { .. } => {}
            }
        }

        self.prune_expired_remote_node_cache_entry(cache_key);
        false
    }

    #[cfg(feature = "remote")]
    fn prune_expired_remote_node_cache_entry(&self, cache_key: &str) {
        let now = std::time::Instant::now();
        let mut guard = self.remote_node_cache.by_label.write();
        match guard.get(cache_key) {
            Some(CachedNodeEntry::Ready { expires_at, .. }) if *expires_at <= now => {
                guard.remove(cache_key);
            }
            Some(CachedNodeEntry::Unreachable { expires_at }) if *expires_at <= now => {
                guard.remove(cache_key);
            }
            _ => {}
        }
    }

    #[cfg(feature = "remote")]
    pub(super) fn insert_cached_remote_node(
        &self,
        cache_key: String,
        info: crate::agent::remote::NodeInfo,
    ) {
        let ttl = Self::remote_node_cache_ttl();
        self.remote_node_cache.by_label.write().insert(
            cache_key,
            CachedNodeEntry::Ready {
                info,
                expires_at: std::time::Instant::now() + ttl,
            },
        );
    }

    #[cfg(feature = "remote")]
    pub(super) fn ensure_remote_node_cache_invalidation_task(
        &self,
        mesh: &crate::agent::remote::MeshHandle,
    ) {
        if self
            .remote_node_cache
            .invalidation_task_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let mut rx = mesh.subscribe_peer_events();
        let cache = Arc::clone(&self.remote_node_cache);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(crate::agent::remote::mesh::PeerEvent::Discovered(peer_id))
                    | Ok(crate::agent::remote::mesh::PeerEvent::Expired(peer_id)) => {
                        let key = format!("peer:{peer_id}");
                        cache.by_label.write().remove(&key);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        cache
                            .invalidation_task_started
                            .store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }
}
