//! Shared remote mesh runtime bootstrap and compatibility checks.

use anyhow::{Result, anyhow};
use std::sync::OnceLock;
use tokio::sync::OnceCell;

use super::runtime_config::{MeshRuntimeConfig, from_toml_config};
use crate::agent::remote::{MeshRuntimeHandle, bootstrap_mesh_runtime};
use crate::config::MeshTomlConfig;

use super::types::{MeshRuntime, MeshSpec};

#[derive(Clone, Debug)]
struct SharedMeshState {
    runtime: MeshRuntimeHandle,
    config: MeshRuntimeConfig,
}

static SHARED_MESH: OnceLock<OnceCell<SharedMeshState>> = OnceLock::new();

pub(crate) async fn start_shared_runtime(spec: MeshSpec) -> Result<MeshRuntime> {
    // The public API exposes a singleton shared remote runtime, so repeat starts must
    // either reuse an equivalent config or fail with a clear mismatch error.
    if matches!(spec, MeshSpec::Disabled) {
        return Err(anyhow!(
            "cannot start a shared mesh runtime from Mesh::disabled()"
        ));
    }

    let requested_cfg = runtime_config_from_spec(spec.clone())?;
    let state = SHARED_MESH.get_or_init(OnceCell::new);
    let existing = state
        .get_or_try_init(|| {
            let requested_cfg = requested_cfg.clone();
            async move {
                let runtime = bootstrap_mesh_runtime(&requested_cfg).await?;
                Ok::<SharedMeshState, anyhow::Error>(SharedMeshState {
                    runtime,
                    config: requested_cfg,
                })
            }
        })
        .await?;

    if !runtime_configs_compatible(&existing.config, &requested_cfg) {
        return Err(anyhow!(
            "shared mesh runtime already started with {}; requested {}",
            runtime_config_label(&existing.config),
            runtime_config_label(&requested_cfg)
        ));
    }

    Ok(MeshRuntime::from_handle(existing.runtime.clone()))
}

fn runtime_configs_compatible(existing: &MeshRuntimeConfig, requested: &MeshRuntimeConfig) -> bool {
    existing.enabled == requested.enabled
        && existing
            .lan
            .as_ref()
            .map(|lan| (&lan.listen, lan.discovery, lan.directory))
            == requested
                .lan
                .as_ref()
                .map(|lan| (&lan.listen, lan.discovery, lan.directory))
        && existing.iroh_enabled == requested.iroh_enabled
        && existing.identity_file == requested.identity_file
        && existing.request_timeout == requested.request_timeout
        && existing.stream_reconnect_grace == requested.stream_reconnect_grace
        && existing.node_name == requested.node_name
        && existing.peers == requested.peers
        && existing.auto_fallback == requested.auto_fallback
        && existing
            .iroh_scopes
            .iter()
            .map(|scope| (&scope.mesh_id, &scope.invite, &scope.name))
            .eq(requested
                .iroh_scopes
                .iter()
                .map(|scope| (&scope.mesh_id, &scope.invite, &scope.name)))
}

fn runtime_config_label(config: &MeshRuntimeConfig) -> String {
    let transports = match (config.lan.is_some(), config.iroh_enabled) {
        (true, true) => "hybrid",
        (true, false) => "lan",
        (false, true) => "iroh",
        (false, false) => "disabled",
    };
    format!(
        "{transports} mesh (identity={:?}, node_name={:?}, peers={}, iroh_scopes={})",
        config.identity_file,
        config.node_name,
        config.peers.len(),
        config.iroh_scopes.len()
    )
}

fn runtime_config_from_spec(spec: MeshSpec) -> Result<MeshRuntimeConfig> {
    let cfg: MeshTomlConfig = spec.into_toml();
    from_toml_config(
        cfg.enabled,
        cfg.transport,
        cfg.discovery,
        cfg.listen,
        cfg.peers.into_iter().map(|p| p.addr).collect(),
        cfg.request_timeout_secs,
        cfg.stream_reconnect_grace_secs,
        cfg.identity_file,
        cfg.invite,
        cfg.node_name,
        cfg.auto_fallback,
        cfg.lan,
        cfg.iroh,
    )
}
