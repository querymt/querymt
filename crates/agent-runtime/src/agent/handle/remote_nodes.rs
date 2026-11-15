use super::*;

impl LocalAgentHandle {
    #[cfg(feature = "remote")]
    pub async fn list_remote_nodes(&self) -> Vec<crate::agent::remote::NodeInfo> {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use futures_util::{StreamExt, stream::FuturesUnordered};
        use querymt_remote::ask_remote_with_timeout;

        let Some(mesh) = self.mesh() else {
            log::debug!("list_remote_nodes: mesh not bootstrapped");
            return Vec::new();
        };

        self.ensure_remote_node_cache_invalidation_task(&mesh);

        let local_peer_id = *mesh.peer_id();
        let timeout = Self::remote_node_info_timeout();
        let concurrency = Self::remote_node_lookup_parallelism();
        let semaphore = Arc::new(Semaphore::new(concurrency));

        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let mut lookups: RemoteNodeLookupQueue = FuturesUnordered::new();
        let mut cached_nodes = Vec::new();
        let mut scheduled_cache_keys = std::collections::HashSet::new();

        let scopes = runtime.active_scopes();
        let alive_peers: Vec<_> = mesh.known_peer_ids();
        log::debug!(
            "list_remote_nodes: querying {} scope(s), {} known peer(s), local_peer_id={}",
            scopes.len(),
            alive_peers.len(),
            local_peer_id,
        );

        for scope in &scopes {
            for peer_id in &alive_peers {
                if *peer_id == local_peer_id {
                    continue;
                }
                let peer_id = *peer_id;
                let dht_name =
                    crate::agent::remote::scope::scoped_node_manager_for_peer(scope, &peer_id);
                log::debug!(
                    "list_remote_nodes: querying per-peer DHT name '{}'",
                    dht_name
                );
                match runtime
                    .lookup_actor_no_retry::<RemoteNodeManager>(dht_name.clone())
                    .await
                {
                    Ok(Some(node_manager_ref)) => {
                        log::debug!(
                            "list_remote_nodes: per-peer DHT hit for peer {} under '{}'",
                            peer_id,
                            dht_name
                        );
                        let cache_key = Self::peer_cache_key(
                            Some(peer_id),
                            node_manager_ref.id().sequence_id(),
                        );
                        if !scheduled_cache_keys.insert(cache_key.clone()) {
                            log::debug!(
                                "list_remote_nodes: duplicate discovery for peer {:?} under '{}'",
                                Some(peer_id),
                                dht_name
                            );
                            continue;
                        }

                        if let Some(info) = self.get_cached_remote_node(&cache_key) {
                            log::debug!(
                                "list_remote_nodes: cache hit for peer {:?} under '{}'",
                                Some(peer_id),
                                dht_name
                            );
                            cached_nodes.push(info);
                            continue;
                        }

                        log::debug!(
                            "list_remote_nodes: enqueuing GetNodeInfo for peer {:?} under '{}'",
                            Some(peer_id),
                            dht_name
                        );
                        let semaphore = Arc::clone(&semaphore);
                        lookups.push(Box::pin(async move {
                            let permit = semaphore.acquire_owned().await.ok();
                            let res =
                                ask_remote_with_timeout(&node_manager_ref, &GetNodeInfo, timeout)
                                    .await;
                            drop(permit);
                            (cache_key, Some(peer_id), res)
                        }));
                    }
                    Ok(None) => {
                        log::debug!(
                            "list_remote_nodes: per-peer DHT miss for peer {} under '{}'",
                            peer_id,
                            dht_name
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "list_remote_nodes: per-peer lookup error for '{}': {}",
                            dht_name,
                            e
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
                match result {
                    Ok(node_manager_ref) => {
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
                                let cache_key = Self::peer_cache_key(
                                    peer_id,
                                    node_manager_ref.id().sequence_id(),
                                );
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

                        let cache_key =
                            Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
                        if !scheduled_cache_keys.insert(cache_key.clone()) {
                            log::debug!(
                                "list_remote_nodes: duplicate discovery for peer {:?} under '{}'",
                                peer_id,
                                dht_name
                            );
                            continue;
                        }

                        if let Some(info) = self.get_cached_remote_node(&cache_key) {
                            log::debug!(
                                "list_remote_nodes: cache hit for peer {:?} under '{}'",
                                peer_id,
                                dht_name
                            );
                            cached_nodes.push(info);
                            continue;
                        }

                        log::debug!(
                            "list_remote_nodes: enqueuing GetNodeInfo for peer {:?} under '{}'",
                            peer_id,
                            dht_name
                        );
                        let semaphore = Arc::clone(&semaphore);
                        lookups.push(Box::pin(async move {
                            let permit = semaphore.acquire_owned().await.ok();
                            let res =
                                ask_remote_with_timeout(&node_manager_ref, &GetNodeInfo, timeout)
                                    .await;
                            drop(permit);
                            (cache_key, peer_id, res)
                        }));
                    }
                    Err(e) => {
                        log::warn!("list_remote_nodes: lookup error for '{}': {}", dht_name, e)
                    }
                }
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
        while let Some((cache_key, peer_id, result)) = lookups.next().await {
            match result {
                Ok(info) => {
                    self.insert_cached_remote_node(cache_key, info.clone());
                    fetched_nodes.push(info);
                }
                Err(e) => {
                    if let Some(pid) = peer_id {
                        self.mark_cached_remote_node_unreachable(
                            cache_key.clone(),
                            Self::stale_lan_probe_ttl(),
                        );
                        self.mark_cached_remote_node_unreachable(
                            Self::peer_cache_key(Some(pid), 0),
                            Self::stale_lan_probe_ttl(),
                        );
                    }
                    match e {
                        kameo::error::RemoteSendError::ReplyTimeout => log::warn!(
                            "list_remote_nodes: GetNodeInfo timed out for peer {:?}",
                            peer_id
                        ),
                        other => log::warn!("list_remote_nodes: GetNodeInfo failed: {}", other),
                    }
                }
            }
        }

        cached_nodes.extend(fetched_nodes);
        cached_nodes
    }
}
