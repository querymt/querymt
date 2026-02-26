//! Coder Agent Example
//!
//! Multi-mode agent that can run as ACP stdio server, web dashboard, or mesh node.
//!
//! ## Usage
//!
//! ```bash
//! # ACP stdio mode
//! cargo run --example coder_agent -- --acp
//!
//! # Web dashboard mode
//! cargo run --example coder_agent --features dashboard -- --dashboard
//! cargo run --example coder_agent --features dashboard -- --dashboard=0.0.0.0:8080
//!
//! # Mesh-only mode (runs until Ctrl+C)
//! cargo run --example coder_agent --features remote -- --mesh
//! cargo run --example coder_agent --features remote -- --mesh=/ip4/0.0.0.0/tcp/9000
//!
//! # Dashboard mode with kameo mesh enabled (cross-machine sessions)
//! cargo run --example coder_agent --features "dashboard remote" -- --dashboard --mesh
//! cargo run --example coder_agent --features "dashboard remote" -- --dashboard --mesh=/ip4/0.0.0.0/tcp/9000
//!
//! # Running a built binary with embedded default config
//! ./coder_agent --mesh
//! ```

#[cfg(feature = "dashboard")]
use clap::ArgGroup;
use clap::Parser;
use querymt_agent::prelude::*;
use std::path::PathBuf;

#[cfg(feature = "dashboard")]
const DEFAULT_DASHBOARD_ADDR: &str = "127.0.0.1:3000";
#[cfg(feature = "remote")]
const DEFAULT_MESH_ADDR: &str = "/ip4/0.0.0.0/tcp/9000";
const EMBEDDED_CONFIG: &str = include_str!("confs/single_coder.toml");
const EMBEDDED_PROMPT: &str = include_str!("prompts/default_system.txt");
const EMBEDDED_PROMPT_REF: &str = r#"{ file = "../prompts/default_system.txt" }"#;

#[derive(Debug, Parser)]
#[command(name = "coder_agent")]
#[command(about = "Run QueryMT coder agent in ACP mode, dashboard mode, or as a mesh node")]
#[command(
    after_help = "Examples:\n  coder_agent --acp\n  coder_agent --dashboard\n  coder_agent --dashboard=0.0.0.0:8080\n  coder_agent --mesh\n  coder_agent --mesh=/ip4/0.0.0.0/tcp/9001\n  coder_agent --dashboard --mesh\n  coder_agent path/to/config.toml --acp"
)]
#[cfg_attr(
    feature = "dashboard",
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

    /// Run web dashboard; optionally set bind address
    #[cfg(feature = "dashboard")]
    #[arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_DASHBOARD_ADDR)]
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

/// Setup logging for stdio mode - writes to stderr only to avoid corrupting stdout JSON-RPC.
fn setup_stdio_logging() {
    use tracing_log::LogTracer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, Registry, fmt};

    // Initialize log->tracing bridge so log:: macros from providers work.
    LogTracer::init().expect("Failed to set LogTracer");

    // Create fmt layer that writes to STDERR only (stdout is reserved for JSON-RPC).
    let fmt_layer = fmt::layer().with_writer(std::io::stderr).with_target(true);

    let filter = EnvFilter::from_default_env();

    let subscriber = Registry::default().with(filter).with(fmt_layer);

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
}

fn embedded_single_coder_config() -> String {
    let inline_prompt = format!("'''{}'''", EMBEDDED_PROMPT);
    EMBEDDED_CONFIG.replace(EMBEDDED_PROMPT_REF, &inline_prompt)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let is_acp = cli.acp;
    #[cfg(feature = "dashboard")]
    let is_dashboard = cli.dashboard.is_some();
    #[cfg(not(feature = "dashboard"))]
    let is_dashboard = false;
    #[cfg(feature = "remote")]
    let has_mesh = cli.mesh.is_some();
    #[cfg(not(feature = "remote"))]
    let has_mesh = false;

    if !is_acp && !is_dashboard && !has_mesh {
        return Err("No mode selected. Use --acp, --dashboard, or --mesh.".into());
    }

    // Setup logging based on mode:
    // - ACP mode: logs to stderr (stdout reserved for JSON-RPC)
    // - Dashboard/mesh modes: full telemetry with stdout
    if is_acp {
        setup_stdio_logging();
    } else {
        querymt_utils::telemetry::setup_telemetry("querymt-coder-agent", env!("CARGO_PKG_VERSION"));
    }

    let runner = if let Some(config_path) = &cli.config_file {
        eprintln!("Loading agent from: {}", config_path.display());
        from_config(config_path).await?
    } else {
        eprintln!("Loading agent from embedded default config: single_coder.toml");
        let embedded_config = embedded_single_coder_config();
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
    } else if is_dashboard {
        #[cfg(feature = "dashboard")]
        {
            let addr = cli.dashboard.as_deref().unwrap_or(DEFAULT_DASHBOARD_ADDR);
            eprintln!("Starting dashboard at http://{}", addr);
            runner.dashboard().run(addr).await?;
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

    Ok(())
}
