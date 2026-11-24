use super::*;

/// Bounded, concurrent `GetNodeInfo` probes of discovered node managers.
#[cfg(feature = "remote")]
pub(super) struct NodeInfoProbes {
    mesh: crate::agent::remote::MeshHandle,
    semaphore: Arc<Semaphore>,
    timeout: std::time::Duration,
    pending: futures_util::stream::FuturesUnordered<
        futures_util::future::BoxFuture<'static, NodeInfoProbe>,
    >,
}

/// One answered probe.
#[cfg(feature = "remote")]
pub(super) struct NodeInfoProbe {
    pub(super) node_manager: kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
    pub(super) cache_key: String,
    pub(super) peer_id: Option<libp2p::PeerId>,
    /// Node info whose identity matches the peer that answered. `Ok(None)`
    /// when the peer answered with a different identity: it was reached, so
    /// it is not marked unreachable, but its reply is discarded.
    pub(super) result: Result<Option<crate::agent::remote::NodeInfo>, crate::error::AgentError>,
}

#[cfg(feature = "remote")]
impl NodeInfoProbes {
    pub(super) fn new(mesh: crate::agent::remote::MeshHandle) -> Self {
        Self {
            mesh,
            semaphore: Arc::new(Semaphore::new(
                LocalAgentHandle::remote_node_lookup_parallelism(),
            )),
            timeout: LocalAgentHandle::remote_node_info_timeout(),
            pending: futures_util::stream::FuturesUnordered::new(),
        }
    }

    pub(super) fn push(
        &mut self,
        node_manager: kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        cache_key: String,
    ) {
        use crate::agent::remote::remote_call::{CallTimeouts, ask_remote};
        use crate::error::AgentError;

        let mesh = self.mesh.clone();
        let semaphore = Arc::clone(&self.semaphore);
        let timeouts = CallTimeouts::uniform(self.timeout);
        self.pending.push(Box::pin(async move {
            let peer_id = node_manager.id().peer_id().copied();
            let permit = semaphore.acquire_owned().await.ok();
            let result = ask_remote::<_, _, AgentError>(
                Some(&mesh),
                &node_manager,
                &crate::agent::remote::GetNodeInfo,
                timeouts,
            )
            .await
            .map(|info| (peer_id == Some(*info.node_id.as_peer_id())).then_some(info));
            drop(permit);
            match &result {
                Err(error) => log::warn!("GetNodeInfo failed for peer {peer_id:?}: {error}"),
                Ok(None) => {
                    log::warn!(
                        "GetNodeInfo for peer {peer_id:?} returned a different peer identity"
                    )
                }
                Ok(Some(_)) => {}
            }
            NodeInfoProbe {
                node_manager,
                cache_key,
                peer_id,
                result,
            }
        }));
    }

    pub(super) async fn next(&mut self) -> Option<NodeInfoProbe> {
        use futures_util::StreamExt;
        self.pending.next().await
    }
}

impl LocalAgentHandle {
    #[cfg(feature = "remote")]
    pub async fn list_remote_nodes(&self) -> Vec<crate::agent::remote::NodeInfo> {
        use crate::agent::remote::RemoteNodeManager;
        use futures_util::StreamExt;

        let Some(mesh) = self.mesh() else {
            log::debug!("list_remote_nodes: mesh not bootstrapped");
            return Vec::new();
        };

        self.ensure_remote_node_cache_invalidation_task(&mesh);

        let local_peer_id = *mesh.peer_id();
        // Protected counts and capabilities can change without a topology event.
        let use_cache = mesh.inbound_router().is_none();

        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let mut probes = NodeInfoProbes::new(mesh.clone());
        let mut cached_nodes = Vec::new();
        let mut scheduled_cache_keys = std::collections::HashSet::new();
        // Probe each discovered node manager once, unless its info is cached.
        let mut schedule = |node_manager_ref: kameo::actor::RemoteActorRef<RemoteNodeManager>,
                            source: &str| {
            let peer_id = node_manager_ref.id().peer_id().copied();
            let cache_key = Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
            if !scheduled_cache_keys.insert(cache_key.clone()) {
                log::debug!(
                    "list_remote_nodes: duplicate discovery for peer {peer_id:?} under '{source}'"
                );
            } else if let Some(info) = use_cache
                .then(|| self.get_cached_remote_node(&cache_key))
                .flatten()
            {
                log::debug!("list_remote_nodes: cache hit for peer {peer_id:?} under '{source}'");
                cached_nodes.push(info);
            } else {
                log::debug!(
                    "list_remote_nodes: enqueuing GetNodeInfo for peer {peer_id:?} under '{source}'"
                );
                probes.push(node_manager_ref, cache_key);
            }
        };

        let scopes = runtime.active_scopes();
        let alive_peers: Vec<_> = mesh.known_peer_ids();
        log::debug!(
            "list_remote_nodes: querying {} scope(s), {} known peer(s), local_peer_id={}",
            scopes.len(),
            alive_peers.len(),
            local_peer_id,
        );

        for scope in &scopes {
            for peer_id in alive_peers.iter().filter(|peer| **peer != local_peer_id) {
                let dht_name =
                    crate::agent::remote::scope::scoped_node_manager_for_peer(scope, peer_id);
                match runtime
                    .lookup_actor_no_retry::<RemoteNodeManager>(dht_name.clone())
                    .await
                {
                    Ok(Some(node_manager_ref))
                        if node_manager_ref.id().peer_id() == Some(peer_id) =>
                    {
                        schedule(node_manager_ref, &dht_name);
                    }
                    Ok(_) => {
                        log::debug!(
                            "list_remote_nodes: per-peer DHT miss for peer {peer_id} under '{dht_name}'"
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "list_remote_nodes: per-peer lookup error for '{dht_name}': {e}"
                        );
                    }
                }
            }
        }

        for scope in &scopes {
            let dht_name = crate::agent::remote::scope::scoped_node_manager(scope);
            log::debug!("list_remote_nodes: querying DHT name '{}'", dht_name);
            let mut stream = runtime.lookup_all_actors::<RemoteNodeManager>(dht_name.clone());
            let mut found_count = 0usize;

            while let Some(result) = stream.next().await {
                let node_manager_ref = match result {
                    Ok(node_manager_ref) => node_manager_ref,
                    Err(e) => {
                        log::warn!("list_remote_nodes: lookup error for '{}': {}", dht_name, e);
                        continue;
                    }
                };
                found_count += 1;
                let peer_id = node_manager_ref.id().peer_id().copied();
                if peer_id == Some(local_peer_id) {
                    log::debug!("list_remote_nodes: skipping local node");
                    continue;
                }

                let stale_lan_ttl = Self::stale_lan_probe_ttl();
                if let Some(pid) = peer_id {
                    let is_peer_alive = mesh.is_peer_alive(&pid);
                    if Self::should_skip_stale_dht_record(scope, is_peer_alive) {
                        let key = format!("peer:{pid}");
                        self.remote_node_cache.by_label.write().remove(&key);
                        log::warn!(
                            "list_remote_nodes: skipping stale DHT record for peer {pid} \
                         (is_peer_alive=false, scope=iroh, dht_name='{}')",
                            dht_name
                        );
                        continue;
                    }

                    if !is_peer_alive && scope.is_lan() {
                        let cache_key =
                            Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
                        if self.is_remote_node_temporarily_unreachable(&cache_key) {
                            log::debug!(
                                "list_remote_nodes: skipping stale LAN DHT record for peer {pid} due to active negative cache ttl={}ms (dht_name='{}')",
                                stale_lan_ttl.as_millis(),
                                dht_name
                            );
                            continue;
                        }
                        log::debug!(
                            "list_remote_nodes: probing stale LAN DHT record for peer {pid} with negative cache ttl={}ms (dht_name='{}')",
                            stale_lan_ttl.as_millis(),
                            dht_name
                        );
                    }
                }

                schedule(node_manager_ref, &dht_name);
            }

            log::debug!(
                "list_remote_nodes: DHT name '{}' yielded {} actor(s)",
                dht_name,
                found_count
            );
        }

        if scopes.is_empty() {
            log::warn!("list_remote_nodes: active_scopes() returned empty — no DHT queries issued");
        }

        let mut fetched_nodes = Vec::new();
        while let Some(probe) = probes.next().await {
            match probe.result {
                Ok(None) => {}
                Ok(Some(info)) => {
                    if use_cache {
                        self.insert_cached_remote_node(probe.cache_key, info.clone());
                    }
                    fetched_nodes.push(info);
                }
                Err(_) => {
                    if let Some(pid) = probe.peer_id {
                        self.mark_cached_remote_node_unreachable(
                            probe.cache_key,
                            Self::stale_lan_probe_ttl(),
                        );
                        self.mark_cached_remote_node_unreachable(
                            Self::peer_cache_key(Some(pid), 0),
                            Self::stale_lan_probe_ttl(),
                        );
                    }
                }
            }
        }

        cached_nodes.extend(fetched_nodes);
        cached_nodes
    }
}
