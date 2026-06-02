#[cfg(feature = "remote")]
use super::utils::ext_json_response;
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_remote_sessions(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RemoteSessionsReq {
            node_id: String,
            #[serde(default)]
            offset: Option<u32>,
            #[serde(default)]
            limit: Option<u32>,
        }

        let parsed: RemoteSessionsReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            let nm_ref = self.find_node_manager(&parsed.node_id).await?;
            let response = self
                .list_remote_sessions(&nm_ref, parsed.offset, parsed.limit)
                .await?;
            ext_json_response(&serde_json::json!({
                "nodeId": parsed.node_id,
                "sessions": response.sessions,
                "nextOffset": response.next_offset,
                "totalCount": response.total_count,
            }))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_remote_create_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CreateReq {
            node_id: String,
            #[serde(default)]
            cwd: Option<String>,
        }

        let parsed: CreateReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            let nm_ref = self.find_node_manager(&parsed.node_id).await?;
            let resp = self
                .create_remote_session(&nm_ref, parsed.cwd.clone())
                .await?;

            ext_json_response(&serde_json::json!({
                "sessionId": resp.session_id,
                "nodeId": parsed.node_id,
                "attached": false,
                "configOptions": [],
            }))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_remote_attach_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AttachReq {
            node_id: String,
            session_id: String,
        }

        let parsed: AttachReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            let mesh = self
                .mesh()
                .ok_or_else(|| Error::invalid_request().data("mesh not bootstrapped"))?;

            let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
            let mut remote_ref = None;
            let mut matched_scope = None;
            let mut lookup_err = None;
            for scope in runtime.active_scopes() {
                let dht_name =
                    crate::agent::remote::scope::scoped_session(&scope, &parsed.session_id);
                match runtime
                    .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name)
                    .await
                {
                    Ok(Some(found)) => {
                        remote_ref = Some(found);
                        matched_scope = Some(scope);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => lookup_err = Some(e),
                }
            }
            let remote_ref = match remote_ref {
                Some(r) => r,
                None => {
                    if let Some(err) = lookup_err {
                        log::debug!(
                            "remote attach scoped lookup error before resume fallback: {}",
                            err
                        );
                    }
                    let nm_ref = self.find_node_manager(&parsed.node_id).await?;
                    let resumed = self
                        .resume_remote_session(&nm_ref, parsed.session_id.clone())
                        .await?;
                    self.resolve_handoff(&parsed.session_id, resumed.handoff)
                        .await?
                }
            };

            let peer_label = self
                .list_remote_nodes()
                .await
                .into_iter()
                .find(|n| n.node_id.to_string() == parsed.node_id)
                .map(|n| n.hostname)
                .unwrap_or_else(|| parsed.node_id.clone());

            self.attach_remote_session(
                parsed.session_id.clone(),
                remote_ref,
                peer_label,
                matched_scope,
                Some(parsed.node_id.clone()),
            )
            .await;

            let snapshot = self
                .build_remote_attach_snapshot(&parsed.session_id)
                .await
                .unwrap_or(serde_json::Value::Null);

            ext_json_response(&serde_json::json!({
                "sessionId": parsed.session_id,
                "nodeId": parsed.node_id,
                "attached": true,
                "configOptions": [],
                "snapshot": snapshot,
            }))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_remote_dismiss_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct DismissReq {
            session_id: String,
        }

        let parsed: DismissReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            {
                let mut registry = self.registry.lock().await;
                registry.detach_remote_session(&parsed.session_id).await;
            }

            self.config
                .provider
                .history_store()
                .remove_remote_session_bookmark(&parsed.session_id)
                .await
                .map_err(|e| {
                    Error::internal_error().data(serde_json::json!({"error": e.to_string()}))
                })?;

            ext_json_response(&serde_json::json!({ "success": true }))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }
}
