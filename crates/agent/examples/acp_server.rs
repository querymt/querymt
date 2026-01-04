use agent_client_protocol::{AgentSideConnection, Client};
use querymt::builder::LLMBuilder;
use querymt::plugin::{
    extism_impl::host::ExtismLoader, host::PluginRegistry, host::native::NativeLoader,
};
use querymt_agent::{
    agent::{QueryMTAgent, SnapshotPolicy, ToolPolicy},
    session::sqlite::SqliteSessionStore,
};
use std::sync::Arc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = match PluginRegistry::from_default_path() {
        Ok(registry) => registry,
        Err(err) => {
            eprintln!(
                "Failed to load providers config from default path: {}",
                err
            );
            return Ok(());
        }
    };
    registry.register_loader(Box::new(ExtismLoader));
    registry.register_loader(Box::new(NativeLoader));
    registry.load_all_plugins().await;

    // 1. Setup LLM
    let provider = LLMBuilder::new()
        .provider("ollama".to_string())
        .model("hf.co/unsloth/Seed-OSS-36B-Instruct-GGUF:Q4_K_M")
        .build(&registry)?;

    // Convert to Arc<dyn LLMProvider>
    let provider_arc = Arc::from(provider);

    // 2. Setup Session Store
    // Using an in-memory SQLite database for this example.
    let store = SqliteSessionStore::connect(std::path::PathBuf::from(":memory:")).await?;
    let store_arc = Arc::new(store);

    // 3. Create Agent
    let agent = QueryMTAgent::new(provider_arc, store_arc)
        .with_snapshot_policy(SnapshotPolicy::Metadata)
        .with_snapshot_root(".")
        .with_tool_policy(ToolPolicy::BuiltInAndProvider)
        .with_max_prompt_bytes(64_000)
        .with_mutating_tools(["apply_patch", "write_file", "delete_file"])
        .with_assume_mutating(false);

    // 4. Run ACP Loop
    // This sets up the Stdin/Stdout transport for the Agent Client Protocol
    let outgoing = tokio::io::stdout().compat_write();
    let incoming = tokio::io::stdin().compat();

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            // Channel for session notifications (optional diagnostics, etc.)
            let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
                agent_client_protocol::SessionNotification,
                tokio::sync::oneshot::Sender<()>,
            )>();

            // The AgentSideConnection handles the JSON-RPC communication
            let (conn, handle_io) = AgentSideConnection::new(agent, outgoing, incoming, |fut| {
                tokio::task::spawn_local(fut);
            });

            // Handle notifications from the connection
            tokio::task::spawn_local(async move {
                while let Some((notification, ack_tx)) = rx.recv().await {
                    if let Err(e) = conn.session_notification(notification).await {
                        eprintln!("Notification error: {}", e);
                        break;
                    }
                    let _ = ack_tx.send(());
                }
            });

            // Run the IO handler until the connection closes
            handle_io
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
        })
        .await?;

    Ok(())
}
