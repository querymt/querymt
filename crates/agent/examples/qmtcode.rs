//! QMT Code Agent Example
//!
//! Multi-mode agent that can run as ACP stdio server, API server, web dashboard, or mesh node.
//!
//! ## Usage
//!
//! ```bash
//! # ACP stdio mode
//! cargo run --example qmtcode -- --acp
//!
//! # API server mode (for alternate UIs like qmtui)
//! cargo run --example qmtcode --features api -- --api
//! cargo run --example qmtcode --features api -- --api=0.0.0.0:8080
//!
//! # Web dashboard mode
//! cargo run --example qmtcode --features dashboard -- --dashboard
//! cargo run --example qmtcode --features dashboard -- --dashboard=0.0.0.0:8080
//!
//! # LAN mesh-only mode (runs until Ctrl+C)
//! cargo run --example qmtcode --features remote -- --mesh
//! cargo run --example qmtcode --features remote -- --mesh=/ip4/0.0.0.0/tcp/9000
//!
//! # Dashboard mode with kameo mesh enabled (cross-machine sessions)
//! cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh
//! cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh=/ip4/0.0.0.0/tcp/9000
//!
//! # Internet mesh: host a mesh and generate an invite token
//! cargo run --example qmtcode --features "remote-internet" -- --mesh --mesh-invite
//! cargo run --example qmtcode --features "remote-internet" -- --mesh --mesh-invite="My Dev Mesh"
//!
//! # Internet mesh: join via invite token
//! cargo run --example qmtcode --features "remote-internet" -- --mesh-join=qmt://mesh/join/TOKEN
//!
//! # Running a built binary with embedded default config
//! ./qmtcode --mesh
//! ```

#[cfg(feature = "api")]
use clap::ArgGroup;
use clap::Parser;
use querymt_agent::prelude::*;
#[cfg(feature = "api")]
use querymt_agent::server::ServerMode;
use rust_embed::RustEmbed;
use std::path::{Component, Path, PathBuf};

#[cfg(feature = "api")]
const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:3000";
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
    about = "Run QueryMT coder agent in ACP mode, API mode, dashboard mode, or as a mesh node"
)]
#[command(
    after_help = "Examples:\n  qmtcode --acp\n  qmtcode --api\n  qmtcode --api=0.0.0.0:8080\n  qmtcode --dashboard\n  qmtcode --dashboard=0.0.0.0:8080\n  qmtcode --mesh\n  qmtcode --mesh=/ip4/0.0.0.0/tcp/9001\n  qmtcode --api --mesh\n  qmtcode --mesh --mesh-invite\n  qmtcode --mesh --mesh-invite=\"My Mesh\"\n  qmtcode --mesh-join=qmt://mesh/join/TOKEN\n  qmtcode path/to/config.toml --acp"
)]
#[cfg_attr(
    feature = "dashboard",
    command(group(ArgGroup::new("transport").args(["acp", "api", "dashboard"]).multiple(false)))
)]
#[cfg_attr(
    all(feature = "api", not(feature = "dashboard")),
    command(group(ArgGroup::new("transport").args(["acp", "api"]).multiple(false)))
)]
struct Cli {
    /// Path to TOML config.
    ///
    /// If omitted, uses an embedded copy of `examples/confs/single_coder.toml`.
    config_file: Option<PathBuf>,

    /// Run as ACP stdio server (for subprocess spawning)
    #[arg(long)]
    acp: bool,

    /// Run API server for alternate UIs; optionally set bind address
    #[cfg(feature = "api")]
    #[arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_SERVER_ADDR)]
    api: Option<String>,

    /// Run web dashboard; optionally set bind address
    #[cfg(feature = "dashboard")]
    #[arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_SERVER_ADDR)]
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

    /// Create and print a signed mesh invite token, then start as an iroh mesh host.
    ///
    /// Requires --mesh. The invite is signed with the node's ed25519 identity
    /// keypair (~/.qmt/mesh_identity.key). Optionally specify a human-readable
    /// mesh name.
    ///
    /// Examples:
    ///   --mesh --mesh-invite                    → generate invite, print, start
    ///   --mesh --mesh-invite="My Agent Mesh"    → with a name
    #[cfg(feature = "remote-internet")]
    #[arg(long, value_name = "name", num_args = 0..=1, default_missing_value = "")]
    mesh_invite: Option<String>,

    /// Time-to-live for invite tokens. Default: 24h.
    ///
    /// Examples: 1h, 7d, 30m, none (no expiry)
    #[cfg(feature = "remote-internet")]
    #[arg(long, value_name = "duration", default_value = "24h")]
    invite_ttl: Option<String>,

    /// Maximum number of uses for invite tokens. Default: 1 (single-use).
    ///
    /// Set to 0 for unlimited uses.
    #[cfg(feature = "remote-internet")]
    #[arg(long, value_name = "n", default_value = "1")]
    invite_uses: Option<u32>,

    /// Join an existing mesh using an invite token.
    ///
    /// Starts the node with iroh transport, dials the inviter from the token,
    /// and joins the mesh. Implies --mesh (no need to specify separately).
    ///
    /// Examples:
    ///   --mesh-join=qmt://mesh/join/eyJpbnZ...
    ///   --mesh-join=eyJpbnZ...
    #[cfg(feature = "remote-internet")]
    #[arg(long, value_name = "token")]
    mesh_join: Option<String>,
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

/// Register the standard mesh actors (RemoteNodeManager, ProviderHostActor)
/// on a bootstrapped mesh.
#[cfg(feature = "remote")]
async fn register_mesh_actors(
    runner: &querymt_agent::prelude::AgentRunner,
    mesh: &querymt_agent::agent::remote::MeshHandle,
) {
    use kameo::actor::Spawn;
    use querymt_agent::agent::remote::{ProviderHostActor, RemoteNodeManager, dht_name};

    let agent_handle = runner.handle();

    // Spawn RemoteNodeManager so remote peers can create sessions here.
    let node_manager = RemoteNodeManager::new(
        agent_handle.config.clone(),
        agent_handle.registry.clone(),
        Some(mesh.clone()),
    );
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    // Register under the global name.
    mesh.register_actor(node_manager_ref.clone(), dht_name::NODE_MANAGER)
        .await;
    eprintln!(
        "RemoteNodeManager registered in kameo DHT as '{}'",
        dht_name::NODE_MANAGER
    );

    // Also register under the per-peer name for direct O(1) lookup.
    let per_peer_name = dht_name::node_manager_for_peer(mesh.peer_id());
    mesh.register_actor(node_manager_ref, per_peer_name.clone())
        .await;
    eprintln!(
        "RemoteNodeManager also registered in kameo DHT as '{}'",
        per_peer_name
    );

    // Spawn ProviderHostActor so remote peers can proxy LLM calls.
    let provider_host = ProviderHostActor::new(agent_handle.config.clone());
    let provider_host_ref = ProviderHostActor::spawn(provider_host);
    let provider_dht_name = dht_name::provider_host(mesh.peer_id());
    mesh.register_actor(provider_host_ref, provider_dht_name.clone())
        .await;
    eprintln!("ProviderHostActor registered in kameo DHT as '{provider_dht_name}'");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let is_acp = cli.acp;
    #[cfg(feature = "api")]
    let is_api = cli.api.is_some();
    #[cfg(not(feature = "api"))]
    let is_api = false;
    #[cfg(feature = "dashboard")]
    let is_dashboard = cli.dashboard.is_some();
    #[cfg(not(feature = "dashboard"))]
    let is_dashboard = false;
    #[cfg(feature = "remote-internet")]
    let has_mesh_join = cli.mesh_join.is_some();
    #[cfg(not(feature = "remote-internet"))]
    let has_mesh_join = false;

    // --mesh-invite implies --mesh (iroh host mode).
    #[cfg(feature = "remote-internet")]
    let has_mesh_invite = cli.mesh_invite.is_some();
    #[cfg(not(feature = "remote-internet"))]
    let has_mesh_invite = false;

    #[cfg(feature = "remote")]
    let has_mesh = cli.mesh.is_some() || has_mesh_join || has_mesh_invite;
    #[cfg(not(feature = "remote"))]
    let has_mesh = has_mesh_join || has_mesh_invite;

    if !is_acp && !is_api && !is_dashboard && !has_mesh {
        return Err(
            "No mode selected. Use --acp, --api, --dashboard, or --mesh, or --mesh-join.".into(),
        );
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
    // Three mesh modes:
    //   1. --mesh (LAN): TCP + QUIC + mDNS, same-subnet discovery
    //   2. --mesh --mesh-invite (iroh host): start iroh mesh, print invite token
    //   3. --mesh-join=TOKEN (iroh join): join existing mesh via invite token
    //
    // Modes 2 and 3 require the `remote-internet` feature.

    // ── Mode 3: Join via invite token ─────────────────────────────────────────
    #[cfg(feature = "remote-internet")]
    if let Some(ref token) = cli.mesh_join {
        use querymt_agent::agent::remote::invite::SignedInviteGrant;
        use querymt_agent::agent::remote::mesh::join_mesh_via_invite;

        let invite =
            SignedInviteGrant::decode(token).map_err(|e| format!("Invalid invite token: {e}"))?;
        invite
            .verify()
            .map_err(|e| format!("Invite verification failed: {e}"))?;

        eprintln!(
            "Joining mesh{} via inviter {}...",
            invite
                .grant
                .mesh_name
                .as_ref()
                .map(|n| format!(" \"{}\"", n))
                .unwrap_or_default(),
            invite.grant.inviter_peer_id
        );

        match join_mesh_via_invite(&invite, None).await {
            Ok(mesh) => {
                eprintln!("Joined mesh: peer_id={}", mesh.peer_id());
                register_mesh_actors(&runner, &mesh).await;
                runner.handle().set_mesh(mesh);
            }
            Err(e) => {
                eprintln!("Warning: mesh join failed: {}", e);
                eprintln!("Continuing without mesh networking...");
            }
        }
    }

    // ── Mode 2: Host with invite token ────────────────────────────────────────
    #[cfg(feature = "remote-internet")]
    let mesh_invite_handled = cli.mesh_join.is_some();
    #[cfg(not(feature = "remote-internet"))]
    let mesh_invite_handled = false;

    #[cfg(feature = "remote")]
    let effective_mesh = cli.mesh.clone().or_else(|| {
        if has_mesh_invite {
            Some(DEFAULT_MESH_ADDR.to_string())
        } else {
            None
        }
    });

    #[cfg(feature = "remote")]
    if let Some(ref mesh_addr) = effective_mesh
        && !mesh_invite_handled
    {
        use querymt_agent::agent::remote::mesh::{
            MeshConfig, MeshDiscovery, MeshTransportMode, bootstrap_mesh,
        };

        // Check if --mesh-invite was passed (iroh host mode).
        #[cfg(feature = "remote-internet")]
        let is_iroh_host = cli.mesh_invite.is_some();
        #[cfg(not(feature = "remote-internet"))]
        let is_iroh_host = false;

        let transport = if is_iroh_host {
            MeshTransportMode::Iroh
        } else {
            MeshTransportMode::Lan
        };

        let mesh_config = MeshConfig {
            listen: if is_iroh_host {
                None
            } else {
                Some(mesh_addr.clone())
            },
            discovery: if is_iroh_host {
                MeshDiscovery::None
            } else {
                MeshDiscovery::Mdns
            },
            bootstrap_peers: vec![],
            directory: querymt_agent::agent::remote::mesh::DirectoryMode::default(),
            request_timeout: std::time::Duration::from_secs(300),
            stream_first_chunk_timeout: std::time::Duration::from_secs(600),
            stream_idle_chunk_timeout: std::time::Duration::from_secs(60),
            transport,
            identity_file: None,
            invite: None,
        };

        match bootstrap_mesh(&mesh_config).await {
            Ok(mesh) => {
                eprintln!("Kameo mesh bootstrapped: peer_id={}", mesh.peer_id());
                if is_iroh_host {
                    eprintln!("Mesh transport: iroh (internet-capable)");
                } else {
                    eprintln!("Mesh listening on: {}", mesh_addr);
                }

                // If hosting with iroh, generate and print the signed invite token.
                #[cfg(feature = "remote-internet")]
                if let Some(name) = &cli.mesh_invite {
                    let mesh_name = if name.is_empty() {
                        None
                    } else {
                        Some(name.clone())
                    };

                    // Parse TTL from CLI flag.
                    let ttl_secs = cli
                        .invite_ttl
                        .as_deref()
                        .and_then(querymt_agent::agent::remote::invite::parse_duration_secs);

                    let max_uses = cli.invite_uses;

                    match mesh.create_invite(mesh_name, ttl_secs, max_uses, false) {
                        Ok(invite) => {
                            let ttl_label = match ttl_secs {
                                Some(s) => {
                                    querymt_agent::agent::remote::invite::format_duration_human(s)
                                }
                                None => "no expiry".to_string(),
                            };
                            let uses_label = match max_uses {
                                Some(0) | None if max_uses == Some(0) => "unlimited".to_string(),
                                Some(1) => "single-use".to_string(),
                                Some(n) => format!("{n} uses"),
                                None => "single-use".to_string(),
                            };

                            let url = invite.to_url();

                            eprintln!();
                            eprintln!("────────────────────────────────────────────");
                            eprintln!("Mesh invite ({uses_label}, expires in {ttl_label}):");
                            eprintln!();
                            eprintln!("  {url}");
                            eprintln!();

                            // Render QR code if the terminal supports it.
                            if let Some(qr) =
                                querymt_agent::agent::remote::qr::render_to_terminal(&url)
                            {
                                for line in qr.lines() {
                                    eprintln!("  {line}");
                                }
                                eprintln!();
                            }

                            eprintln!("────────────────────────────────────────────");
                            eprintln!();
                        }
                        Err(e) => {
                            eprintln!("Warning: failed to create invite: {e}");
                        }
                    }
                }

                register_mesh_actors(&runner, &mesh).await;
                runner.handle().set_mesh(mesh);
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
    } else if is_api {
        #[cfg(feature = "api")]
        {
            let addr = cli.api.as_deref().unwrap_or(DEFAULT_SERVER_ADDR);
            eprintln!("Starting API server at http://{}", addr);
            runner.server().run(addr, ServerMode::Api).await?;
        }
        #[cfg(not(feature = "api"))]
        {
            return Err("--api requires the `api` feature.".into());
        }
    } else if is_dashboard {
        #[cfg(feature = "dashboard")]
        {
            let addr = cli.dashboard.as_deref().unwrap_or(DEFAULT_SERVER_ADDR);
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
