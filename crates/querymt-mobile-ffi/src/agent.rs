//! Agent initialization and shutdown.

use crate::ffi_helpers::set_last_error;
use crate::mcp;
use crate::runtime::global_runtime;
use crate::state;
use crate::types::FfiErrorCode;
use querymt::plugin::host::PluginRegistry;
use querymt_agent::config::{Config, ConfigSource};
use std::ffi::CStr;
use std::sync::Arc;

/// Parse inline TOML config using the shared `querymt_agent::config::load_config`
/// parser. Returns a `Config` (Single or Multi) or an error code.
pub fn parse_config(config_toml: *const std::ffi::c_char) -> Result<Config, FfiErrorCode> {
    if config_toml.is_null() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "Null pointer argument".into(),
        );
        return Err(FfiErrorCode::InvalidArgument);
    }

    let config_str = unsafe {
        CStr::from_ptr(config_toml).to_str().map_err(|_| {
            set_last_error(
                FfiErrorCode::InvalidArgument,
                "config_toml is not valid UTF-8".into(),
            );
            FfiErrorCode::InvalidArgument
        })?
    };

    let rt = global_runtime();
    rt.block_on(async {
        querymt_agent::config::load_config(ConfigSource::Toml(config_str.to_owned()))
            .await
            .map_err(|e| {
                set_last_error(
                    FfiErrorCode::InvalidArgument,
                    format!("Failed to parse config: {:#}", e),
                );
                FfiErrorCode::InvalidArgument
            })
    })
}

/// Create the agent from an already-parsed config. The caller should have
/// already called `events::setup_mobile_telemetry()`.
pub fn init_agent_from_config(config: Config, out_agent: *mut u64) -> Result<(), FfiErrorCode> {
    if out_agent.is_null() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "Null pointer argument".into(),
        );
        return Err(FfiErrorCode::InvalidArgument);
    }

    let runtime = global_runtime();
    let result = runtime.block_on(async { init_agent_async(config).await });

    match result {
        Ok(agent_handle) => {
            unsafe {
                *out_agent = agent_handle;
            }
            Ok(())
        }
        Err(code) => Err(code),
    }
}

async fn init_agent_async(config: Config) -> Result<u64, FfiErrorCode> {
    // Bootstrap providers metadata cache (models.dev.json) so that
    // get_model_info() can resolve context limits, pricing, etc.
    // Best-effort: if the device is offline on first launch the cache
    // simply stays empty and model info falls back to defaults.
    if let Err(e) = querymt::providers::update_providers_if_stale().await {
        log::warn!("Failed to bootstrap providers metadata cache: {e}");
    }

    let plugin_registry = Arc::new(PluginRegistry::empty());
    register_static_providers(&plugin_registry);

    // Extract mesh config for diagnostics before consuming config.
    let mesh_config = match &config {
        Config::Single(single) => single.mesh.clone(),
        Config::Multi(quorum) => quorum.mesh.clone(),
    };
    let db_path = match &config {
        Config::Single(single) => single.agent.db.clone(),
        Config::Multi(_) => None,
    };

    let storage = create_storage_backend(db_path).await?;

    let infra = querymt_agent::api::AgentInfra {
        plugin_registry: plugin_registry.clone(),
        storage: Some(storage.clone()),
    };

    let agent = match config {
        Config::Single(single) => {
            querymt_agent::api::Agent::from_single_config_with_infra(single, infra)
                .await
                .map_err(|e| {
                    set_last_error(
                        FfiErrorCode::RuntimeError,
                        format!("Agent construction failed: {:#}", e),
                    );
                    FfiErrorCode::RuntimeError
                })?
        }
        Config::Multi(quorum) => {
            log::info!("Initializing multi-agent/quorum config");
            querymt_agent::api::Agent::from_quorum_config_with_infra(quorum, infra)
                .await
                .map_err(|e| {
                    set_last_error(
                        FfiErrorCode::RuntimeError,
                        format!("Quorum construction failed: {:#}", e),
                    );
                    FfiErrorCode::RuntimeError
                })?
        }
    };

    let handle = state::insert_agent(agent, storage, plugin_registry.clone());

    // Store mesh config diagnostics so mesh_status can report them.
    #[cfg(feature = "remote")]
    if mesh_config.enabled {
        let listen_str = mesh_config.listen.clone();
        let discovery_str = match mesh_config.discovery {
            querymt_agent::config::MeshDiscoveryConfig::Mdns => "mdns",
            querymt_agent::config::MeshDiscoveryConfig::Kademlia => "kademlia",
            querymt_agent::config::MeshDiscoveryConfig::None => "none",
        };
        let node_name = mesh_config.node_name.clone();
        state::with_agent(handle, |record| {
            record.mesh_listen = listen_str;
            record.mesh_discovery = Some(discovery_str.to_string());
            Ok(())
        })?;

        let inner = state::with_agent_read(handle, |record| Ok(record.agent.inner().clone()))?;
        if let Some(mesh) = inner.mesh() {
            let refs =
                querymt_agent::agent::remote::spawn_and_register_local_mesh_actors_with_name(
                    inner.as_ref(),
                    &mesh,
                    node_name,
                )
                .await;
            state::set_local_mesh_actors(handle, refs)?;
        }
    }

    Ok(handle)
}

pub fn shutdown_agent_inner(agent_handle: u64) -> Result<(), FfiErrorCode> {
    let mut record = state::remove_agent(agent_handle)?;
    if record.call_tracker.has_active_calls(agent_handle) {
        let _ = state::insert_agent(record.agent, record.storage, record.plugin_registry);
        set_last_error(FfiErrorCode::Busy, "Agent has active FFI calls".into());
        return Err(FfiErrorCode::Busy);
    }
    mcp::unregister_all_mcp_for_agent(agent_handle);
    record.sessions.clear();
    drop(record);
    Ok(())
}

fn register_static_providers(registry: &PluginRegistry) {
    #[cfg(feature = "provider-anthropic")]
    {
        let factory = qmt_anthropic::create_http_factory();
        registry.register_static_http(factory);
        log::info!("Registered static provider: anthropic");
    }
    #[cfg(feature = "provider-openai")]
    {
        let factory = qmt_openai::create_http_factory();
        registry.register_static_http(factory);
        log::info!("Registered static provider: openai");
    }
}

async fn create_storage_backend(
    db_path: Option<std::path::PathBuf>,
) -> Result<Arc<dyn querymt_agent::session::backend::StorageBackend>, FfiErrorCode> {
    let db_path = match db_path {
        Some(path) => path,
        None => querymt_agent::session::backend::default_agent_db_path().map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Failed to resolve default DB path: {:#}", e),
            );
            FfiErrorCode::RuntimeError
        })?,
    };

    let storage = querymt_agent::session::sqlite_storage::SqliteStorage::connect(db_path)
        .await
        .map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Failed to open SQLite storage: {:#}", e),
            );
            FfiErrorCode::RuntimeError
        })?;

    Ok(Arc::new(storage))
}
