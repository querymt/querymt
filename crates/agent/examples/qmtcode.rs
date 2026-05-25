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
//! cargo run --example qmtcode --features remote -- --mesh=/ip4/0.0.0.0/tcp/0
//!
//! # Dashboard mode with kameo mesh enabled (cross-machine sessions)
//! cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh
//! cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh=/ip4/0.0.0.0/tcp/0
//!
//! # Internet mesh: host a mesh and generate an invite token
//! cargo run --example qmtcode --features "remote" -- --mesh --mesh-invite
//! cargo run --example qmtcode --features "remote" -- --mesh --mesh-invite="My Dev Mesh"
//!
//! # Internet mesh: join via invite token
//! cargo run --example qmtcode --features "remote" -- --mesh-join=qmt://mesh/join/TOKEN
//!
//! # Running a built binary with embedded default config
//! ./qmtcode --mesh
//! ```

use clap::ArgAction;
#[cfg(any(feature = "api", feature = "dashboard"))]
use clap::ArgGroup;
use clap::Parser;
use querymt_agent::prelude::*;
use querymt_agent::profiles::{
    DEFAULT_EMBEDDED_PROFILE_KEY, LocalProfileCatalog, ProfileCatalog, ProfileConfigKind,
    ProfileMetadata, ProfileRuntimeManager, ProfileSource, ensure_unique_profile_ids,
};
#[cfg(any(feature = "api", feature = "dashboard"))]
use querymt_agent::server::ServerMode;
use rust_embed::RustEmbed;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "api")]
const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:3000";
#[cfg(feature = "remote")]
const DEFAULT_MESH_ADDR: &str = "/ip4/0.0.0.0/tcp/0";
const EMBEDDED_SINGLE_CODER_CONFIG: &str = include_str!("confs/single_coder.toml");
const EMBEDDED_CODER_DELEGATE_CONFIG: &str = include_str!("confs/coder_delegate.toml");

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

    /// Directory containing local TOML profiles.
    #[arg(long, value_name = "path", action = ArgAction::Append)]
    profiles_dir: Vec<PathBuf>,

    /// Profile id to load from the local profile catalog.
    #[arg(long, value_name = "id")]
    profile: Option<String>,

    /// List local profiles and exit.
    #[arg(long)]
    list_profiles: bool,

    /// Path to the shared sessions SQLite database.
    ///
    /// Overrides QMT_SESSIONS_DB and the default ~/.qmt/sessions.db runtime path.
    #[arg(long, value_name = "path")]
    db: Option<PathBuf>,

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
    /// Optionally specify the multiaddr to listen on (default: /ip4/0.0.0.0/tcp/0).
    ///
    /// Examples:
    ///   --mesh                          → listen on /ip4/0.0.0.0/tcp/0 (OS-assigned random port)
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
    #[cfg(feature = "remote")]
    #[arg(long, value_name = "name", num_args = 0..=1, default_missing_value = "")]
    mesh_invite: Option<String>,

    /// Time-to-live for invite tokens. Default: 24h.
    ///
    /// Examples: 1h, 7d, 30m, none (no expiry)
    #[cfg(feature = "remote")]
    #[arg(long, value_name = "duration", default_value = "24h")]
    invite_ttl: Option<String>,

    /// Maximum number of uses for invite tokens. Default: 1 (single-use).
    ///
    /// Set to 0 for unlimited uses.
    #[cfg(feature = "remote")]
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
    #[cfg(feature = "remote")]
    #[arg(long, value_name = "token")]
    mesh_join: Option<String>,
}

fn embedded_profile_config(config_name: &str, config: &str) -> anyhow::Result<String> {
    use anyhow::Context;

    let mut value: toml::Value = toml::from_str(config)
        .with_context(|| format!("Failed to parse embedded {config_name}"))?;
    inline_embedded_system_prompts(&mut value, config_name)?;
    toml::to_string(&value).with_context(|| format!("Failed to serialize embedded {config_name}"))
}

fn inline_embedded_system_prompts(
    value: &mut toml::Value,
    config_name: &str,
) -> anyhow::Result<()> {
    let root = value
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("Embedded {config_name} must be a TOML table"))?;

    if let Some(agent) = root.get_mut("agent").and_then(toml::Value::as_table_mut)
        && let Some(system) = agent.get_mut("system")
    {
        inline_system_prompt_value(system, config_name, "[agent].system")?;
    }

    if let Some(planner) = root.get_mut("planner").and_then(toml::Value::as_table_mut)
        && let Some(system) = planner.get_mut("system")
    {
        inline_system_prompt_value(system, config_name, "[planner].system")?;
    }

    if let Some(delegates) = root
        .get_mut("delegates")
        .and_then(toml::Value::as_array_mut)
    {
        for (index, delegate) in delegates.iter_mut().enumerate() {
            if let Some(system) = delegate
                .as_table_mut()
                .and_then(|delegate| delegate.get_mut("system"))
            {
                inline_system_prompt_value(
                    system,
                    config_name,
                    &format!("[[delegates]][{index}].system"),
                )?;
            }
        }
    }

    Ok(())
}

fn inline_system_prompt_value(
    system: &mut toml::Value,
    config_name: &str,
    path: &str,
) -> anyhow::Result<()> {
    use anyhow::{Context, anyhow};

    match system {
        toml::Value::String(_) => Ok(()),
        toml::Value::Array(parts) => {
            for part in parts {
                if let toml::Value::Table(table) = part
                    && let Some(file_ref) = table.get("file").and_then(toml::Value::as_str)
                {
                    let asset_key = embedded_prompt_asset_key(file_ref).ok_or_else(|| {
                        anyhow!("Unsupported embedded prompt path '{file_ref}' in {config_name}")
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
            Ok(())
        }
        _ => Err(anyhow!(
            "Embedded {config_name} has unsupported {path} format"
        )),
    }
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

fn qmtcode_profile_catalog(profiles_dirs: &[PathBuf]) -> anyhow::Result<LocalProfileCatalog> {
    qmtcode_profile_catalog_with_user_dir(profiles_dirs, None)
}

fn qmtcode_profile_catalog_with_user_dir(
    profiles_dirs: &[PathBuf],
    user_profiles_dir: Option<PathBuf>,
) -> anyhow::Result<LocalProfileCatalog> {
    let embedded_single_coder =
        embedded_profile_config("single_coder.toml", EMBEDDED_SINGLE_CODER_CONFIG)?;
    let embedded_coder_delegate =
        embedded_profile_config("coder_delegate.toml", EMBEDDED_CODER_DELEGATE_CONFIG)?;
    let mut builder = LocalProfileCatalog::builder()
        .include_embedded_default(false)
        .embedded_profile_toml(embedded_single_coder)?
        .embedded_profile_toml(embedded_coder_delegate)?;

    // ~/.qmt/profiles is the conventional user-local profile directory; missing dirs are ignored.
    builder = match user_profiles_dir {
        Some(dir) => builder.default_user_dir(dir),
        None => builder.include_default_user_dir(true),
    };

    for dir in profiles_dirs {
        builder = builder.local_dir(dir.clone());
    }

    Ok(builder.build())
}

fn validate_profile_args(cli: &Cli) -> anyhow::Result<()> {
    if cli.config_file.is_some() && cli.profile.is_some() {
        anyhow::bail!("--profile cannot be used with explicit config path");
    }
    Ok(())
}

fn selected_profile_id(cli: &Cli) -> &str {
    cli.profile
        .as_deref()
        .unwrap_or(DEFAULT_EMBEDDED_PROFILE_KEY)
}

fn profile_kind_label(kind: Option<ProfileConfigKind>) -> &'static str {
    match kind {
        Some(ProfileConfigKind::Single) => "single",
        Some(ProfileConfigKind::Quorum) => "quorum",
        None => "unknown",
    }
}

fn profile_source_label(source: &ProfileSource) -> String {
    match source {
        ProfileSource::Embedded { key } | ProfileSource::EmbeddedToml { key } => {
            format!("embedded:{key}")
        }
        ProfileSource::LocalPath { path } => format!("local:{}", path.display()),
    }
}

const PROFILE_LIST_HEADERS: [&str; 5] = ["ID", "Name", "Kind", "Source", "Tags"];
const PROFILE_LIST_CAPS: [usize; 5] = [24, 28, 8, 64, 40];

fn truncate_cell(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let prefix: String = value.chars().take(max_chars - 3).collect();
    format!("{prefix}...")
}

fn padded_cell(value: &str, width: usize) -> String {
    format!("{value:<width$}")
}

fn compact_table(headers: &[&str], rows: &[Vec<String>], caps: &[usize]) -> String {
    let truncated_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(caps.iter())
                .map(|(value, cap)| truncate_cell(value, *cap))
                .collect()
        })
        .collect();

    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let row_width = truncated_rows
                .iter()
                .filter_map(|row| row.get(column))
                .map(|value| value.chars().count())
                .max()
                .unwrap_or(0);
            header.chars().count().max(row_width).min(caps[column])
        })
        .collect();

    let format_row = |cells: Vec<String>| -> String {
        cells
            .into_iter()
            .enumerate()
            .map(|(column, value)| {
                if column + 1 == widths.len() {
                    value
                } else {
                    format!("{}  ", padded_cell(&value, widths[column]))
                }
            })
            .collect::<String>()
            .trim_end()
            .to_string()
    };

    let mut lines = vec![format_row(
        headers.iter().map(|header| header.to_string()).collect(),
    )];
    lines.extend(truncated_rows.into_iter().map(format_row));
    lines.join("\n")
}

fn format_profile_list(profiles: &[querymt_agent::profiles::ProfileMetadata]) -> String {
    let rows: Vec<Vec<String>> = profiles
        .iter()
        .map(|profile| {
            vec![
                profile.id.clone(),
                profile.name.clone(),
                profile_kind_label(profile.config_kind).to_string(),
                profile_source_label(&profile.source),
                profile.tags.join(", "),
            ]
        })
        .collect();

    compact_table(&PROFILE_LIST_HEADERS, &rows, &PROFILE_LIST_CAPS)
}

/// Register the standard mesh actors (RemoteNodeManager, ProviderHostActor)
/// on a bootstrapped mesh using scoped DHT names.
#[cfg(feature = "remote")]
async fn register_mesh_actors(
    runner: &querymt_agent::prelude::AgentRunner,
    mesh: &querymt_agent::agent::remote::MeshHandle,
) {
    querymt_agent::agent::remote::spawn_and_register_local_mesh_actors(&runner.handle(), mesh)
        .await;
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
    #[cfg(feature = "remote")]
    let has_mesh_join = cli.mesh_join.is_some();
    #[cfg(not(feature = "remote"))]
    let has_mesh_join = false;

    // --mesh-invite implies --mesh (iroh host mode).
    #[cfg(feature = "remote")]
    let has_mesh_invite = cli.mesh_invite.is_some();
    #[cfg(not(feature = "remote"))]
    let has_mesh_invite = false;

    #[cfg(feature = "remote")]
    let has_mesh = cli.mesh.is_some() || has_mesh_join || has_mesh_invite;
    #[cfg(not(feature = "remote"))]
    let has_mesh = has_mesh_join || has_mesh_invite;

    validate_profile_args(&cli)?;

    let profile_catalog = qmtcode_profile_catalog(&cli.profiles_dir)?;
    if cli.list_profiles {
        let mut profiles = profile_catalog.list_profiles().await?;
        if let Some(config_path) = &cli.config_file {
            let config = querymt_agent::config::load_config(config_path).await?;
            let config_kind = match &config {
                Config::Single(_) => ProfileConfigKind::Single,
                Config::Multi(_) => ProfileConfigKind::Quorum,
            };
            profiles.push(ProfileMetadata {
                id: "config-file".to_string(),
                name: config_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("Config File")
                    .to_string(),
                description: Some("Explicit config path".to_string()),
                tags: Vec::new(),
                source: ProfileSource::LocalPath {
                    path: config_path.clone(),
                },
                config_kind: Some(config_kind),
                fingerprint: None,
            });
        }
        ensure_unique_profile_ids(&profiles)?;
        println!("{}", format_profile_list(&profiles));
        return Ok(());
    }

    if !is_acp && !is_api && !is_dashboard && !has_mesh {
        return Err(
            "No mode selected. Use --acp, --api, --dashboard, or --mesh, or --mesh-join.".into(),
        );
    }

    // Setup telemetry: ACP mode writes console logs to stderr (stdout is
    // reserved for JSON-RPC); dashboard/mesh modes use stdout.
    // OTLP export (traces + logs over gRPC) is active in all modes.
    querymt_utils::telemetry::setup_telemetry("qmtcode", env!("QMT_BUILD_VERSION"), is_acp);

    let shared_infra = AgentInfra::shared_with_db_path(cli.db.clone()).await?;

    #[allow(unused_variables, unused_assignments)]
    let mut profile_manager: Option<Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>> = None;
    let runner = if let Some(config_path) = &cli.config_file {
        eprintln!("Loading agent from: {}", config_path.display());
        from_config_with_infra(config_path, shared_infra.clone()).await?
    } else {
        let selected_profile = selected_profile_id(&cli).to_string();
        eprintln!("Loading agent from profile: {selected_profile}");
        let document = profile_catalog.load_profile(&selected_profile).await?;
        let runner = from_config_value_with_infra(document.config, shared_infra.clone()).await?;
        let catalog: Arc<dyn ProfileCatalog> = Arc::new(profile_catalog);
        #[allow(unused_assignments)]
        {
            profile_manager = Some(Arc::new(ProfileRuntimeManager::with_infra_boxed(
                catalog,
                selected_profile.clone(),
                shared_infra.clone(),
            )));
        }
        runner
    };

    eprintln!("Agent loaded successfully!\n");

    // ── Phase 6: Mesh Bootstrap ───────────────────────────────────────────────
    //
    // Three mesh modes:
    //   1. --mesh (LAN): TCP + QUIC + mDNS, same-subnet discovery
    //   2. --mesh --mesh-invite (iroh host): start iroh mesh, print invite token
    //   3. --mesh-join=TOKEN (iroh join): join existing mesh via invite token
    //
    // Modes 2 and 3 require the `remote` feature.

    // ── Mode 3: Join via invite token ─────────────────────────────────────────
    #[cfg(feature = "remote")]
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
    #[cfg(feature = "remote")]
    let mesh_invite_handled = cli.mesh_join.is_some();

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
        let is_iroh_host = cli.mesh_invite.is_some();

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
            stream_reconnect_grace: std::time::Duration::from_secs(120),
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
                #[cfg(feature = "remote")]
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
            let server = runner.server();
            let server = if let Some(manager) = profile_manager.clone() {
                server.with_profiles(manager)
            } else {
                server
            };
            server.run(addr, ServerMode::Api).await?;
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
            let server = runner.server();
            let server = if let Some(manager) = profile_manager.clone() {
                server.with_profiles(manager)
            } else {
                server
            };
            server.run(addr, ServerMode::Dashboard).await?;
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
    use clap::CommandFactory;

    #[test]
    fn embedded_config_inlines_system_prompts_exactly() {
        let config = embedded_profile_config("single_coder.toml", EMBEDDED_SINGLE_CODER_CONFIG)
            .expect("embedded config should load");
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
    fn embedded_coder_delegate_inlines_planner_and_delegate_system_prompts() {
        let config = embedded_profile_config("coder_delegate.toml", EMBEDDED_CODER_DELEGATE_CONFIG)
            .expect("embedded config should load");
        let value: toml::Value =
            toml::from_str(&config).expect("embedded config should parse as TOML");

        let planner_system = value
            .get("planner")
            .and_then(toml::Value::as_table)
            .and_then(|planner| planner.get("system"))
            .and_then(toml::Value::as_array)
            .expect("embedded config should contain [planner].system array");
        let planner_inlined: Vec<&str> = planner_system
            .iter()
            .map(|part| part.as_str().expect("system part must be an inline string"))
            .collect();
        assert_eq!(
            planner_inlined,
            vec![
                include_str!("prompts/default_system.txt"),
                include_str!("prompts/code_meta.jinja2"),
            ]
        );

        let delegates = value
            .get("delegates")
            .and_then(toml::Value::as_array)
            .expect("embedded config should contain delegates");
        let coder_system = delegates[0]
            .get("system")
            .and_then(toml::Value::as_array)
            .expect("coder delegate should contain system array");
        assert_eq!(
            coder_system[0]
                .as_str()
                .expect("system part must be an inline string"),
            include_str!("prompts/default_system.txt")
        );
        assert!(
            delegates[1]
                .get("system")
                .and_then(toml::Value::as_array)
                .expect("explorer delegate should contain system array")[0]
                .is_str()
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

    #[test]
    fn profile_args_reject_explicit_config_and_profile() {
        let cli = Cli::try_parse_from(["qmtcode", "agent.toml", "--profile", "default"])
            .expect("CLI args should parse");

        let err = validate_profile_args(&cli).expect_err("combination should be rejected");
        assert!(
            err.to_string()
                .contains("--profile cannot be used with explicit config path")
        );
    }

    #[test]
    fn profile_list_format_includes_required_columns() {
        let output = format_profile_list(&[querymt_agent::profiles::ProfileMetadata {
            id: "default".to_string(),
            name: "Default".to_string(),
            description: None,
            tags: vec!["coding".to_string(), "planner".to_string()],
            source: ProfileSource::EmbeddedToml {
                key: "default".to_string(),
            },
            config_kind: Some(ProfileConfigKind::Single),
            fingerprint: None,
        }]);

        let header = output.lines().next().expect("header line");
        assert!(header.contains("ID"));
        assert!(header.contains("Name"));
        assert!(header.contains("Kind"));
        assert!(header.contains("Source"));
        assert!(header.contains("Tags"));
        assert!(!output.contains('\t'));
    }

    #[test]
    fn profile_list_format_aligns_rows_and_spaces_tags() {
        let output = format_profile_list(&[
            querymt_agent::profiles::ProfileMetadata {
                id: "default".to_string(),
                name: "Default".to_string(),
                description: None,
                tags: Vec::new(),
                source: ProfileSource::EmbeddedToml {
                    key: "default".to_string(),
                },
                config_kind: Some(ProfileConfigKind::Single),
                fingerprint: None,
            },
            querymt_agent::profiles::ProfileMetadata {
                id: "coder-delegate".to_string(),
                name: "Coder Delegate".to_string(),
                description: None,
                tags: vec!["coding".to_string(), "planner".to_string()],
                source: ProfileSource::LocalPath {
                    path: PathBuf::from("/home/me/.qmt/profiles/coder.toml"),
                },
                config_kind: Some(ProfileConfigKind::Quorum),
                fingerprint: None,
            },
        ]);

        assert_eq!(
            output,
            "ID              Name            Kind    Source                                   Tags\n\
             default         Default         single  embedded:default\n\
             coder-delegate  Coder Delegate  quorum  local:/home/me/.qmt/profiles/coder.toml  coding, planner"
        );
    }

    #[test]
    fn profile_list_format_truncates_wide_cells() {
        let output = format_profile_list(&[querymt_agent::profiles::ProfileMetadata {
            id: "profile-id-that-is-far-too-wide-for-the-list".to_string(),
            name: "Profile name that is also far too wide for the list".to_string(),
            description: None,
            tags: vec!["tag".repeat(20)],
            source: ProfileSource::LocalPath {
                path: PathBuf::from(format!("/{}", "very-long-segment/".repeat(8))),
            },
            config_kind: Some(ProfileConfigKind::Single),
            fingerprint: None,
        }]);

        let row = output.lines().nth(1).expect("profile row");
        assert!(row.contains("..."));
        assert!(row.len() <= 24 + 28 + 8 + 64 + 40 + (4 * 2));
    }

    #[tokio::test]
    async fn qmtcode_catalog_uses_inline_embedded_default() {
        let temp = tempfile::tempdir().expect("temp dir");
        let missing_user_dir = temp.path().join("missing");
        let catalog = qmtcode_profile_catalog_with_user_dir(&[], Some(missing_user_dir))
            .expect("catalog should build");
        let profiles = catalog.list_profiles().await.expect("profiles should list");

        assert_eq!(profiles.len(), 2);
        let default = profiles
            .iter()
            .find(|profile| profile.id == DEFAULT_EMBEDDED_PROFILE_KEY)
            .expect("default profile should be listed");
        assert_eq!(default.name, "Default");
        assert_eq!(default.tags, vec!["coding", "single-agent"]);
        assert_eq!(default.config_kind, Some(ProfileConfigKind::Single));
        assert!(matches!(default.source, ProfileSource::EmbeddedToml { .. }));

        let document = catalog
            .load_profile(DEFAULT_EMBEDDED_PROFILE_KEY)
            .await
            .expect("inline embedded profile should load");
        assert!(matches!(document.config, Config::Single(_)));
    }

    #[tokio::test]
    async fn qmtcode_catalog_lists_and_loads_coder_delegate() {
        let temp = tempfile::tempdir().expect("temp dir");
        let missing_user_dir = temp.path().join("missing");
        let catalog = qmtcode_profile_catalog_with_user_dir(&[], Some(missing_user_dir))
            .expect("catalog should build");
        let profiles = catalog.list_profiles().await.expect("profiles should list");
        let coder_delegate = profiles
            .iter()
            .find(|profile| profile.id == "coder-delegate")
            .expect("coder delegate profile should be listed");

        assert_eq!(coder_delegate.name, "Coder Delegate");
        assert_eq!(
            coder_delegate.description.as_deref(),
            Some("Multi-agent coder profile with planner, coder, and explorer delegates")
        );
        assert_eq!(
            coder_delegate.tags,
            vec!["coding", "delegation", "multi-agent"]
        );
        assert_eq!(coder_delegate.config_kind, Some(ProfileConfigKind::Quorum));

        let document = catalog
            .load_profile("coder-delegate")
            .await
            .expect("inline embedded profile should load");
        assert!(matches!(document.config, Config::Multi(_)));
    }

    #[tokio::test]
    async fn qmtcode_catalog_lists_default_user_dir_profiles() {
        let user_dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            user_dir.path().join("user-coder.toml"),
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        )
        .expect("write profile");
        let catalog =
            qmtcode_profile_catalog_with_user_dir(&[], Some(user_dir.path().to_path_buf()))
                .expect("catalog should build");

        let profiles = catalog.list_profiles().await.expect("profiles should list");
        assert!(profiles.iter().any(|profile| profile.id == "user-coder"));
    }

    #[test]
    fn profile_flags_are_exposed_without_remote_service_flags() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--profiles-dir"));
        assert!(help.contains("--profile"));
        assert!(help.contains("--list-profiles"));
        assert!(!help.contains("--profiles-url"));
    }

    #[test]
    fn db_flag_is_exposed() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--db <path>"));
        assert!(help.contains("QMT_SESSIONS_DB"));
    }
}
