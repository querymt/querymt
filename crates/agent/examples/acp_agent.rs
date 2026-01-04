//! Full ACP server example using QueryMTAgent.
//!
//! This example demonstrates the complete integration of QueryMTAgent with the ACP protocol
//! supporting both stdio and WebSocket transports:
//! - Full bidirectional communication (client→agent and agent→client)
//! - Session management and tool execution
//! - Permission requests for mutating operations
//! - Session update notifications
//!
//! ## Usage
//!
//! Run the server with stdio (default):
//! ```bash
//! cargo run --example acp_agent
//! # or explicitly:
//! cargo run --example acp_agent stdio
//! ```
//!
//! Run the server with WebSocket:
//! ```bash
//! cargo run --example acp_agent ws://127.0.0.1:3030
//! ```
//!
//! ## Testing with Manual Input (stdio)
//!
//! ```bash
//! cat <<'EOF' | ANTHROPIC_API_KEY=sk-... cargo run --example acp_agent 2>&1 | grep '^{'
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1","clientInfo":{"name":"test","version":"0.1"}}}
//! {"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}
//! {"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"SESSION_ID","prompt":[{"type":"text","text":"What files are in the current directory?"}]}}
//! EOF
//! ```
//!
//! ## Testing with WebSocket Client
//!
//! ```bash
//! # Terminal 1: Start WebSocket server
//! cargo run --example acp_agent ws://127.0.0.1:3030
//!
//! # Terminal 2: Connect with wscat
//! wscat -c ws://127.0.0.1:3030/ws
//! > {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1","clientInfo":{"name":"test","version":"0.1"}}}
//! ```
//!
//! ## Testing with Python SDK
//!
//! See `test_acp_bidirectional.py` for a full integration test using the Python ACP SDK.
//!
//! ## Features Demonstrated
//!
//! 1. ✅ Simple ergonomic API: `agent.acp("stdio").await` or `agent.acp("ws://...").await`
//! 2. ✅ Full QueryMTAgent capabilities (LLM calls, tool execution, session management)
//! 3. ✅ Bidirectional bridge for Send/!Send boundary crossing
//! 4. ✅ Permission requests for mutating tools
//! 5. ✅ Session update notifications during execution
//! 6. ✅ Graceful shutdown on SIGTERM/SIGINT
//! 7. ✅ Both stdio and WebSocket transports

use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Get transport from command line args (defaults to stdio)
    let transport = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "stdio".to_string());

    // Configure logging to stderr (for stdio) or stdout (for WebSocket)
    let log_target = if transport == "stdio" {
        env_logger::Target::Stderr
    } else {
        env_logger::Target::Stdout
    };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(log_target)
        .init();

    log::info!("================================================");
    log::info!("  QueryMT Agent - ACP Server");
    log::info!("================================================");
    log::info!("Protocol: Agent Client Protocol v1");
    log::info!("Transport: {}", transport);
    log::info!("Agent: QueryMTAgent with full capabilities");
    log::info!("Features:");
    log::info!("  - LLM provider: Anthropic Claude");
    log::info!("  - Built-in tools: read_file, write_file, shell");
    log::info!("  - Session management");
    log::info!("  - Permission requests");
    log::info!("  - Session notifications");
    log::info!("================================================");

    // Build the agent with a simple API
    let agent = Agent::single()
        .provider("anthropic", "claude-sonnet-4-20250514")
        .cwd("/tmp")
        .tools(["read_file", "write_file", "shell", "list", "glob"])
        .build()
        .await?;

    log::info!("Agent built successfully, starting ACP server...");

    // Start the ACP server with the specified transport
    // This blocks until stdin closes, connection drops, or SIGTERM/SIGINT
    agent.acp(transport).await?;

    log::info!("Server shutdown complete");

    // Force exit after graceful shutdown.
    // The agent has properly cleaned up all its tasks (event bus observers,
    // active sessions, I/O tasks, bridge tasks), but there may be background
    // threads from dependencies (e.g., Extism plugin runtime, Tokio internal
    // threads) that prevent natural termination. Since all user-facing cleanup
    // is complete, explicit exit is appropriate for a CLI server.
    std::process::exit(0);
}
