use anyhow::Result;
use qmt_openai::create_http_factory;
use querymt_remote::{
    LanDiscovery, LanMeshConfig, MeshRuntimeConfig, ModelAllowlistBackend, ProviderShare,
    RegistryProviderBackend, StaticCatalogBackend, bootstrap_mesh_runtime,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let label = std::env::var("QMT_NODE_LABEL").ok();
    let allowed_models = std::env::var("QMT_ALLOWED_MODELS")
        .unwrap_or_else(|_| "gpt-4o-mini".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    let runtime = bootstrap_mesh_runtime(&MeshRuntimeConfig {
        enabled: true,
        lan: Some(LanMeshConfig {
            listen: Some("/ip4/0.0.0.0/tcp/0".to_string()),
            discovery: LanDiscovery::Mdns,
            directory: querymt_remote::mesh_runtime_config::DirectoryMode::Cached,
        }),
        iroh_enabled: false,
        iroh_scopes: Vec::new(),
        identity_file: None,
        request_timeout: std::time::Duration::from_secs(300),
        stream_reconnect_grace: std::time::Duration::from_secs(120),
        node_name: label.clone(),
        peers: Vec::new(),
        auto_fallback: false,
    })
    .await?;

    let registry = Arc::new(querymt::PluginRegistry::empty());
    registry.register_static_http(create_http_factory());
    let backend = ModelAllowlistBackend::new(RegistryProviderBackend::new(Arc::clone(&registry)))
        .allow_models("openai", allowed_models.clone());

    let catalog = StaticCatalogBackend::provider_models(
        runtime.peer_id().to_string(),
        label,
        "openai",
        allowed_models.clone(),
    );

    let share = ProviderShare::new(Arc::new(backend), Arc::new(catalog));
    share.register_on_mesh(&runtime).await;

    println!(
        "sharing provider=openai allowed_models={} peer_id={}",
        allowed_models.join(","),
        runtime.peer_id()
    );
    println!("set OPENAI_API_KEY before connecting clients");

    futures_util::future::pending::<()>().await;
    Ok(())
}
