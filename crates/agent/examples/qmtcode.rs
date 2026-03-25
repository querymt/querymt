//! QMT Code Agent Example
//!
//! Multi-mode agent that can run as ACP stdio server, headless UI server, web dashboard, or mesh node.
//!
//! ## Usage
//!
//! ```bash
//! # ACP stdio mode
//! cargo run --example qmtcode -- --acp
//!
//! # Headless UI mode (for alternate UIs like qmtui)
//! cargo run --example qmtcode --features headless-ui -- --headless-ui
//! cargo run --example qmtcode --features headless-ui -- --headless-ui=0.0.0.0:8080
//!
//! # Web dashboard mode
//! cargo run --example qmtcode --features dashboard -- --dashboard
//! cargo run --example qmtcode --features dashboard -- --dashboard=0.0.0.0:8080
//!
//! # Mesh-only mode (runs until Ctrl+C)
//! cargo run --example qmtcode --features remote -- --mesh
//! cargo run --example qmtcode --features remote -- --mesh=/ip4/0.0.0.0/tcp/9000
//!
//! # Dashboard mode with kameo mesh enabled (cross-machine sessions)
//! cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh
//! cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh=/ip4/0.0.0.0/tcp/9000
//!
//! # Running a built binary with embedded default config
//! ./qmtcode --mesh
//! ```

#[cfg(any(feature = "api-only", feature = "dashboard"))]
use clap::ArgGroup;
use clap::Parser;
use querymt_agent::prelude::*;
#[cfg(feature = "api-only")]
use querymt_agent::server::ServerMode;
use rust_embed::RustEmbed;
use std::path::{Component, Path, PathBuf};

#[cfg(any(feature = "api-only", feature = "dashboard"))]
const DEFAULT_DASHBOARD_ADDR: &str = "127.0.0.1:3000";
#[cfg(feature = "remote")]
const DEFAULT_MESH_ADDR: &str = "/ip4/0.0.0.0/tcp/9000";
const EMBEDDED_CONFIG: &str = include_str!("confs/single_coder.toml");

#[derive(RustEmbed)]
#[folder = "examples/prompts/"]
struct EmbeddedPromptAssets;

#[derive(Debug, Parser)]
#[command(name = "qmtcode")]
#[command(version = env!("QMT_BUILD_VERSION"))]
#[command(
    about = "Run QueryMT coder agent in ACP mode, API-only mode, dashboard mode, or as a mesh node"
)]
#[command(
    after_help = "Examples:\n  qmtcode --acp\n  qmtcode --api-only\n  qmtcode --api-only=0.0.0.0:8080\n  qmtcode --dashboard\n  qmtcode --dashboard=0.0.0.0:8080\n  qmtcode --mesh\n  qmtcode --mesh=/ip4/0.0.0.0/tcp/9001\n  qmtcode --api-only --mesh\n  qmtcode path/to/config.toml --acp"
)]
#[cfg_attr(
    all(feature = "api-only", feature = "dashboard"),
    command(group(ArgGroup::new("transport").args(["acp", "api_only", "dashboard"]).multiple(false)))
)]
#[cfg_attr(
    all(feature = "api-only", not(feature = "dashboard")),
    command(group(ArgGroup::new("transport").args(["acp", "api_only"]).multiple(false)))
)]
#[cfg_attr(
    all(not(feature = "api-only"), feature = "dashboard"),
    command(group(ArgGroup::new("transport").args(["acp", "dashboard"]).multiple(false)))
)]
struct Cli {
    /// Path to TOML config.
    ///
    /// If omitted, uses an embedded copy of `examples/confs/single_coder.toml`.
    config_file: Option<PathBuf>,

    /// Run as ACP stdio server (for subprocess spawning)
    #[arg(long)]
    acp: bool,

    /// Run API-only server for alternate UIs; optionally set bind address
    #[cfg(feature = "api-only")]
    #[cfg_attr(feature = "dashboard", arg(long = "api-only", value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_DASHBOARD_ADDR, conflicts_with = "dashboard"))]
    #[cfg_attr(not(feature = "dashboard"), arg(long = "api-only", value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_DASHBOARD_ADDR))]
    api_only: Option<String>,

    /// Run web dashboard; optionally set bind address
    #[cfg(feature = "dashboard")]
    #[cfg_attr(feature = "api-only", arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_DASHBOARD_ADDR, conflicts_with = "api_only"))]
    #[cfg_attr(not(feature = "api-only"), arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_DASHBOARD_ADDR))]
    dashboard: Option<String>,

    /// Enable kameo mesh networking for cross-machine sessions.
    ///
    /// Starts a libp2p swarm with mDNS peer discovery and registers this node
    /// as a `RemoteNodeManager` so remote peers can create sessions here.
    ///
    /// Optionally specify the multiaddr to listen on (default: /ip4/0.0.0.0/tcp/9000).
    ///
    /// Examples:
    ///   --mesh                          → listen on /ip4/0.0.0.0/tcp/9000
    ///   --mesh=/ip4/0.0.0.0/tcp/9001   → listen on port 9001
    ///   --mesh=/ip4/0.0.0.0/tcp/0      → OS-assigned random port
    ///
    /// Requires the `remote` cargo feature.
    #[cfg(feature = "remote")]
    #[arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_MESH_ADDR)]
    mesh: Option<String>,
}

fn embedded_single_coder_config() -> anyhow::Result<String> {
    use anyhow::{Context, anyhow};

    let mut value: toml::Value =
        toml::from_str(EMBEDDED_CONFIG).context("Failed to parse embedded single_coder.toml")?;

    let system = value
        .get_mut("agent")
        .and_then(toml::Value::as_table_mut)
        .and_then(|agent| agent.get_mut("system"))
        .context("Embedded single_coder.toml missing [agent].system")?;

    match system {
        toml::Value::String(_) => {}
        toml::Value::Array(parts) => {
            for part in parts {
                if let toml::Value::Table(table) = part
                    && let Some(file_ref) = table.get("file").and_then(toml::Value::as_str)
                {
                    let asset_key = embedded_prompt_asset_key(file_ref).ok_or_else(|| {
                        anyhow!(
                            "Unsupported embedded prompt path '{file_ref}' in single_coder.toml"
                        )
                    })?;

                    let embedded = EmbeddedPromptAssets::get(&asset_key).ok_or_else(|| {
                        anyhow!("Embedded prompt '{file_ref}' not found under examples/prompts")
                    })?;

                    let prompt =
                        String::from_utf8(embedded.data.into_owned()).with_context(|| {
                            format!("Embedded prompt '{file_ref}' is not valid UTF-8")
                        })?;

                    *part = toml::Value::String(prompt);
                }
            }
        }
        _ => {
            return Err(anyhow!(
                "Embedded single_coder.toml has unsupported [agent].system format"
            ));
        }
    }

    toml::to_string(&value).context("Failed to serialize embedded single_coder.toml")
}

fn embedded_prompt_asset_key(file_ref: &str) -> Option<String> {
    let joined = Path::new("confs").join(file_ref);
    let mut normalized_parts: Vec<String> = Vec::new();

    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized_parts.pop()?;
            }
            Component::Normal(part) => {
                normalized_parts.push(part.to_string_lossy().into_owned());
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    let normalized = normalized_parts.join("/");
    normalized.strip_prefix("prompts/").map(str::to_owned)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let is_acp = cli.acp;
    #[cfg(feature = "api-only")]
    let is_api_only = cli.api_only.is_some();
    #[cfg(not(feature = "api-only"))]
    let is_api_only = false;
    #[cfg(feature = "dashboard")]
    let is_dashboard = cli.dashboard.is_some();
    #[cfg(not(feature = "dashboard"))]
    let is_dashboard = false;
    #[cfg(feature = "remote")]
    let has_mesh = cli.mesh.is_some();
    #[cfg(not(feature = "remote"))]
    let has_mesh = false;

    if !is_acp && !is_api_only && !is_dashboard && !has_mesh {
        return Err("No mode selected. Use --acp, --api-only, --dashboard, or --mesh.".into());
    }

    // Setup telemetry: ACP mode writes console logs to stderr (stdout is
    // reserved for JSON-RPC); dashboard/mesh modes use stdout.
    // OTLP export (traces + logs over gRPC) is active in all modes.
    querymt_utils::telemetry::setup_telemetry("qmtcode", env!("QMT_BUILD_VERSION"), is_acp);

    let runner = if let Some(config_path) = &cli.config_file {
        eprintln!("Loading agent from: {}", config_path.display());
        from_config(config_path).await?
    } else {
        eprintln!("Loading agent from embedded default config: single_coder.toml");
        let embedded_config = embedded_single_coder_config()?;
        from_config(ConfigSource::Toml(embedded_config)).await?
    };

    eprintln!("Agent loaded successfully!\n");

    // ── Phase 6: Mesh Bootstrap ───────────────────────────────────────────────
    //
    // If --mesh was passed, start the kameo libp2p swarm and register this node
    // as a RemoteNodeManager so remote peers can create sessions here.
    #[cfg(feature = "remote")]
    if let Some(ref mesh_addr) = cli.mesh {
        use kameo::actor::Spawn;
        use querymt_agent::agent::remote::RemoteNodeManager;
        use querymt_agent::agent::remote::mesh::{MeshConfig, MeshDiscovery, bootstrap_mesh};

        let mesh_config = MeshConfig {
            listen: Some(mesh_addr.clone()),
            discovery: MeshDiscovery::Mdns,
            bootstrap_peers: vec![],
            directory: querymt_agent::agent::remote::mesh::DirectoryMode::default(),
            request_timeout: std::time::Duration::from_secs(300),
        };

        match bootstrap_mesh(&mesh_config).await {
            Ok(mesh) => {
                eprintln!("Kameo mesh bootstrapped: peer_id={}", mesh.peer_id());
                eprintln!("Mesh listening on: {}", mesh_addr);

                let agent_handle = runner.handle();

                // Spawn RemoteNodeManager, giving it the MeshHandle so it can
                // register newly created sessions in the DHT without global state.
                let node_manager = RemoteNodeManager::new(
                    agent_handle.config.clone(),
                    agent_handle.registry.clone(),
                    Some(mesh.clone()),
                );
                let node_manager_ref = RemoteNodeManager::spawn(node_manager);

                // Register under the global name so lookup_all_actors (used by
                // list_remote_nodes) can discover this node alongside all others.
                mesh.register_actor(
                    node_manager_ref.clone(),
                    querymt_agent::agent::remote::dht_name::NODE_MANAGER,
                )
                .await;
                eprintln!(
                    "RemoteNodeManager registered in kameo DHT as '{}'",
                    querymt_agent::agent::remote::dht_name::NODE_MANAGER
                );

                // Also register under the per-peer name so find_node_manager
                // can do a direct O(1) DHT lookup by peer_id, bypassing the
                // is_peer_alive gate that guards the lookup_all_actors scan.
                // This makes create_remote_session robust against mDNS TTL
                // expiry (30 s) on cross-machine setups.
                let per_peer_name =
                    querymt_agent::agent::remote::dht_name::node_manager_for_peer(mesh.peer_id());
                mesh.register_actor(node_manager_ref, per_peer_name.clone())
                    .await;
                eprintln!(
                    "RemoteNodeManager also registered in kameo DHT as '{}'",
                    per_peer_name
                );

                // Spawn ProviderHostActor so that remote peers can proxy LLM
                // calls through this node's provider registry when the session's
                // provider_node points here.
                {
                    use querymt_agent::agent::remote::ProviderHostActor;
                    use querymt_agent::agent::remote::dht_name;
                    let provider_host = ProviderHostActor::new(agent_handle.config.clone());
                    let provider_host_ref = ProviderHostActor::spawn(provider_host);
                    let dht_name = dht_name::provider_host(mesh.peer_id());
                    mesh.register_actor(provider_host_ref, dht_name.clone())
                        .await;
                    eprintln!("ProviderHostActor registered in kameo DHT as '{dht_name}'");
                }

                // Store the MeshHandle on AgentHandle so list_remote_nodes,
                // create_remote_session, and attach_remote_session can use it.
                agent_handle.set_mesh(mesh);
            }
            Err(e) => {
                eprintln!("Warning: mesh bootstrap failed: {}", e);
                eprintln!("Continuing without mesh networking...");
            }
        }
    }

    if is_acp {
        eprintln!("Starting ACP stdio server...");
        runner.acp("stdio").await?;
    } else if is_api_only {
        #[cfg(feature = "api-only")]
        {
            let addr = cli.api_only.as_deref().unwrap_or(DEFAULT_DASHBOARD_ADDR);
            eprintln!("Starting API-only server at http://{}", addr);
            runner.server().run(addr, ServerMode::ApiOnly).await?;
        }
        #[cfg(not(feature = "api-only"))]
        {
            return Err("--api-only requires the `api-only` feature.".into());
        }
    } else if is_dashboard {
        #[cfg(feature = "dashboard")]
        {
            let addr = cli.dashboard.as_deref().unwrap_or(DEFAULT_DASHBOARD_ADDR);
            eprintln!("Starting dashboard at http://{}", addr);
            runner.server().run(addr, ServerMode::Dashboard).await?;
        }
        #[cfg(not(feature = "dashboard"))]
        {
            return Err("--dashboard requires the `dashboard` feature.".into());
        }
    } else {
        eprintln!("Mesh node running. Press Ctrl+C to stop.");
        tokio::signal::ctrl_c().await?;
        eprintln!("Received Ctrl+C, shutting down mesh node...");
    }

    // Graceful shutdown: release scheduler lease, stop background tasks.
    // Idempotent — safe to call even if the dashboard server already ran shutdown.
    runner.handle().shutdown().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_config_inlines_system_prompts_exactly() {
        let config = embedded_single_coder_config().expect("embedded config should load");
        let value: toml::Value =
            toml::from_str(&config).expect("embedded config should parse as TOML");

        let system = value
            .get("agent")
            .and_then(toml::Value::as_table)
            .and_then(|agent| agent.get("system"))
            .and_then(toml::Value::as_array)
            .expect("embedded config should contain [agent].system array");

        let inlined: Vec<&str> = system
            .iter()
            .map(|part| part.as_str().expect("system part must be an inline string"))
            .collect();

        assert_eq!(
            inlined,
            vec![
                include_str!("prompts/default_system.txt"),
                include_str!("prompts/code_meta.jinja2"),
            ]
        );
    }

    #[test]
    fn embedded_prompt_asset_key_rejects_path_escape() {
        assert_eq!(
            embedded_prompt_asset_key("../prompts/default_system.txt").as_deref(),
            Some("default_system.txt")
        );
        assert_eq!(
            embedded_prompt_asset_key("../prompts/code_meta.jinja2").as_deref(),
            Some("code_meta.jinja2")
        );
        assert!(embedded_prompt_asset_key("../../outside.txt").is_none());
    }
}
