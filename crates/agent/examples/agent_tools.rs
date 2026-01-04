use querymt::builder::LLMBuilder;
use querymt::plugin::{
    extism_impl::host::ExtismLoader, host::PluginRegistry, host::native::NativeLoader,
};
use querymt_agent::{
    agent::{QueryMTAgent, ToolPolicy},
    session::sqlite::SqliteSessionStore,
};
use std::sync::Arc;

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

    let provider = LLMBuilder::new()
        .provider("ollama".to_string())
        .model("hf.co/unsloth/Seed-OSS-36B-Instruct-GGUF:Q4_K_M")
        .build(&registry)?;
    let provider_arc = Arc::from(provider);

    let store = SqliteSessionStore::connect(std::path::PathBuf::from(":memory:")).await?;
    let store_arc = Arc::new(store);

    let agent = QueryMTAgent::new(provider_arc, store_arc)
        .with_tool_policy(ToolPolicy::BuiltInOnly)
        .with_max_prompt_bytes(32_000)
        .with_allowed_tools(["search_text"]);

    // Dynamic tool changes between cycles (e.g., UI toggles or policy updates).
    agent.set_tool_policy(ToolPolicy::BuiltInAndProvider);
    agent.set_denied_tools(["shell", "mcp.exec"]);

    println!("Agent configured with dynamic tool policy.");
    Ok(())
}
