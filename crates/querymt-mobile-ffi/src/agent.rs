//! Agent initialization and shutdown.

use crate::ffi_helpers::set_last_error;
use crate::mcp;
use crate::runtime::global_runtime;
use crate::state;
use crate::types::{FfiErrorCode, MobileInitConfig};
use querymt::plugin::host::PluginRegistry;
use std::ffi::CStr;
use std::sync::Arc;

/// Parse and validate the config JSON. Returns the parsed config or an error code.
pub fn parse_config(
    config_json: *const std::ffi::c_char,
) -> Result<MobileInitConfig, FfiErrorCode> {
    if config_json.is_null() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "Null pointer argument".into(),
        );
        return Err(FfiErrorCode::InvalidArgument);
    }

    let config_str = unsafe {
        CStr::from_ptr(config_json).to_str().map_err(|_| {
            set_last_error(
                FfiErrorCode::InvalidArgument,
                "config_json is not valid UTF-8".into(),
            );
            FfiErrorCode::InvalidArgument
        })?
    };

    let config: MobileInitConfig = serde_json::from_str(config_str).map_err(|e| {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            format!("Failed to parse config JSON: {:#}", e),
        );
        FfiErrorCode::InvalidArgument
    })?;

    if config.agent.provider.is_empty() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "Missing required field: agent.provider".into(),
        );
        return Err(FfiErrorCode::InvalidArgument);
    }
    if config.agent.model.is_empty() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "Missing required field: agent.model".into(),
        );
        return Err(FfiErrorCode::InvalidArgument);
    }

    Ok(config)
}

/// Create the agent from an already-parsed config. The caller should have
/// already called `events::setup_mobile_telemetry(&config.telemetry)`.
pub fn init_agent_from_config(
    config: MobileInitConfig,
    out_agent: *mut u64,
) -> Result<(), FfiErrorCode> {
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

async fn init_agent_async(config: MobileInitConfig) -> Result<u64, FfiErrorCode> {
    // Bootstrap providers metadata cache (models.dev.json) so that
    // get_model_info() can resolve context limits, pricing, etc.
    // Best-effort: if the device is offline on first launch the cache
    // simply stays empty and model info falls back to defaults.
    if let Err(e) = querymt::providers::update_providers_if_stale().await {
        log::warn!("Failed to bootstrap providers metadata cache: {e}");
    }

    let plugin_registry = Arc::new(PluginRegistry::empty());
    register_static_providers(&plugin_registry);
    let storage = create_storage_backend(&config).await?;

    let infra = querymt_agent::api::AgentInfra {
        plugin_registry: plugin_registry.clone(),
        storage: Some(storage.clone()),
    };

    let agent_config = build_single_agent_config(&config)?;

    let agent = querymt_agent::api::Agent::from_single_config_with_infra(agent_config, infra)
        .await
        .map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Agent construction failed: {:#}", e),
            );
            FfiErrorCode::RuntimeError
        })?;

    let handle = state::insert_agent(agent, storage, plugin_registry.clone());

    // Store mesh config diagnostics so mesh_status can report them.
    #[cfg(feature = "remote")]
    if config.mesh.enabled {
        let listen_str = config.mesh.listen.clone();
        let discovery_str = config.mesh.discovery.clone();
        state::with_agent(handle, |record| {
            record.mesh_listen = listen_str;
            record.mesh_discovery = Some(discovery_str);
            Ok(())
        })?;
    }

    #[cfg(feature = "remote")]
    if config.mesh.enabled {
        let inner = state::with_agent_read(handle, |record| Ok(record.agent.inner().clone()))?;
        if let Some(mesh) = inner.mesh() {
            let refs =
                querymt_agent::agent::remote::spawn_and_register_local_mesh_actors_with_name(
                    inner.as_ref(),
                    &mesh,
                    config.mesh.node_name.clone(),
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
    config: &MobileInitConfig,
) -> Result<Arc<dyn querymt_agent::session::backend::StorageBackend>, FfiErrorCode> {
    let db_path = match &config.agent.db {
        Some(path) => std::path::PathBuf::from(path),
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

fn build_single_agent_config(
    config: &MobileInitConfig,
) -> Result<querymt_agent::config::SingleAgentConfig, FfiErrorCode> {
    let agent_part = querymt_agent::config::AgentSettings {
        provider: config.agent.provider.clone(),
        model: config.agent.model.clone(),
        cwd: config.agent.cwd.clone().map(std::path::PathBuf::from),
        db: config.agent.db.clone().map(std::path::PathBuf::from),
        tools: config.agent.tools.clone(),
        system: config
            .agent
            .system
            .iter()
            .map(|s| querymt_agent::config::SystemPart::Inline(s.clone()))
            .collect(),
        api_key: config.agent.api_key.clone(),
        parameters: config
            .agent
            .parameters
            .clone()
            .map(|m| m.into_iter().collect()),
        execution: querymt_agent::config::ExecutionPolicy::default(),
        skills: querymt_agent::config::SkillsConfig::default(),
        assume_mutating: true,
        mutating_tools: vec![],
    };

    Ok(querymt_agent::config::SingleAgentConfig {
        agent: agent_part,
        mcp: vec![],
        middleware: vec![],
        mesh: querymt_agent::config::MeshTomlConfig {
            enabled: config.mesh.enabled,
            transport: match config.mesh.transport.as_str() {
                "iroh" => querymt_agent::config::MeshTransportConfig::Iroh,
                _ => querymt_agent::config::MeshTransportConfig::Lan,
            },
            listen: config.mesh.listen.clone(),
            discovery: match config.mesh.discovery.as_str() {
                "none" => querymt_agent::config::MeshDiscoveryConfig::None,
                "kademlia" => querymt_agent::config::MeshDiscoveryConfig::Kademlia,
                _ => querymt_agent::config::MeshDiscoveryConfig::Mdns,
            },
            auto_fallback: config.mesh.auto_fallback,
            peers: config
                .mesh
                .peers
                .iter()
                .map(|p| querymt_agent::config::MeshPeerConfig {
                    name: p.name.clone(),
                    addr: p.addr.clone(),
                })
                .collect(),
            request_timeout_secs: config.mesh.request_timeout_secs,
            stream_reconnect_grace_secs: config.mesh.stream_reconnect_grace_secs,
            identity_file: config.mesh.identity_file.clone(),
            invite: config.mesh.invite.clone(),
            node_name: config.mesh.node_name.clone(),
        },
        remote_agents: config
            .remote_agents
            .iter()
            .map(|r| querymt_agent::config::RemoteAgentConfig {
                id: r.id.clone(),
                name: r.name.clone(),
                description: r.description.clone(),
                peer: r.peer.clone(),
                capabilities: r.capabilities.clone(),
            })
            .collect(),
    })
}
