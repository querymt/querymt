use agent_client_protocol::{AgentSideConnection, Client};
use querymt::builder::LLMBuilder;
use querymt::mcp::{adapter::McpToolAdapter, config::Config};
use querymt::plugin::{
    extism_impl::host::ExtismLoader, host::PluginRegistry, host::native::NativeLoader,
};
use querymt_agent::{
    agent::{QueryMTAgent, SnapshotPolicy, ToolPolicy},
    session::sqlite::SqliteSessionStore,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

fn mcp_config_path() -> PathBuf {
    std::env::var("QMT_MCP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("mcp_cfg.toml"))
}

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

    // Load MCP config and add MCP tools to the LLM builder.
    let mut builder = LLMBuilder::new()
        .provider("openai".to_string())
        .api_key(std::env::var("OPENAI_API_KEY").unwrap_or("sk-OPENAI".into()))
        .model("gpt-4o-mini")
        .tool_choice(querymt::chat::ToolChoice::Any);

    let config = Config::load(mcp_config_path()).await?;
    let mcp_clients = config.create_mcp_clients().await?;
    for (name, client) in mcp_clients {
        let server = client.peer().clone();
        let tools = server.list_all_tools().await?;
        for tool in tools
            .into_iter()
            .map(|tool| McpToolAdapter::try_new(tool, server.clone(), name.clone()))
        {
            if let Ok(adapter) = tool {
                builder = builder.add_tool(adapter);
            }
        }
    }

    let provider = builder.build(&registry)?;
    let provider_arc = Arc::from(provider);

    let store = SqliteSessionStore::connect(std::path::PathBuf::from(":memory:")).await?;
    let store_arc = Arc::new(store);

    let agent = QueryMTAgent::new(provider_arc, store_arc)
        .with_tool_policy(ToolPolicy::BuiltInAndProvider)
        .with_snapshot_policy(SnapshotPolicy::None)
        .with_max_prompt_bytes(64_000);

    let outgoing = tokio::io::stdout().compat_write();
    let incoming = tokio::io::stdin().compat();

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
                agent_client_protocol::SessionNotification,
                tokio::sync::oneshot::Sender<()>,
            )>();

            let (conn, handle_io) = AgentSideConnection::new(agent, outgoing, incoming, |fut| {
                tokio::task::spawn_local(fut);
            });

            tokio::task::spawn_local(async move {
                while let Some((notification, ack_tx)) = rx.recv().await {
                    if let Err(e) = conn.session_notification(notification).await {
                        eprintln!("Notification error: {}", e);
                        break;
                    }
                    let _ = ack_tx.send(());
                }
            });

            handle_io
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
        })
        .await?;

    Ok(())
}
