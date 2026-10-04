//! QMT Code Agent Example
//!
//! Multi-mode agent that can run as ACP stdio server, API server, embedded web dashboard, or mesh node.
//!
//! ## Usage
//!
//! ```bash
//! # ACP stdio mode
//! cargo run --example qmtcode -- --acp
//!
//! # API server mode (ACP WebSocket and SFT export)
//! cargo run --example qmtcode --features api -- --api
//! cargo run --example qmtcode --features api -- --api=0.0.0.0:8080
//!
//! # Embedded Svelte dashboard mode (from the workspace root)
//! cargo run --example qmtcode --features dashboard -- --dashboard
//! cargo run --example qmtcode --features dashboard -- --dashboard=0.0.0.0:8080
//!
//! # Mesh mode: LAN plus any previously joined/hosted Iroh meshes
//! cargo run --example qmtcode --features remote -- --mesh
//! cargo run --example qmtcode --features remote -- --mesh=/ip4/0.0.0.0/tcp/0
//! cargo run --example qmtcode --features remote -- --mesh --mesh-no-lan
//!
//! # UDP offload compatibility override (also works with --profile and --mesh-join)
//! cargo run --example qmtcode --features remote -- --mesh --mesh-iroh-gso=false
//! QMT_MESH_IROH_GSO=false cargo run --example qmtcode --features remote -- --mesh
//!
//! # Dashboard mode with mesh enabled
//! cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh
//! cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh --mesh-no-lan
//!
//! # Internet mesh: host and print a new invite token
//! cargo run --example qmtcode --features "remote" -- --mesh --mesh-invite
//! cargo run --example qmtcode --features "remote" -- --mesh --mesh-invite="My Dev Mesh"
//!
//! # Internet mesh: first-time join via invite token
//! cargo run --example qmtcode --features "remote" -- --mesh-join=qmt://mesh/join/TOKEN
//! # Future runs after joining only need --mesh
//!
//! # Running a built binary with embedded default config
//! ./qmtcode --mesh
//! ```

use clap::{ArgAction, ArgGroup, Parser};
use querymt_agent::prelude::*;
use querymt_agent::profiles::{
    DEFAULT_EMBEDDED_PROFILE_KEY, LocalProfileCatalog, ProfileCatalog, ProfileConfigKind,
    ProfileMetadata, ProfileSource, ensure_unique_profile_ids,
    standard_embedded_profile_catalog_builder,
};
#[cfg(any(feature = "api", feature = "dashboard"))]
use querymt_agent::server::ServerMode;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "api")]
const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:3000";
const DEFAULT_ACP_WS_ADDR: &str = "127.0.0.1:3030";
#[cfg(feature = "remote")]
const DEFAULT_MESH_ADDR: &str = "/ip4/0.0.0.0/tcp/0";
#[cfg(feature = "remote")]
const DEFAULT_MESH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
#[cfg(feature = "remote")]
const DEFAULT_MESH_STREAM_RECONNECT_GRACE: std::time::Duration =
    std::time::Duration::from_secs(120);
#[derive(Debug, Parser)]
#[command(name = "qmtcode")]
#[command(version = env!("QMT_BUILD_VERSION"))]
#[command(
    about = "Run QueryMT coder agent in ACP mode, API mode, dashboard mode, or as a mesh node"
)]
#[command(
    after_help = "Examples:\n  qmtcode --acp\n  qmtcode --acp-ws\n  qmtcode --acp-ws=0.0.0.0:42069\n  qmtcode --api\n  qmtcode --api=0.0.0.0:8080\n  qmtcode --dashboard\n  qmtcode --dashboard=0.0.0.0:8080\n  qmtcode --mesh\n  qmtcode --mesh=/ip4/0.0.0.0/tcp/9001\n  qmtcode --api --mesh\n  qmtcode --mesh --mesh-invite\n  qmtcode --mesh --mesh-invite=\"My Mesh\"\n  qmtcode --mesh-join=qmt://mesh/join/TOKEN\n  qmtcode path/to/config.toml --acp"
)]
#[cfg_attr(
    all(feature = "api", feature = "dashboard"),
    command(group(ArgGroup::new("transport").args(["acp", "acp_ws", "api", "dashboard"]).multiple(false)))
)]
#[cfg_attr(
    all(feature = "api", not(feature = "dashboard")),
    command(group(ArgGroup::new("transport").args(["acp", "acp_ws", "api"]).multiple(false)))
)]
#[cfg_attr(
    all(not(feature = "api"), feature = "dashboard"),
    command(group(ArgGroup::new("transport").args(["acp", "acp_ws", "dashboard"]).multiple(false)))
)]
#[cfg_attr(
    all(not(feature = "api"), not(feature = "dashboard")),
    command(group(ArgGroup::new("transport").args(["acp", "acp_ws"]).multiple(false)))
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

    /// Run as ACP WebSocket server; optionally set bind address
    #[arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_ACP_WS_ADDR)]
    acp_ws: Option<String>,

    /// Run API server for alternate UIs; optionally set bind address
    #[cfg(feature = "api")]
    #[arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_SERVER_ADDR)]
    api: Option<String>,

    /// Run the embedded Svelte dashboard over ACP; optionally set bind address
    #[cfg(feature = "dashboard")]
    #[arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_SERVER_ADDR)]
    dashboard: Option<String>,

    /// Enable mesh networking for cross-machine sessions.
    ///
    /// Starts LAN discovery/listening and also reconnects any previously joined
    /// or hosted Iroh meshes persisted in `~/.qmt/mesh_state.json`.
    ///
    /// Optionally specify the LAN multiaddr to listen on
    /// (default: /ip4/0.0.0.0/tcp/0).
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

    /// Disable LAN listen/mDNS when running mesh mode.
    ///
    /// Useful for cloud deployments that should only reconnect hosted/joined
    /// Iroh meshes and must not use mDNS.
    #[cfg(feature = "remote")]
    #[arg(long)]
    mesh_no_lan: bool,

    /// Override iroh actor-endpoint GSO (UDP segmentation offload).
    ///
    /// Precedence: CLI > QMT_MESH_IROH_GSO > config/profile TOML > true.
    /// Set false as a UDP offload compatibility workaround; restart to change it.
    #[cfg(feature = "remote")]
    #[arg(long, env = "QMT_MESH_IROH_GSO", value_name = "true|false", action = ArgAction::Set)]
    mesh_iroh_gso: Option<bool>,

    /// Create and print a signed mesh invite token, then host that Iroh mesh.
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
    /// This is the first-join path. After a successful join, future runs only
    /// need `--mesh`, which reloads stored memberships automatically.
    ///
    /// Examples:
    ///   --mesh-join=qmt://mesh/join/eyJpbnZ...
    ///   --mesh-join=eyJpbnZ...
    #[cfg(feature = "remote")]
    #[arg(long, value_name = "token")]
    mesh_join: Option<String>,
}

fn qmtcode_profile_catalog(profiles_dirs: &[PathBuf]) -> anyhow::Result<LocalProfileCatalog> {
    qmtcode_profile_catalog_with_user_dir(profiles_dirs, None)
}

fn qmtcode_profile_catalog_with_user_dir(
    profiles_dirs: &[PathBuf],
    user_profiles_dir: Option<PathBuf>,
) -> anyhow::Result<LocalProfileCatalog> {
    let mut builder = standard_embedded_profile_catalog_builder()?;

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

#[cfg(feature = "remote")]
fn configure_mesh_iroh_gso(mut config: Config, override_value: Option<bool>) -> (Config, bool) {
    let mesh = match &mut config {
        Config::Single(config) => &mut config.mesh,
        Config::Multi(config) => &mut config.mesh,
    };
    mesh.iroh_gso = override_value.unwrap_or(mesh.iroh_gso);
    let iroh_gso = mesh.iroh_gso;
    (config, iroh_gso)
}

// Keep the process-wide endpoint setting consistent across profile switches/reloads.
#[cfg(feature = "remote")]
struct MeshGsoProfileCatalog {
    inner: LocalProfileCatalog,
    iroh_gso: bool,
}

#[cfg(feature = "remote")]
#[async_trait::async_trait]
impl ProfileCatalog for MeshGsoProfileCatalog {
    async fn list_profiles(&self) -> anyhow::Result<Vec<ProfileMetadata>> {
        self.inner.list_profiles().await
    }

    async fn load_profile(
        &self,
        id: &str,
    ) -> anyhow::Result<querymt_agent::profiles::ProfileDocument> {
        let mut document = self.inner.load_profile(id).await?;
        let (config, _) = configure_mesh_iroh_gso(document.config, Some(self.iroh_gso));
        document.config = config;
        Ok(document)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        self.inner.watch_roots()
    }
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
    runner.handle().set_mesh(mesh.clone());
    if let Err(e) = runner.handle().ensure_mesh_published(None).await {
        eprintln!("Warning: failed to publish mesh actors: {e}");
    }
}

#[cfg(feature = "remote")]
fn load_stored_iroh_scopes() -> anyhow::Result<Vec<querymt_agent::agent::remote::IrohMeshConfig>> {
    use querymt_agent::agent::remote::IrohMeshConfig;
    use querymt_agent::agent::remote::mesh_state::{MeshStateStore, default_mesh_state_path};

    let path = default_mesh_state_path()?;
    let store = MeshStateStore::load_or_create(&path)?;
    Ok(store
        .active_mesh_ids()
        .into_iter()
        .map(|mesh_id| IrohMeshConfig {
            mesh_id,
            invite: None,
            name: None,
        })
        .collect())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let hf_download_config = querymt_provider_common::configure_hf_download_concurrency();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(hf_download_config))
}

async fn run(
    hf_download_config: querymt_provider_common::HfDownloadConcurrencyConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    querymt_provider_common::log_hf_download_concurrency(&hf_download_config);
    let cli = Cli::parse();
    let is_acp = cli.acp;
    let is_acp_ws = cli.acp_ws.is_some();
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

    if !is_acp && !is_acp_ws && !is_api && !is_dashboard && !has_mesh {
        return Err(
            "No mode selected. Use --acp, --acp-ws, --api, --dashboard, --mesh, or --mesh-join."
                .into(),
        );
    }

    // Setup telemetry: ACP mode writes console logs to stderr (stdout is
    // reserved for JSON-RPC); dashboard/mesh modes use stdout.
    // OTLP export (traces + logs over gRPC) is active in all modes.
    querymt_utils::telemetry::setup_telemetry("qmtcode", env!("QMT_BUILD_VERSION"), is_acp);

    let shared_infra = AgentInfra::shared_with_db_path(cli.db.clone()).await?;

    #[cfg(feature = "remote")]
    let mesh_iroh_gso;

    let runner = if let Some(config_path) = &cli.config_file {
        eprintln!("Loading agent from: {}", config_path.display());
        let config = querymt_agent::config::load_config(config_path).await?;
        #[cfg(feature = "remote")]
        let config = {
            let (config, gso) = configure_mesh_iroh_gso(config, cli.mesh_iroh_gso);
            mesh_iroh_gso = gso;
            config
        };
        from_config_value_with_infra(config, shared_infra.clone()).await?
    } else {
        let selected_profile = selected_profile_id(&cli).to_string();
        eprintln!("Loading agent from profile: {selected_profile}");
        #[cfg(feature = "remote")]
        let profile_catalog = {
            let document = profile_catalog.load_profile(&selected_profile).await?;
            let (_, gso) = configure_mesh_iroh_gso(document.config, cli.mesh_iroh_gso);
            mesh_iroh_gso = gso;
            MeshGsoProfileCatalog {
                inner: profile_catalog,
                iroh_gso: gso,
            }
        };
        let catalog: Arc<dyn ProfileCatalog> = Arc::new(profile_catalog);
        let profiles = AgentProfiles::new(catalog, selected_profile, shared_infra.clone());
        let runtime = profiles.active_runtime().await?;
        AgentRunner::new(runtime.agent().clone()).with_profiles(profiles)
    };

    eprintln!("Agent loaded successfully!\n");

    #[cfg(feature = "remote")]
    let mut mesh_runtime: Option<querymt_agent::agent::remote::MeshRuntimeHandle> = None;

    // ── Phase 6: Mesh Bootstrap ───────────────────────────────────────────────
    //
    // Simplified mesh modes:
    //   1. --mesh: LAN + any stored Iroh memberships
    //   2. --mesh --mesh-invite: same as --mesh, plus create/print a new invite
    //   3. --mesh-join=TOKEN: first-time join via invite token
    //
    // After a successful --mesh-join, future runs only need --mesh.

    // ── Mode 3: Join via invite token ─────────────────────────────────────────
    #[cfg(feature = "remote")]
    if let Some(ref token) = cli.mesh_join {
        use querymt_agent::agent::remote::invite::SignedInviteGrant;
        use querymt_agent::agent::remote::mesh::join_mesh_via_invite_with_gso;

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

        match join_mesh_via_invite_with_gso(&invite, None, mesh_iroh_gso).await {
            Ok(runtime) => {
                let mesh = runtime.as_mesh_handle().clone();
                eprintln!("Joined mesh: peer_id={}", mesh.peer_id());
                register_mesh_actors(&runner, &mesh).await;
                runner.handle().set_mesh(mesh.clone());
                if let Some(manager) = runner.profiles() {
                    manager.set_mesh(mesh).await;
                }
                mesh_runtime = Some(runtime);
            }
            Err(e) => {
                eprintln!("Warning: mesh join failed: {}", e);
                eprintln!("Continuing without mesh networking...");
            }
        }
    }

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
        && cli.mesh_join.is_none()
    {
        use querymt_agent::agent::remote::bootstrap_mesh_runtime;
        use querymt_agent::agent::remote::{
            IrohMeshConfig, LanDiscovery, LanMeshConfig, MeshRuntimeConfig,
        };

        let mut iroh_scopes = load_stored_iroh_scopes()?;
        if cli.mesh_invite.is_some() && iroh_scopes.is_empty() {
            let identity = querymt_agent::agent::remote::identity::load_or_generate_keypair(None)?;
            let host_peer_id = identity.public().to_peer_id().to_string();
            let mesh_name = cli.mesh_invite.as_ref().and_then(|name| {
                if name.is_empty() {
                    None
                } else {
                    Some(name.as_str())
                }
            });
            iroh_scopes.push(IrohMeshConfig {
                mesh_id: querymt_agent::agent::remote::invite::mesh_id_for(
                    &host_peer_id,
                    mesh_name,
                ),
                invite: None,
                name: cli.mesh_invite.clone().filter(|name| !name.is_empty()),
            });
        }

        let runtime_config = MeshRuntimeConfig {
            enabled: true,
            lan: if cli.mesh_no_lan {
                None
            } else {
                Some(LanMeshConfig {
                    listen: Some(mesh_addr.clone()),
                    discovery: LanDiscovery::Mdns,
                    directory: querymt_agent::agent::remote::DirectoryMode::default(),
                })
            },
            iroh_enabled: true,
            iroh_gso: mesh_iroh_gso,
            iroh_scopes,
            identity_file: None,
            request_timeout: DEFAULT_MESH_REQUEST_TIMEOUT,
            stream_reconnect_grace: DEFAULT_MESH_STREAM_RECONNECT_GRACE,
            node_name: None,
            peers: Vec::new(),
            auto_fallback: false,
        };

        match bootstrap_mesh_runtime(&runtime_config).await {
            Ok(runtime) => {
                let mesh = runtime.as_mesh_handle().clone();
                eprintln!("Kameo mesh bootstrapped: peer_id={}", mesh.peer_id());
                if runtime_config.lan.is_some() {
                    eprintln!("Mesh listening on: {}", mesh_addr);
                }
                match (
                    runtime_config.lan.is_some(),
                    runtime_config.iroh_scopes.is_empty(),
                ) {
                    (true, true) => eprintln!("Mesh transports: LAN"),
                    (true, false) => eprintln!(
                        "Mesh transports: LAN + {} stored/hosted Iroh scope(s)",
                        runtime_config.iroh_scopes.len()
                    ),
                    (false, false) => eprintln!(
                        "Mesh transports: Iroh ({} stored/hosted scope(s))",
                        runtime_config.iroh_scopes.len()
                    ),
                    (false, true) => eprintln!("Mesh transports: none"),
                }

                if let Some(name) = &cli.mesh_invite {
                    let mesh_name = if name.is_empty() {
                        None
                    } else {
                        Some(name.clone())
                    };
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
                runner.handle().set_mesh(mesh.clone());
                if let Some(manager) = runner.profiles() {
                    manager.set_mesh(mesh).await;
                }
                mesh_runtime = Some(runtime);
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
    } else if let Some(addr) = cli.acp_ws.as_deref() {
        log::info!("Starting ACP WebSocket server at ws://{addr}/acp/ws...");
        let transport = format!("ws://{addr}");
        runner.acp(&transport).await?;
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

    // Request shutdown without blocking on background drains; telemetry is finalized last.
    runner.shutdown().await;
    #[cfg(feature = "remote")]
    if let Some(runtime) = mesh_runtime.take() {
        runtime.request_shutdown();
    }
    querymt_utils::telemetry::flush_telemetry();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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

    #[cfg(feature = "remote")]
    fn parse_gso_cli(args: &[&str]) -> Result<Cli, clap::Error> {
        use clap::FromArgMatches;
        // Ignore the caller's environment without mutating process-global state.
        let matches = Cli::command()
            .mut_arg("mesh_iroh_gso", |arg| arg.env(None::<&str>))
            .try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[cfg(feature = "remote")]
    #[test]
    fn mesh_iroh_gso_cli_accepts_explicit_values_for_all_mesh_modes() {
        for mode in ["--mesh", "--mesh-invite=Test", "--mesh-join=TOKEN"] {
            let cli = parse_gso_cli(&["qmtcode", "--profile=default", mode]).unwrap();
            assert_eq!(cli.mesh_iroh_gso, None);
            for value in [false, true] {
                let arg = format!("--mesh-iroh-gso={value}");
                let cli = parse_gso_cli(&["qmtcode", "--profile=default", mode, &arg]).unwrap();
                assert_eq!(cli.mesh_iroh_gso, Some(value));
            }
        }
    }

    #[cfg(feature = "remote")]
    #[test]
    fn mesh_iroh_gso_cli_rejects_invalid_or_missing_values() {
        for arg in [
            "--mesh-iroh-gso=invalid",
            "--mesh-iroh-gso=0",
            "--mesh-iroh-gso",
        ] {
            assert!(parse_gso_cli(&["qmtcode", "--mesh", arg]).is_err());
        }
    }

    #[cfg(feature = "remote")]
    #[test]
    fn mesh_iroh_gso_environment_and_cli_precedence() {
        const CASE_ENV: &str = "QMT_TEST_MESH_IROH_GSO_CASE";
        if let Ok(case) = std::env::var(CASE_ENV) {
            let mut args = vec!["qmtcode", "--profile=default", "--mesh"];
            if case == "cli-on" {
                args.push("--mesh-iroh-gso=true");
            } else if case == "cli-off" {
                args.push("--mesh-iroh-gso=false");
            }
            let parsed = Cli::try_parse_from(args);
            if case == "invalid" {
                assert!(parsed.is_err());
            } else {
                let expected = match case.as_str() {
                    "on" | "cli-on" => Some(true),
                    "off" | "cli-off" => Some(false),
                    "unset" => None,
                    _ => panic!("unexpected test case: {case}"),
                };
                assert_eq!(parsed.unwrap().mesh_iroh_gso, expected);
            }
            return;
        }
        // Subprocesses isolate environment tests from the parallel test runner.
        for (case, value) in [
            ("unset", None),
            ("on", Some("true")),
            ("off", Some("false")),
            ("invalid", Some("invalid")),
            ("cli-on", Some("false")),
            ("cli-off", Some("true")),
        ] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args([
                "--exact",
                "tests::mesh_iroh_gso_environment_and_cli_precedence",
            ]);
            child.env(CASE_ENV, case).env_remove("QMT_MESH_IROH_GSO");
            if let Some(value) = value {
                child.env("QMT_MESH_IROH_GSO", value);
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "case {case}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn mesh_iroh_gso_overrides_config_and_profile_before_startup() {
        let dir = tempfile::tempdir().unwrap();
        let inner = LocalProfileCatalog::builder()
            .include_default_user_dir(false)
            .local_dir(dir.path())
            .embedded_config_toml("single", "Single", None,
                "[agent]\nprovider = 'test'\nmodel = 'test'\n[mesh]\nenabled = true\ntransport = 'iroh'\niroh_gso = false")
            .embedded_config_toml("multi", "Multi", None,
                "[quorum]\n[planner]\nprovider = 'test'\nmodel = 'test'\n[mesh]\nenabled = true\ntransport = 'iroh'\niroh_gso = false")
            .build();
        for id in ["single", "multi"] {
            let document = inner.load_profile(id).await.unwrap();
            let (config, gso) = configure_mesh_iroh_gso(document.config, None);
            assert!(!gso);
            let (config, gso) = configure_mesh_iroh_gso(config, Some(true));
            assert!(gso);
            let (_, gso) = configure_mesh_iroh_gso(config, Some(false));
            assert!(!gso);
        }
        for gso in [false, true] {
            let catalog = MeshGsoProfileCatalog {
                inner: inner.clone(),
                iroh_gso: gso,
            };
            assert_eq!(catalog.watch_roots(), inner.watch_roots());
            assert_eq!(
                catalog.list_profiles().await.unwrap().len(),
                inner.list_profiles().await.unwrap().len()
            );
            for id in ["single", "multi"] {
                let document = catalog.load_profile(id).await.unwrap();
                let (_, resolved) = configure_mesh_iroh_gso(document.config, None);
                assert_eq!(resolved, gso);
            }
        }
    }

    #[cfg(feature = "remote")]
    #[test]
    fn mesh_iroh_gso_help_exposes_cli_environment_and_precedence() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--mesh-iroh-gso <true|false>"));
        assert!(help.contains("QMT_MESH_IROH_GSO"));
        assert!(help.contains("CLI > QMT_MESH_IROH_GSO > config/profile TOML > true"));
    }

    #[cfg(not(feature = "remote"))]
    #[test]
    fn mesh_iroh_gso_flag_requires_remote_feature() {
        assert!(Cli::try_parse_from(["qmtcode", "--mesh-iroh-gso=false"]).is_err());
    }

    #[test]
    fn db_flag_is_exposed() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--db <path>"));
        assert!(help.contains("QMT_SESSIONS_DB"));
    }

    #[test]
    fn acp_ws_flag_defaults_to_localhost() {
        let cli = Cli::try_parse_from(["qmtcode", "--acp-ws"]).expect("CLI args should parse");
        assert_eq!(cli.acp_ws.as_deref(), Some(DEFAULT_ACP_WS_ADDR));
    }

    #[test]
    fn acp_ws_flag_accepts_bind_override() {
        let cli = Cli::try_parse_from(["qmtcode", "--acp-ws=0.0.0.0:42069"])
            .expect("CLI args should parse");
        assert_eq!(cli.acp_ws.as_deref(), Some("0.0.0.0:42069"));
    }

    #[test]
    fn acp_and_acp_ws_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["qmtcode", "--acp", "--acp-ws"]).is_err());
    }

    #[cfg(feature = "dashboard")]
    #[test]
    fn dashboard_flag_defaults_to_localhost() {
        let cli = Cli::try_parse_from(["qmtcode", "--dashboard"]).expect("CLI args should parse");
        assert_eq!(cli.dashboard.as_deref(), Some(DEFAULT_SERVER_ADDR));
    }
}
