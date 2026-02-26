//! Sandboxed worker process for querymt agent sessions.
//!
//! Each worker hosts exactly one `SessionActor` inside a nono sandbox.
//! The orchestrator spawns workers via `WorkerManager` and communicates
//! through the kameo actor mesh.
//!
//! # Usage
//!
//! ```text
//! querymt-worker --cwd /project --mode build --session-id <id> \
//!     --mesh-peer /ip4/127.0.0.1/tcp/9000 --db-path /path/to/agent.db
//! ```
//!
//! # Lifecycle
//!
//! 1. Parse CLI arguments
//! 2. Apply nono sandbox (irreversible, before any tool execution)
//! 3. Bootstrap libp2p mesh and connect to orchestrator
//! 4. Build minimal AgentConfig from shared database
//! 5. Spawn a `SessionActor` and register in the DHT
//! 6. Signal readiness and run until killed by orchestrator

use querymt_worker::config;
use querymt_worker::extension::ExtensionManager;

use clap::Parser;
use kameo::actor::Spawn;
use querymt_agent::agent::core::SessionRuntime;
use querymt_agent::agent::remote::dht_name;
use querymt_agent::agent::remote::mesh::{MeshConfig, MeshDiscovery, bootstrap_mesh};
use querymt_agent::agent::session_actor::SessionActor;
use querymt_sandbox::SandboxPolicy;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

/// CLI arguments for the sandboxed worker process.
#[derive(Parser, Debug)]
#[command(
    name = "querymt-worker",
    about = "Sandboxed worker for querymt sessions"
)]
struct Args {
    /// Working directory for this session.
    #[arg(long)]
    cwd: PathBuf,

    /// Initial agent mode (build, plan, review).
    #[arg(long, default_value = "build")]
    mode: String,

    /// Unique session identifier assigned by the orchestrator.
    #[arg(long)]
    session_id: String,

    /// Path to the supervisor Unix socket for extension requests (mode switching).
    /// If not provided, runtime capability expansion is disabled.
    #[arg(long)]
    supervisor_socket: Option<PathBuf>,

    /// Disable sandbox enforcement (for debugging/development only).
    #[arg(long, default_value = "false")]
    no_sandbox: bool,

    /// Multiaddr the worker's libp2p swarm listens on.
    /// If not provided, a random port is used.
    #[arg(long)]
    mesh_listen: Option<String>,

    /// Orchestrator's libp2p multiaddr to bootstrap against.
    #[arg(long)]
    mesh_peer: String,

    /// Path to the shared SQLite database.
    #[arg(long)]
    db_path: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let mode: querymt_agent::agent::core::AgentMode =
        args.mode.parse().map_err(|e: String| anyhow::anyhow!(e))?;

    tracing::info!(
        cwd = %args.cwd.display(),
        mode = %mode,
        session_id = %args.session_id,
        no_sandbox = args.no_sandbox,
        "Starting querymt worker"
    );

    // 1. Apply sandbox BEFORE any tool execution
    if !args.no_sandbox {
        let policy = SandboxPolicy {
            cwd: args.cwd.clone(),
            read_only: mode.is_read_only(),
            allow_network: true, // needed for web_fetch, browse, LLM API calls
            db_path: Some(args.db_path.clone()),
            // Allow the socket directory so the worker can connect to the
            // supervisor socket after the sandbox is applied (step 2 below).
            socket_dir: args
                .supervisor_socket
                .as_ref()
                .and_then(|p| p.parent().map(|d| d.to_path_buf())),
        };
        policy.apply()?;
        tracing::info!("Sandbox applied successfully");
    } else {
        tracing::warn!("Sandbox disabled via --no-sandbox flag");
    }

    // 2. Connect supervisor socket and build ExtensionManager (if provided).
    //
    // The ExtensionManager wraps the supervisor socket and manages Seatbelt
    // extension tokens for dynamic Build/Plan mode switching. It must be
    // connected AFTER sandbox_init() but BEFORE any tool calls.
    let extension_manager: Option<Arc<ExtensionManager>> =
        if let Some(ref socket_path) = args.supervisor_socket {
            tracing::info!(
                socket = %socket_path.display(),
                "Connecting to supervisor socket"
            );
            let socket = nono::SupervisorSocket::connect(socket_path)?;
            tracing::info!("Supervisor socket connected");

            let mgr = Arc::new(ExtensionManager::new(
                socket,
                args.cwd.clone(),
                args.session_id.clone(),
            ));

            // If the initial mode is Build, request write access immediately.
            // The static sandbox profile only grants Read to CWD; write access
            // must be obtained via an extension token from the orchestrator.
            if !mode.is_read_only() {
                tracing::info!("Requesting initial write extension for Build mode");
                mgr.request_write();
            }

            Some(mgr)
        } else {
            tracing::debug!("No supervisor socket provided, extension management disabled");
            None
        };

    // 3. Bootstrap the libp2p mesh and connect to the orchestrator.
    let mesh_config = MeshConfig {
        listen: args.mesh_listen.clone(),
        discovery: MeshDiscovery::None, // worker doesn't need mDNS; it has the orchestrator address
        bootstrap_peers: vec![args.mesh_peer.clone()],
        ..MeshConfig::default()
    };

    tracing::info!(
        mesh_peer = %args.mesh_peer,
        mesh_listen = ?args.mesh_listen,
        "Bootstrapping mesh"
    );

    let mesh = bootstrap_mesh(&mesh_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bootstrap mesh: {}", e))?;

    tracing::info!(
        peer_id = %mesh.peer_id(),
        "Mesh bootstrapped"
    );

    // 4. Build a minimal AgentConfig from the shared database.
    let agent_config = config::build_worker_config(&args.db_path).await?;

    // Wire the mesh handle into the SessionProvider so that when the session's
    // `provider_node_id` names a remote node (e.g. the orchestrator or a
    // gpu-box peer), `build_provider_from_config` can construct a
    // `MeshChatProvider` instead of erroring with "no mesh handle available".
    //
    // Without this, the orchestrator's `handle_set_session_model` sees
    // `session_ref.is_remote() == true` (sandbox workers are Remote variants)
    // and tags the session with `provider_node_id = orchestrator_peer_id`,
    // but the worker's `SessionProvider` has no mesh handle to honour it.
    agent_config.provider.set_mesh(Some(mesh.clone()));

    // 5. Create SessionActor and register in the DHT.
    let mut runtime = SessionRuntime::new(
        Some(args.cwd.clone()),
        HashMap::new(),
        HashMap::new(),
        Vec::new(),
    );

    // Attach the extension manager to the runtime so the SessionActor can
    // request/release write access when the mode changes.
    //
    // At this point `runtime` is the sole Arc reference (the actor hasn't been
    // spawned yet), so `Arc::get_mut` is guaranteed to succeed.
    if let Some(ref mgr) = extension_manager {
        if let Some(rt) = Arc::get_mut(&mut runtime) {
            rt.extension_manager =
                Some(mgr.clone() as Arc<dyn querymt_sandbox::WriteAccessManager>);
        } else {
            tracing::warn!(
                "Could not attach extension manager: multiple Arc references to SessionRuntime"
            );
        }
    }

    let actor = SessionActor::new(agent_config, args.session_id.clone(), runtime)
        .with_mesh(Some(mesh.clone()));
    let actor_ref = SessionActor::spawn(actor);

    let dht_name = dht_name::session(&args.session_id);
    mesh.register_actor(actor_ref.clone(), dht_name.clone())
        .await;

    tracing::info!(
        session_id = %args.session_id,
        dht_name = %dht_name,
        "SessionActor registered in DHT"
    );

    // 6. Signal readiness — the orchestrator can detect this via stdout
    // as a belt-and-suspenders alongside the DHT lookup.
    println!("ready");
    tracing::info!(
        session_id = %args.session_id,
        "Worker ready"
    );

    // 7. Run until killed by orchestrator
    tokio::signal::ctrl_c().await?;
    tracing::info!("Worker shutting down");

    Ok(())
}
