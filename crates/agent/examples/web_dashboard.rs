#![cfg(feature = "dashboard")]

//! Web Dashboard Example
//!
//! Demonstrates the web-based UI for interacting with an agent.
//! Requires the `dashboard` feature to be enabled.
//!
//! ## Usage
//!
//! ```bash
//! cargo run --example web_dashboard --features dashboard
//! ```
//!
//! Then open http://127.0.0.1:3030 in your browser.

use querymt_agent::prelude::*;
#[cfg(feature = "dashboard")]
use querymt_agent::server::ServerMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Agent::single().provider("openai", "gpt-4").build().await?;

    let server = agent.server();
    let addr = "127.0.0.1:3030";

    println!("Dashboard running at http://{}", addr);
    server.run(addr, ServerMode::Dashboard).await?;
    Ok(())
}
