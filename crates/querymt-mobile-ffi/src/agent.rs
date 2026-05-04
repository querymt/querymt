//! Agent initialization and shutdown.

use crate::ffi_helpers::set_last_error;
use crate::mcp;
use crate::runtime::global_runtime;
use crate::state;
use crate::types::{FfiErrorCode, MobileInitConfig};
use querymt::plugin::host::PluginRegistry;
use std::ffi::CStr;
use std::sync::Arc;

pub fn init_agent_inner(
    config_json: *const std::ffi::c_char,
    out_agent: *mut u64,
) -> Result<(), FfiErrorCode> {
    if config_json.is_null() || out_agent.is_null() {
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
    let plugin_registry = Arc::new(PluginRegistry::empty());
    register_static_providers(&plugin_registry);
    let storage = create_storage_backend(&config).await?;

    let infra = querymt_agent::api::AgentInfra {
        plugin_registry: plugin_registry.clone(),
        storage: Some(storage.clone()),
    };

    let (mesh_handle, mesh_auto_fallback) = if config.mesh.enabled {
        match bootstrap_mesh_from_config(&config).await {
            Ok((mesh, fb)) => (Some(mesh), fb),
            Err(e) => {
                set_last_error(
                    FfiErrorCode::RuntimeError,
                    format!("Mesh bootstrap failed: {:#}", e),
                );
                return Err(FfiErrorCode::RuntimeError);
            }
        }
    } else {
        (None, false)
    };

    let agent_config = build_single_agent_config(&config)?;

    #[cfg(feature = "remote")]
    let agent = querymt_agent::api::Agent::from_single_config_with_registry_and_infra(
        agent_config,
        None,
        mesh_handle,
        mesh_auto_fallback,
        infra,
    )
    .await
    .map_err(|e| {
        set_last_error(
            FfiErrorCode::RuntimeError,
            format!("Agent construction failed: {:#}", e),
        );
        FfiErrorCode::RuntimeError
    })?;

    #[cfg(not(feature = "remote"))]
    let agent = querymt_agent::api::Agent::from_config(agent_config, infra)
        .await
        .map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Agent construction failed: {:#}", e),
            );
            FfiErrorCode::RuntimeError
        })?;

    let handle = state::insert_agent(agent, storage, plugin_registry.clone());

    #[cfg(feature = "remote")]
    if config.mesh.enabled {
        let inner = state::with_agent_read(handle, |record| Ok(record.agent.inner().clone()))?;
        if let Some(mesh) = inner.mesh() {
            let refs = querymt_agent::agent::remote::spawn_and_register_local_mesh_actors(
                inner.as_ref(),
                &mesh,
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
            auto_fallback: config.mesh.auto_fallback,
            identity_file: config.mesh.identity_file.clone(),
            invite: config.mesh.invite.clone(),
            ..Default::default()
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

#[cfg(feature = "remote")]
async fn bootstrap_mesh_from_config(
    config: &MobileInitConfig,
) -> Result<(querymt_agent::agent::remote::MeshHandle, bool), anyhow::Error> {
    use querymt_agent::agent::remote::{
        MeshConfig, MeshDiscovery, MeshTransportMode, bootstrap_mesh,
    };

    let transport = match config.mesh.transport.as_str() {
        "iroh" => MeshTransportMode::Iroh,
        _ => MeshTransportMode::Lan,
    };

    let mesh_config = MeshConfig {
        listen: Some("/ip4/0.0.0.0/tcp/9000".to_string()),
        discovery: MeshDiscovery::Mdns,
        bootstrap_peers: vec![],
        directory: Default::default(),
        request_timeout: std::time::Duration::from_secs(300),
        stream_reconnect_grace: std::time::Duration::from_secs(30),
        transport,
        identity_file: config
            .mesh
            .identity_file
            .clone()
            .map(std::path::PathBuf::from),
        invite: None,
    };

    let mesh_handle = bootstrap_mesh(&mesh_config).await?;
    Ok((mesh_handle, config.mesh.auto_fallback))
}

#[cfg(not(feature = "remote"))]
async fn bootstrap_mesh_from_config(
    _config: &MobileInitConfig,
) -> Result<(std::convert::Infallible, bool), anyhow::Error> {
    Err(anyhow::anyhow!(
        "Mesh not available (feature=remote not enabled)"
    ))
}
