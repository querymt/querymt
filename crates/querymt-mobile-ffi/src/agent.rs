//! Agent initialization and shutdown.

use crate::ffi_helpers::set_last_error;
use crate::mcp;
use crate::runtime::global_runtime;
use crate::state;
use crate::types::FfiErrorCode;
use querymt::plugin::host::PluginRegistry;
use querymt_agent::config::{Config, ConfigSource};
use querymt_agent::profiles::{
    DEFAULT_EMBEDDED_PROFILE_KEY, LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager,
    standard_embedded_profile_catalog_builder,
};
use std::ffi::CStr;
use std::path::PathBuf;
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
    if let Some(handle) = state::attach_existing_runtime_agent() {
        log::info!(
            "ffi.agent.attach: attached to existing process runtime (agent_handle={handle})"
        );
        return Ok(handle);
    }

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
    let storage = create_storage_backend(None).await?;

    use std::sync::atomic::{AtomicU64, Ordering};
    let agent_handle_cell = Arc::new(AtomicU64::new(0));

    let session_mcp_source: Arc<dyn querymt_agent::agent::session_mcp::SessionMcpAttachmentSource> =
        Arc::new(crate::MobileSessionMcpAttachmentSource {
            agent_handle_cell: agent_handle_cell.clone(),
        });

    let infra = querymt_agent::api::AgentInfra {
        plugin_registry: plugin_registry.clone(),
        storage: Some(storage.clone()),
        session_mcp_attachment_source: Some(session_mcp_source),
    };

    let profile_catalog =
        build_mobile_profile_catalog(resolve_mobile_profiles_dir()).map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Failed to build profile catalog: {:#}", e),
            );
            FfiErrorCode::RuntimeError
        })?;
    let profile_manager = Arc::new(ProfileRuntimeManager::with_infra_boxed(
        Arc::new(profile_catalog) as Arc<dyn ProfileCatalog>,
        DEFAULT_EMBEDDED_PROFILE_KEY,
        infra.clone(),
    ));

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

    let (handle, reused_runtime) = state::attach_or_insert_runtime_agent(agent, storage);

    let inner = state::with_agent_read(handle, |record| Ok(record.agent.inner().clone()))?;
    inner.set_profiles(profile_manager.clone());
    match profile_manager.list_profiles().await {
        Ok(profiles) => log::info!(
            "ffi.profiles.init: catalog ready profiles={} active={}",
            profiles.len(),
            DEFAULT_EMBEDDED_PROFILE_KEY
        ),
        Err(e) => log::warn!("ffi.profiles.init: failed to list profile catalog: {e:#}"),
    }

    if reused_runtime {
        log::info!(
            "ffi.agent.attach: attached to existing process runtime (agent_handle={handle})"
        );
    } else {
        log::info!("ffi.runtime.init: initialized process runtime (agent_handle={handle})");
    }

    // Update the MCP attachment source with the now-known agent handle.
    agent_handle_cell.store(handle, Ordering::Release);

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

        if let Some(mesh) = inner.mesh() {
            profile_manager.set_mesh(mesh.clone()).await;
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

fn resolve_mobile_profiles_dir() -> Option<PathBuf> {
    match querymt_utils::providers::config_dir() {
        Ok(config_dir) => {
            let profiles_dir = config_dir.join("profiles");
            log::info!(
                "ffi.profiles.init: using mobile profile dir {}",
                profiles_dir.display()
            );
            Some(profiles_dir)
        }
        Err(e) => {
            log::warn!(
                "ffi.profiles.init: config dir unavailable, using embedded profiles only: {e:#}"
            );
            None
        }
    }
}

fn build_mobile_profile_catalog(
    profiles_dir: Option<PathBuf>,
) -> anyhow::Result<LocalProfileCatalog> {
    let mut builder = standard_embedded_profile_catalog_builder()?;
    if let Some(dir) = profiles_dir {
        builder = builder.default_user_dir(dir);
    }
    Ok(builder.build())
}

pub fn shutdown_agent_inner(agent_handle: u64) -> Result<(), FfiErrorCode> {
    let has_active_calls = state::with_agent_read(agent_handle, |record| {
        Ok(record.call_tracker.has_active_calls(agent_handle))
    })?;
    if has_active_calls {
        set_last_error(FfiErrorCode::Busy, "Agent has active FFI calls".into());
        return Err(FfiErrorCode::Busy);
    }

    let mut record = state::remove_agent(agent_handle)?;
    mcp::unregister_all_mcp_for_agent(agent_handle);
    record.sessions.clear();
    drop(record);
    log::info!("ffi.agent.detach: detached logical agent (agent_handle={agent_handle})");
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
    #[cfg(feature = "provider-google")]
    {
        let factory = qmt_google::create_http_factory();
        registry.register_static_http(factory);
        log::info!("Registered static provider: google");
    }
    #[cfg(feature = "provider-deepseek")]
    {
        let factory = qmt_deepseek::create_http_factory();
        registry.register_static_http(factory);
        log::info!("Registered static provider: deepseek");
    }
}

async fn create_storage_backend(
    db_path: Option<std::path::PathBuf>,
) -> Result<Arc<dyn querymt_agent::session::backend::StorageBackend>, FfiErrorCode> {
    let db_path = match db_path {
        Some(path) => path,
        None => querymt_agent::session::backend::resolve_agent_db_path(None).map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Failed to resolve DB path: {:#}", e),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mobile_profile_catalog_includes_embedded_default() {
        let catalog = build_mobile_profile_catalog(None).expect("catalog should build");
        let profiles = catalog.list_profiles().await.expect("list profiles");

        assert_eq!(profiles.len(), 2);
        assert!(
            profiles
                .iter()
                .any(|profile| profile.id == DEFAULT_EMBEDDED_PROFILE_KEY),
            "mobile catalog should include the embedded default profile"
        );
        assert!(
            profiles
                .iter()
                .any(|profile| profile.id == "coder-delegate"),
            "mobile catalog should include the embedded coder delegate profile"
        );
    }

    #[tokio::test]
    async fn mobile_profile_catalog_skips_missing_config_profiles_dir() {
        let profiles_dir = PathBuf::from("/path/that/should/not/exist/querymt/profiles");
        let catalog =
            build_mobile_profile_catalog(Some(profiles_dir)).expect("catalog should build");
        let profiles = catalog.list_profiles().await.expect("list profiles");

        assert_eq!(profiles.len(), 2);
        assert!(
            profiles
                .iter()
                .any(|profile| profile.id == DEFAULT_EMBEDDED_PROFILE_KEY)
        );
        assert!(
            profiles
                .iter()
                .any(|profile| profile.id == "coder-delegate")
        );
    }
}
